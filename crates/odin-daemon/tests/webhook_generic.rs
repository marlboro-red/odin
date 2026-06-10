//! End-to-end test for the GENERIC `webhook` trigger (not GitHub): a real HTTP POST with
//! `X-Odin-Event` → event match → param extraction from the JSON body → daemon dispatch →
//! recorded run, over a temp git repo and a shell-only workflow.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use odin_core::ir::{TriggerDecl, WebhookDecl};
use odin_core::{EngineBuilder, RunStatus, SqliteStore, Store, Workflow};
use odin_daemon::{Daemon, WebhookServer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "t@odin.invalid"]);
    git(dir, &["config", "user.name", "Odin Test"]);
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "init"]);
}

/// A durable, shell-only workflow with a required `r` param mapped from the JSON body by a generic
/// `webhook` trigger matching the `deploy` event.
fn webhook_workflow() -> Workflow {
    let src = "\
name: deployer
workspace: { type: worktree }
durable: true
triggers:
  - type: webhook
    event: deploy
    params:
      r: deployment.ref
params:
  r: { required: true }
steps:
  - { id: log, run: \"echo deploying-{{ params.r }}\" }
";
    Workflow::from_yaml_str(src).unwrap()
}

fn webhook_decl(workflow: &Workflow) -> &WebhookDecl {
    workflow
        .triggers
        .iter()
        .find_map(|t| match t {
            TriggerDecl::Webhook(d) => Some(d),
            _ => None,
        })
        .expect("workflow has a webhook trigger")
}

async fn post_generic(addr: SocketAddr, event: &str, delivery: &str, body: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST /webhook HTTP/1.1\r\nHost: localhost\r\nX-Odin-Event: {event}\r\n\
         X-Odin-Delivery: {delivery}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.flush().await.unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await.unwrap();
    resp // full HTTP response (status line + headers + body)
}

/// POSTs `/webhook` with NO event header (the handler 400s) — returns the full response.
async fn post_no_event(addr: SocketAddr) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = "POST /webhook HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
               Content-Length: 2\r\nConnection: close\r\n\r\n{}";
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await.unwrap();
    resp
}

async fn wait_terminal(store: &Arc<SqliteStore>, timeout: Duration) -> (usize, Option<RunStatus>) {
    let deadline = Instant::now() + timeout;
    loop {
        let runs = store.recent(10).await.unwrap();
        if let Some(first) = runs.first() {
            if first.status.is_terminal() {
                return (runs.len(), Some(first.status));
            }
        }
        if Instant::now() >= deadline {
            return (runs.len(), runs.first().map(|r| r.status));
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generic_webhook_extracts_a_param_and_dispatches_a_run() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let store = Arc::new(SqliteStore::open(repo.path().join("state.db")).unwrap());
    let engine = EngineBuilder::new()
        .repo(repo.path())
        .store(store.clone())
        .build()
        .unwrap();

    let workflow = webhook_workflow();
    // Dev mode (no secret): unsigned generic deliveries are accepted.
    let mut server = WebhookServer::new("127.0.0.1:0".parse().unwrap(), None);
    let trigger = server.subscribe_webhook(webhook_decl(&workflow), workflow.name.clone());

    let mut daemon = Daemon::new(engine, [workflow]);
    daemon.add_trigger(Box::new(trigger));
    let shutdown = daemon.cancellation_token();
    let bound = server.bind().await.unwrap();
    let addr = bound.local_addr();
    let server_task = tokio::spawn(bound.serve(shutdown.clone()));
    let daemon_task = tokio::spawn(daemon.run());

    let body = serde_json::to_vec(&serde_json::json!({
        "deployment": { "ref": "v9.9.9" }
    }))
    .unwrap();
    let resp = post_generic(addr, "deploy", "del-1", &body).await;
    assert!(resp.contains("202"), "expected 202 Accepted, got: {resp}");
    // Every response carries the API-version header.
    assert!(
        resp.to_lowercase().contains("x-odin-api-version: 1"),
        "missing version header: {resp}"
    );
    // The 202 body is JSON listing the matched workflow(s).
    assert!(
        resp.contains("\"matched\":[\"deployer\"]"),
        "202 should list matched workflows: {resp}"
    );

    let (count, terminal) = wait_terminal(&store, Duration::from_secs(10)).await;
    assert_eq!(count, 1, "exactly one run should have been dispatched");
    assert_eq!(terminal, Some(RunStatus::Succeeded));
    // The `ref` was extracted from the body into the required param.
    let run = store.recent(1).await.unwrap().pop().unwrap();
    assert_eq!(
        run.input.params.get("r").and_then(|v| v.as_str()),
        Some("v9.9.9"),
        "the deployment.ref should have been mapped into params.r"
    );

    // A non-matching event fires nothing.
    let resp = post_generic(addr, "other-event", "del-2", &body).await;
    assert!(resp.contains("202"), "got: {resp}");

    // An invalid request (no event header) → a JSON error body with a stable `code`.
    let err = post_no_event(addr).await;
    assert!(err.contains("400"), "expected 400, got: {err}");
    assert!(
        err.contains("\"code\":\"missing_event_header\""),
        "error body should be JSON with a code: {err}"
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), server_task).await;
}
