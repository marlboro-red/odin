//! End-to-end tests for the GitHub webhook path: a real HTTP POST → signature check →
//! event match → param extraction → daemon dispatch → recorded run, over a temp git repo
//! and a shell-only workflow (no provider/API cost).

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use odin_core::ir::{GithubWebhookDecl, TriggerDecl};
use odin_core::{EngineBuilder, RunStatus, SqliteStore, Store, Workflow};
use odin_daemon::{Daemon, WebhookServer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed"
    );
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "t@odin.invalid"]);
    git(dir, &["config", "user.name", "Odin Test"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "init"]);
}

/// A durable, shell-only workflow with a *required* `who` param, a `github_webhook` trigger
/// that maps `who` from the payload, and a repo filter.
fn webhook_workflow() -> Workflow {
    let src = "\
name: wh-flow
workspace: { type: worktree }
durable: true
triggers:
  - type: github_webhook
    events: [\"issues.labeled\"]
    repo: marlboro-red/odin
    params:
      who: issue.user.login
params:
  who: { required: true }
steps:
  - { id: greet, run: \"echo hello-{{ params.who }}\" }
";
    Workflow::from_yaml_str(src).unwrap()
}

fn webhook_decl(workflow: &Workflow) -> &GithubWebhookDecl {
    workflow
        .triggers
        .iter()
        .find_map(|t| match t {
            TriggerDecl::GithubWebhook(d) => Some(d),
            _ => None,
        })
        .expect("workflow has a github_webhook trigger")
}

fn sign(secret: &str, body: &[u8]) -> String {
    use hmac::Mac as _;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// POSTs a raw HTTP/1.1 webhook request and returns the response status line.
async fn post_webhook(
    addr: SocketAddr,
    event: &str,
    body: &[u8],
    signature: Option<&str>,
) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let sig_header =
        signature.map_or_else(String::new, |s| format!("X-Hub-Signature-256: {s}\r\n"));
    let req = format!(
        "POST /webhook HTTP/1.1\r\nHost: localhost\r\nX-GitHub-Event: {event}\r\n\
         X-GitHub-Delivery: test-delivery\r\nContent-Type: application/json\r\n\
         {sig_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.flush().await.unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await.unwrap();
    resp.lines().next().unwrap_or_default().to_owned()
}

/// Polls the store until the most recent run reaches a terminal status (or the deadline
/// passes), returning `(count, terminal_status)` — so callers wait for the run to *finish*,
/// not merely to start.
async fn wait_for_terminal_run(
    store: &Arc<SqliteStore>,
    timeout: Duration,
) -> (usize, Option<RunStatus>) {
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

struct Harness {
    addr: SocketAddr,
    store: Arc<SqliteStore>,
    shutdown: CancellationToken,
    daemon_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    server_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    _repo: tempfile::TempDir,
}

impl Harness {
    /// Boots an engine + a webhook server (bound to an ephemeral port) + a daemon driving
    /// the webhook workflow, all sharing one shutdown token.
    async fn start(secret: Option<&str>) -> Self {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let store = Arc::new(SqliteStore::open(repo.path().join("state.db")).unwrap());
        let engine = EngineBuilder::new()
            .repo(repo.path())
            .store(store.clone())
            .build()
            .unwrap();

        let workflow = webhook_workflow();
        let mut server =
            WebhookServer::new("127.0.0.1:0".parse().unwrap(), secret.map(str::to_owned));
        let trigger = server.subscribe(webhook_decl(&workflow), workflow.name.clone());

        let mut daemon = Daemon::new(engine, [workflow]);
        daemon.add_trigger(Box::new(trigger));
        let shutdown = daemon.cancellation_token();

        let bound = server.bind().await.unwrap();
        let addr = bound.local_addr();
        let server_task = tokio::spawn(bound.serve(shutdown.clone()));
        let daemon_task = tokio::spawn(daemon.run());

        Self {
            addr,
            store,
            shutdown,
            daemon_task,
            server_task,
            _repo: repo,
        }
    }

    async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.daemon_task).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.server_task).await;
    }
}

const SECRET: &str = "test-secret";

/// A minimal `issues.labeled` payload for repo `marlboro-red/odin`, opener `octocat`.
fn labeled_payload() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "action": "labeled",
        "repository": { "full_name": "marlboro-red/odin" },
        "issue": { "html_url": "https://github.com/marlboro-red/odin/issues/1", "user": { "login": "octocat" } }
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_webhook_triggers_a_run_with_mapped_params() {
    let h = Harness::start(Some(SECRET)).await;
    let body = labeled_payload();

    let status = post_webhook(h.addr, "issues", &body, Some(&sign(SECRET, &body))).await;
    assert!(
        status.contains("202"),
        "expected 202 Accepted, got: {status}"
    );

    let (n, terminal) = wait_for_terminal_run(&h.store, Duration::from_secs(15)).await;
    assert_eq!(
        n, 1,
        "the signed, matching webhook should produce exactly one run"
    );
    // SUCCEEDED proves the required `who` param was extracted from the payload
    // (issue.user.login) — without it, param validation would have failed the run.
    assert_eq!(terminal, Some(RunStatus::Succeeded));

    let runs = h.store.recent(10).await.unwrap();
    assert_eq!(runs[0].workflow.as_str(), "wh-flow");

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhook_with_a_bad_signature_is_rejected_and_no_run_happens() {
    let h = Harness::start(Some(SECRET)).await;
    let body = labeled_payload();

    let status = post_webhook(h.addr, "issues", &body, Some(&sign("wrong-secret", &body))).await;
    assert!(
        status.contains("401"),
        "expected 401 Unauthorized, got: {status}"
    );

    // Give any (erroneous) dispatch a chance to land, then assert nothing ran.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        h.store.recent(10).await.unwrap().len(),
        0,
        "a bad signature must not run anything"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhook_for_an_unmatched_event_does_not_run() {
    let h = Harness::start(Some(SECRET)).await;
    // Right repo + signature, but action "closed" doesn't match the "issues.labeled" sub.
    let body = serde_json::to_vec(&serde_json::json!({
        "action": "closed",
        "repository": { "full_name": "marlboro-red/odin" },
        "issue": { "user": { "login": "octocat" } }
    }))
    .unwrap();

    let status = post_webhook(h.addr, "issues", &body, Some(&sign(SECRET, &body))).await;
    assert!(
        status.contains("202"),
        "accepted but unmatched, got: {status}"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        h.store.recent(10).await.unwrap().len(),
        0,
        "an unmatched event must not run"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_endpoint_responds_without_a_signature() {
    let h = Harness::start(Some(SECRET)).await;
    let mut stream = TcpStream::connect(h.addr).await.unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await.unwrap();
    assert!(
        resp.lines().next().unwrap_or_default().contains("200"),
        "resp: {resp}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dev_mode_accepts_unsigned_and_runs() {
    // No secret configured (dev mode): an unsigned, matching event is accepted and runs.
    let h = Harness::start(None).await;
    let body = labeled_payload();

    let status = post_webhook(h.addr, "issues", &body, None).await;
    assert!(
        status.contains("202"),
        "dev mode should accept unsigned, got: {status}"
    );

    let (n, terminal) = wait_for_terminal_run(&h.store, Duration::from_secs(15)).await;
    assert_eq!(n, 1, "dev-mode unsigned event should run");
    assert_eq!(terminal, Some(RunStatus::Succeeded));

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_json_body_is_rejected() {
    let h = Harness::start(None).await;
    let status = post_webhook(h.addr, "issues", b"{not valid json", None).await;
    assert!(
        status.contains("400"),
        "malformed JSON should be 400, got: {status}"
    );
    // Nothing should run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.store.recent(10).await.unwrap().len(), 0);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_event_header_is_rejected() {
    let h = Harness::start(None).await;
    // An empty X-GitHub-Event value is treated as missing.
    let status = post_webhook(h.addr, "", &labeled_payload(), None).await;
    assert!(
        status.contains("400"),
        "missing X-GitHub-Event should be 400, got: {status}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redelivered_webhook_runs_only_once() {
    let h = Harness::start(Some(SECRET)).await;
    let body = labeled_payload();
    let sig = sign(SECRET, &body);

    // post_webhook sends a fixed X-GitHub-Delivery, so the second POST is a "redelivery".
    let first = post_webhook(h.addr, "issues", &body, Some(&sig)).await;
    assert!(first.contains("202"), "first delivery accepted: {first}");
    let second = post_webhook(h.addr, "issues", &body, Some(&sig)).await;
    assert!(
        second.contains("200"),
        "a duplicate delivery should be acked (200) but not re-run: {second}"
    );

    let (n, terminal) = wait_for_terminal_run(&h.store, Duration::from_secs(15)).await;
    assert_eq!(n, 1, "a re-delivered webhook must not start a second run");
    assert_eq!(terminal, Some(RunStatus::Succeeded));
    h.shutdown().await;
}
