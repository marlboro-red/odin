//! `odin status`: an at-a-glance view of recent runs from the local store — the terminal
//! counterpart to the web dashboard. `--watch` live-refreshes; `--json` emits the same
//! [`RunView`] shape as the daemon's `/api/runs`.

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context as _;
use odin_core::{RunView, SqliteStore, Store};

pub(crate) struct StatusArgs {
    pub repo: Option<PathBuf>,
    pub db: Option<PathBuf>,
    pub limit: usize,
    pub watch: bool,
    pub json: bool,
}

pub(crate) fn run(args: StatusArgs) -> anyhow::Result<ExitCode> {
    let path = args.db.unwrap_or_else(|| {
        args.repo
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".odin")
            .join("state.db")
    });
    if !path.exists() {
        if args.json {
            println!("[]");
        } else {
            eprintln!("no run state database at {}", path.display());
        }
        return Ok(ExitCode::SUCCESS);
    }
    let store = SqliteStore::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;

    if args.json {
        let views = runtime.block_on(load(&store, args.limit))?;
        println!("{}", serde_json::to_string_pretty(&views)?);
    } else if args.watch {
        runtime.block_on(watch(&store, args.limit))?;
    } else {
        render(&runtime.block_on(load(&store, args.limit))?);
    }
    Ok(ExitCode::SUCCESS)
}

async fn load(store: &SqliteStore, limit: usize) -> anyhow::Result<Vec<RunView>> {
    Ok(store
        .recent(limit)
        .await?
        .iter()
        .map(RunView::project)
        .collect())
}

/// Clears the screen and re-renders every 2s until interrupted (ctrl-c).
async fn watch(store: &SqliteStore, limit: usize) -> anyhow::Result<()> {
    loop {
        let views = load(store, limit).await?;
        if std::io::stdout().is_terminal() {
            print!("\x1b[2J\x1b[H"); // clear screen + cursor home (only on a real terminal)
        }
        render(&views);
        println!("\n(watching — ctrl-c to exit)");
        std::io::stdout().flush().ok();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

const STATUS_ORDER: [&str; 6] = [
    "running",
    "awaiting_approval",
    "pending",
    "succeeded",
    "failed",
    "cancelled",
];

fn render(views: &[RunView]) {
    if views.is_empty() {
        println!("no runs yet");
        return;
    }
    // Summary header: counts of the displayed runs, by status.
    let summary: Vec<String> = STATUS_ORDER
        .iter()
        .filter_map(|s| {
            let n = views.iter().filter(|v| v.status == *s).count();
            (n > 0).then(|| format!("{n} {}", s.replace('_', " ")))
        })
        .collect();
    println!("{}\n", summary.join("  ·  "));

    for v in views {
        let passed = v.steps.iter().filter(|s| s.status == "passed").count();
        let short: String = v.run_id.chars().take(8).collect();
        let mut line = format!(
            "{} {:<9} {:<8} {:<20} {:>2}/{:<2} {:>4}",
            glyph(&v.status),
            row_label(&v.status),
            short,
            truncate(&v.workflow, 20),
            passed,
            v.steps.len(),
            ago(&v.updated_at),
        );
        if let Some(msg) = v.gate.as_ref().and_then(|g| g.message.as_deref()) {
            line.push_str("  ↳ ");
            line.push_str(msg);
        }
        println!("{line}");
    }
}

/// A compact (≤9 char) status word for the aligned status column. `awaiting_approval` is
/// shortened to `awaiting` (the ⏸ glyph already conveys it) so the columns line up.
fn row_label(status: &str) -> &'static str {
    match status {
        "awaiting_approval" => "awaiting",
        "running" => "running",
        "pending" => "pending",
        "failed" => "failed",
        "cancelled" => "cancelled",
        "succeeded" => "succeeded",
        _ => "unknown",
    }
}

fn glyph(status: &str) -> &'static str {
    match status {
        "succeeded" => "✓",
        "failed" | "cancelled" => "✗",
        "running" => "▸",
        "awaiting_approval" => "⏸",
        _ => "·",
    }
}

/// A compact "12s" / "3m" / "5h" / "2d" age from an RFC 3339 timestamp.
fn ago(rfc3339: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return "?".to_owned();
    };
    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}
