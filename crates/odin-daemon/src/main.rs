//! `odind` — the Odin daemon.
//!
//! A thin runner over [`odin_daemon`]: load a directory of workflow files, build an engine
//! backed by a SQLite store, derive triggers from each workflow's `triggers:` block (cron
//! schedules and a GitHub webhook server), and serve until `ctrl-c`. Durable in-flight runs
//! resume on the next start.
//!
//! ```text
//! odind --workflows ./workflows --repo . [--db ./.odin/state.db] \
//!       [--webhook-addr 127.0.0.1:9292] [--webhook-secret <SECRET>] \
//!       [--log-format text|json] [--otlp-endpoint http://localhost:4317]
//! ```
//!
//! Logging is structured via `tracing`; control the level with `$ODIN_LOG` (then `$RUST_LOG`),
//! defaulting to `info`. `--otlp-endpoint` exports spans to an OpenTelemetry collector when
//! built with `--features otlp`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use odin_core::ir::{HumanDuration, StepKind, TriggerDecl};
use odin_core::telemetry::{self, Options};
use odin_core::{EngineBuilder, PrunePolicy, SqliteStore, Store, Workflow};
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
    /// Address the HTTP server binds to (started if a workflow declares a `github_webhook`
    /// trigger or an `approval` gate — serving `/webhook` and/or `/approve`).
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:9292")]
    webhook_addr: SocketAddr,
    /// Maximum runs executing concurrently across the daemon (default 4). A burst of events
    /// queues for a free slot rather than launching unbounded runs.
    #[arg(long, value_name = "N")]
    max_concurrent_runs: Option<usize>,
    /// HMAC secret for verifying GitHub webhook signatures; falls back to
    /// `$ODIN_WEBHOOK_SECRET`. Required if any workflow declares a `github_webhook` trigger,
    /// unless `--webhook-allow-unsigned` is given.
    #[arg(long, value_name = "SECRET")]
    webhook_secret: Option<String>,
    /// Explicitly run the webhook server WITHOUT signature verification (local testing
    /// only). Without this, a declared webhook trigger and no secret is a startup error.
    #[arg(long)]
    webhook_allow_unsigned: bool,
    /// Serve the web status dashboard at `http://<webhook-addr>/` (and its read-only
    /// `/api/runs`). Approve/reject from the page sign in your browser with the webhook secret.
    #[arg(long)]
    dashboard: bool,
    /// Run a periodic retention sweep every DURATION (e.g. `24h`), deleting old/excess terminal
    /// runs. Off unless set; requires `--prune-older-than` and/or `--prune-keep-last`.
    #[arg(long, value_name = "DURATION")]
    prune_interval: Option<String>,
    /// Retention age for `--prune-interval`: prune terminal runs last updated longer ago than
    /// this (e.g. `90d`).
    #[arg(long, value_name = "DURATION")]
    prune_older_than: Option<String>,
    /// Retention count for `--prune-interval`: keep at most this many terminal runs per workflow.
    #[arg(long, value_name = "N")]
    prune_keep_last: Option<u32>,
    /// Log output format.
    #[arg(long, value_name = "FORMAT", default_value = "text", value_parser = ["text", "json"])]
    log_format: String,
    /// Export spans to an OpenTelemetry OTLP collector (e.g. `http://localhost:4317`). Honored
    /// only when built with `--features otlp`; otherwise ignored with a warning.
    #[arg(long, value_name = "URL")]
    otlp_endpoint: Option<String>,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Top-level fatal: telemetry may already be torn down, so write directly.
            eprintln!("odind: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Build the runtime first, then run everything inside it so the OTLP batch exporter has a
    // tokio context at telemetry init.
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(serve(cli))
}

