//! `odind` — the Odin daemon.
//!
//! A thin runner over [`odin_daemon`]: load a directory of workflow files, build an engine
//! backed by a SQLite store, derive triggers from each workflow's `triggers:` block (cron
//! schedules and a GitHub webhook server), and serve until `ctrl-c`. Durable in-flight runs
//! resume on the next start.
//!
//! ```text
//! odind --workflows ./workflows --repo . [--db ./.odin/state.db] \
//!       [--webhook-addr 127.0.0.1:9292] [--webhook-secret <SECRET>]
//! ```

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use odin_core::ir::TriggerDecl;
use odin_core::{EngineBuilder, SqliteStore, Workflow};
use odin_daemon::{Daemon, WebhookServer};

/// Run Odin workflows from event triggers (cron schedules + GitHub webhooks).
#[derive(Parser)]
#[command(name = "odind", version, about)]
struct Cli {
    /// Directory of workflow files to serve; every `*.yaml` / `*.yml` is loaded.
    #[arg(long, value_name = "DIR")]
    workflows: PathBuf,
    /// Git repository the engine provisions workspaces from.
    #[arg(long, value_name = "DIR", default_value = ".")]
    repo: PathBuf,
    /// SQLite state database. Defaults to `<repo>/.odin/state.db`.
    #[arg(long, value_name = "FILE")]
    db: Option<PathBuf>,
    /// Address the webhook HTTP server binds to (only started if a workflow declares a
    /// `github_webhook` trigger).
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:9292")]
    webhook_addr: SocketAddr,
    /// HMAC secret for verifying GitHub webhook signatures; falls back to
    /// `$ODIN_WEBHOOK_SECRET`. Required if any workflow declares a `github_webhook` trigger,
    /// unless `--webhook-allow-unsigned` is given.
    #[arg(long, value_name = "SECRET")]
    webhook_secret: Option<String>,
    /// Explicitly run the webhook server WITHOUT signature verification (local testing
    /// only). Without this, a declared webhook trigger and no secret is a startup error.
    #[arg(long)]
    webhook_allow_unsigned: bool,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("odind: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let workflows = load_workflows(&cli.workflows)?;
    if workflows.is_empty() {
        anyhow::bail!("no valid workflows found in {}", cli.workflows.display());
    }
    eprintln!(
        "odind: loaded {} workflow(s) from {}",
        workflows.len(),
        cli.workflows.display()
    );

    let db = cli
        .db
        .clone()
        .unwrap_or_else(|| cli.repo.join(".odin").join("state.db"));
    if let Some(parent) = db.parent() {
        // Propagate (don't swallow): otherwise a failed mkdir surfaces later as a confusing
        // "opening the run state database" error that blames the wrong thing.
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating state directory {}", parent.display()))?;
    }
    let store = SqliteStore::open(&db).context("opening the run state database")?;
    let engine = EngineBuilder::new()
        .repo(&cli.repo)
        .store(Arc::new(store))
        .build()?;

    // Build the webhook server from every `github_webhook` decl (before the workflows are
    // moved into the daemon), collecting the pull-side triggers to register.
    let secret = cli
        .webhook_secret
        .or_else(|| std::env::var("ODIN_WEBHOOK_SECRET").ok())
        .filter(|s| !s.is_empty());
    let has_secret = secret.is_some();
    let mut webhook_server = WebhookServer::new(cli.webhook_addr, secret);
    let mut webhook_triggers = Vec::new();
    for workflow in &workflows {
        for decl in &workflow.triggers {
            if let TriggerDecl::GithubWebhook(github) = decl {
                webhook_triggers.push(webhook_server.subscribe(github, workflow.name.clone()));
            }
        }
    }

    // Fail closed: a network-facing webhook trigger without a verification secret would
    // accept requests from anyone. Only an explicit opt-in permits running unsigned.
    if !webhook_server.is_empty() && !has_secret && !cli.webhook_allow_unsigned {
        anyhow::bail!(
            "a github_webhook trigger is declared but no secret is configured; set \
             --webhook-secret or $ODIN_WEBHOOK_SECRET (or pass --webhook-allow-unsigned for \
             local testing without signature verification)"
        );
    }

    let mut daemon = Daemon::from_workflows(engine, workflows)?;
    for trigger in webhook_triggers {
        daemon.add_trigger(Box::new(trigger));
    }
    let shutdown = daemon.cancellation_token();

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(async move {
        // On ctrl-c, ask the daemon + webhook server to stop accepting new events and drain
        // in-flight work (rather than dropping the futures mid-flight).
        let signal_token = shutdown.clone();
        let signal = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("odind: ctrl-c received, draining in-flight runs…");
                signal_token.cancel();
            }
        });

        let result = if webhook_server.is_empty() {
            daemon.run().await
        } else {
            eprintln!(
                "odind: webhook server: {} subscription(s), 25 MiB body cap",
                webhook_server.subscription_count()
            );
            let bound = webhook_server.bind().await?;
            // No built-in TLS: a non-loopback bind over plain HTTP must sit behind a
            // TLS-terminating reverse proxy, or signatures travel in cleartext.
            if !bound.local_addr().ip().is_loopback() {
                eprintln!(
                    "odind: WARNING webhook server bound to non-loopback {} over plain HTTP; \
                     terminate TLS at a reverse proxy in front of it",
                    bound.local_addr()
                );
            }
            eprintln!(
                "odind: webhook server listening on http://{}/webhook",
                bound.local_addr()
            );
            // Drive the supervisor loop and the HTTP server together; both end on shutdown.
            let (daemon_res, server_res) = tokio::join!(daemon.run(), bound.serve(shutdown));
            daemon_res.and(server_res)
        };
        signal.abort();
        result
    })
}

/// Loads every `*.yaml` / `*.yml` workflow in `dir` (sorted for determinism). A single
/// unreadable directory entry, unreadable file, or unparseable file is skipped with a
/// warning rather than aborting startup — one bad file must not take the whole daemon down.
/// Only failing to read the directory itself is fatal.
fn load_workflows(dir: &Path) -> anyhow::Result<Vec<Workflow>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading workflow dir {}", dir.display()))?;
    let mut paths = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry.path()),
            Err(e) => {
                eprintln!("odind: skipping unreadable directory entry: {e}");
                None
            }
        })
        .filter(|p| matches!(p.extension().and_then(|s| s.to_str()), Some("yaml" | "yml")))
        .collect::<Vec<_>>();
    paths.sort();

    let mut workflows = Vec::new();
    for path in paths {
        let src = match std::fs::read_to_string(&path) {
            Ok(src) => src,
            Err(e) => {
                eprintln!("odind: skipping {}: {e}", path.display());
                continue;
            }
        };
        match Workflow::from_yaml_str(&src) {
            Ok(workflow) => {
                eprintln!("odind:   • {} ({})", workflow.name.as_str(), path.display());
                workflows.push(workflow);
            }
            Err(e) => eprintln!("odind: skipping {}: {e}", path.display()),
        }
    }
    Ok(workflows)
}
