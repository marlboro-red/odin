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
    /// Read from a remote `odind`'s `GET /api/runs` instead of the local store — the same
    /// `RunView` shape, so the render is identical. Mutually exclusive with `--db`/`--repo`.
    pub url: Option<String>,
}

pub(crate) fn run(args: StatusArgs) -> anyhow::Result<ExitCode> {
    // Remote mode: poll a daemon's HTTP API rather than opening a local SQLite store.
    if let Some(url) = &args.url {
        let agent = remote_agent();
        if args.watch && !args.json {
            return watch_remote(&agent, url, args.limit);
        }
        let views = fetch_remote(&agent, url, args.limit)?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&views)?);
        } else {
            render(&views);
        }
        return Ok(ExitCode::SUCCESS);
    }
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

/// The HTTP client for `--url`: a per-request timeout so a stalled daemon can't hang `odin status`
/// (or freeze `--watch`) forever — ureq's default has NO read timeout — and no redirect following
/// (the daemon's `/api/runs` never redirects; refusing one closes a bounce-to-elsewhere vector).
/// Shared across `--watch` polls so the connection is kept alive.
fn remote_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .redirects(0)
        .build()
}

/// Fetches `GET <url>/api/runs?limit=N` from a remote daemon and parses the `RunView` list — the
/// same shape the local store projects, so the caller renders it identically.
fn fetch_remote(agent: &ureq::Agent, url: &str, limit: usize) -> anyhow::Result<Vec<RunView>> {
    let base = url.trim_end_matches('/');
    let endpoint = format!("{base}/api/runs?limit={limit}");
    let body = agent
        .get(&endpoint)
        .call()
        .map_err(|e| match e {
            // A 4xx/5xx carries the daemon's own message; surface it (e.g. "dashboard not enabled").
            ureq::Error::Status(code, resp) => {
                let msg = resp.into_string().unwrap_or_default();
                anyhow::anyhow!("{endpoint} returned HTTP {code}: {}", msg.trim())
            }
            // A connection-level failure (refused, DNS, TLS) — the daemon isn't reachable.
            ureq::Error::Transport(t) => anyhow::anyhow!("requesting {endpoint}: {t}"),
        })?
        .into_string()
        .with_context(|| format!("reading the response from {endpoint}"))?;
    serde_json::from_str(&body).with_context(|| format!("parsing the run list from {endpoint}"))
}

/// Re-fetches the remote daemon every 2s until interrupted (ctrl-c).
fn watch_remote(agent: &ureq::Agent, url: &str, limit: usize) -> anyhow::Result<ExitCode> {
    loop {
        let result = fetch_remote(agent, url, limit);
        // Clear the screen on every tick (success OR failure) so a down daemon doesn't leave a
        // stale table sitting above the error.
        if std::io::stdout().is_terminal() {
            print!("\x1b[2J\x1b[H");
        }
        match result {
            Ok(views) => render(&views),
            // Don't abort the watch on a transient blip (daemon restart); show it and retry.
            Err(e) => println!("status unavailable: {e}"),
        }
        println!("\n(watching {url} — ctrl-c to exit)");
        std::io::stdout().flush().ok();
        std::thread::sleep(Duration::from_secs(2));
    }
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

#[cfg(test)]
mod tests {
    use super::{fetch_remote, remote_agent};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    /// Serve `body` once over HTTP on an ephemeral port; returns the base URL.
    fn serve_once(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf); // drain the request line/headers
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn fetch_remote_parses_the_runview_list() {
        let body = r#"[{"run_id":"abc12345","workflow":"wf","status":"succeeded","created_at":"2026-06-10T00:00:00+00:00","updated_at":"2026-06-10T00:00:02+00:00","duration_ms":1200,"steps":[],"gate":null}]"#;
        let url = serve_once("200 OK", body);
        let views = fetch_remote(&remote_agent(), &url, 10).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].status, "succeeded");
        assert_eq!(views[0].duration_ms, Some(1200));
    }

    #[test]
    fn fetch_remote_surfaces_an_http_error() {
        let url = serve_once("404 Not Found", "dashboard not enabled");
        let err = fetch_remote(&remote_agent(), &url, 10)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("404"),
            "expected the HTTP status in the error: {err}"
        );
        assert!(
            err.contains("dashboard not enabled"),
            "expected the body: {err}"
        );
    }
}