#[allow(clippy::too_many_lines)]
async fn serve(cli: Cli) -> anyhow::Result<()> {
    // Install telemetry first so everything below is captured; hold the guard for the whole
    // process (dropping it flushes the OTLP exporter).
    let format = cli.log_format.parse().unwrap_or_default();
    let _telemetry = telemetry::init(&Options {
        format,
        otlp_endpoint: cli.otlp_endpoint.clone(),
    });
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "odind starting");

    let workflows = load_workflows(&cli.workflows)?;
    if workflows.is_empty() {
        anyhow::bail!("no valid workflows found in {}", cli.workflows.display());
    }
    tracing::info!(
        count = workflows.len(),
        dir = %cli.workflows.display(),
        "loaded workflows"
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
    let store: Arc<dyn Store> =
        Arc::new(SqliteStore::open(&db).context("opening the run state database")?);
    let engine = EngineBuilder::new()
        .repo(&cli.repo)
        .store(store.clone())
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

    // Expose `POST /approve` when any workflow has an approval gate, so paused runs can be
    // decided over HTTP (the daemon-side equivalent of `odin approve`/`reject`). The handler
    // resumes inline, so it needs a shared engine handle and the loaded workflow set.
    let has_approvals = workflows.iter().any(|w| {
        w.steps
            .iter()
            .any(|s| matches!(s.kind, StepKind::Approval(_)))
    });
    if has_approvals {
        webhook_server.enable_approvals(engine.clone(), Arc::from(workflows.clone()));
    }
    // The read-only `/metrics` + `/health` endpoints are always served (Prometheus scrapes them),
    // so the HTTP server runs even for a webhook-less, approval-less daemon.
    webhook_server.enable_metrics(store.clone());
    if cli.dashboard {
        webhook_server.enable_dashboard();
    }

    // Fail closed: a network-facing endpoint that MUTATES run state (`/webhook`, `/approve`)
    // without a verification secret would accept requests from anyone. Only an explicit opt-in
    // permits running those unsigned. (`/metrics` + `/health` are read-only and need no secret.)
    if webhook_server.serves_mutations() && !has_secret && !cli.webhook_allow_unsigned {
        anyhow::bail!(
            "a github_webhook trigger or an approval gate exposes a network endpoint, but no \
             secret is configured; set --webhook-secret or $ODIN_WEBHOOK_SECRET (or pass \
             --webhook-allow-unsigned for local testing without signature verification)"
        );
    }

    let mut daemon = Daemon::from_workflows(engine, workflows)?;
    if let Some(n) = cli.max_concurrent_runs {
        daemon = daemon.with_max_concurrent_runs(n);
    }
    if let Some(policy) = prune_policy(
        cli.prune_interval.as_deref(),
        cli.prune_older_than.as_deref(),
        cli.prune_keep_last,
    )? {
        let period = HumanDuration::parse(cli.prune_interval.as_deref().unwrap_or_default())
            .map_err(|e| anyhow::anyhow!("--prune-interval: {e}"))?
            .as_duration();
        daemon = daemon.with_prune(policy, period);
    }
    for trigger in webhook_triggers {
        daemon.add_trigger(Box::new(trigger));
    }
    let shutdown = daemon.cancellation_token();

    // On ctrl-c, ask the daemon + webhook server to stop accepting new events and drain
    // in-flight work (rather than dropping the futures mid-flight).
    let signal_token = shutdown.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("ctrl-c received, draining in-flight runs");
            signal_token.cancel();
        }
    });

    // The HTTP server always runs (it serves `/metrics` + `/health` unconditionally, and
    // `/webhook` / `/approve` when configured), alongside the supervisor loop.
    tracing::info!(
        subscriptions = webhook_server.subscription_count(),
        approvals = has_approvals,
        body_cap_mib = 25,
        "http server configured"
    );
    let bound = webhook_server.bind().await?;
    // No built-in TLS: a non-loopback bind over plain HTTP must sit behind a
    // TLS-terminating reverse proxy, or signatures travel in cleartext.
    if !bound.local_addr().ip().is_loopback() {
        tracing::warn!(
            addr = %bound.local_addr(),
            "http server bound to a non-loopback address over plain HTTP; terminate TLS at a \
             reverse proxy in front of it"
        );
    }
    tracing::info!(addr = %bound.local_addr(), "http server listening (/webhook, /approve, /metrics, /health)");
    // Drive the supervisor loop and the HTTP server together; both end on shutdown.
    let (daemon_res, server_res) = tokio::join!(daemon.run(), bound.serve(shutdown));
    let result = daemon_res.and(server_res);
    signal.abort();
    result
}

/// Builds the daemon's retention policy from the `--prune-*` flags, or `None` if `interval` is
/// unset (pruning disabled). Errors if an interval is set with no age or count limit — that would
/// be a no-op sweep loop, almost certainly a misconfiguration.
fn prune_policy(
    interval: Option<&str>,
    older_than: Option<&str>,
    keep_last: Option<u32>,
) -> anyhow::Result<Option<PrunePolicy>> {
    if interval.is_none() {
        return Ok(None);
    }
    let max_age = match older_than {
        Some(s) => {
            let std = HumanDuration::parse(s)
                .map_err(|e| anyhow::anyhow!("--prune-older-than: {e}"))?
                .as_duration();
            Some(
                chrono::Duration::from_std(std)
                    .map_err(|_| anyhow::anyhow!("--prune-older-than is too large"))?,
            )
        }
        None => None,
    };
    let policy = PrunePolicy {
        max_age,
        keep_last,
        workflow: None,
    };
    if policy.is_noop() {
        anyhow::bail!("--prune-interval requires --prune-older-than and/or --prune-keep-last");
    }
    Ok(Some(policy))
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
                tracing::warn!(error = %e, "skipping unreadable directory entry");
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
                tracing::warn!(path = %path.display(), error = %e, "skipping unreadable workflow");
                continue;
            }
        };
        match Workflow::from_yaml_str(&src) {
            Ok(workflow) => {
                tracing::debug!(workflow = %workflow.name.as_str(), path = %path.display(), "loaded workflow");
                workflows.push(workflow);
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping unparseable workflow");
            }
        }
    }
    Ok(workflows)
}

#[cfg(test)]
mod tests {
    use super::prune_policy;

    #[test]
    fn prune_policy_is_disabled_without_an_interval() {
        // Limits given but no interval ⇒ pruning off.
        assert!(prune_policy(None, Some("90d"), Some(10)).unwrap().is_none());
    }

    #[test]
    fn prune_policy_refuses_an_interval_with_no_limit() {
        // An interval with no age/count limit would be a no-op sweep loop.
        assert!(prune_policy(Some("24h"), None, None).is_err());
    }

    #[test]
    fn prune_policy_builds_from_age_and_count() {
        let p = prune_policy(Some("24h"), Some("90d"), Some(200))
            .unwrap()
            .unwrap();
        assert!(p.max_age.is_some());
        assert_eq!(p.keep_last, Some(200));
    }

    #[test]
    fn prune_policy_rejects_a_bad_age() {
        assert!(prune_policy(Some("24h"), Some("not-a-duration"), None).is_err());
    }
}
