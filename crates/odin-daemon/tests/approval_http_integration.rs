//! End-to-end tests for the daemon's `POST /approve` endpoint: a real HTTP POST → signature
//! check → `Engine::submit_approval` → the paused run resumes (approve) or fails with feedback
//! (reject), over a temp git repo and a shell-only approval workflow (no provider/API cost).

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use odin_core::{
    EngineBuilder, RunId, RunInput, RunStatus, SqliteStore, StepStatus, Store, Workflow,
};
use odin_daemon::WebhookServer;
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

/// A durable, shell-only workflow that pauses at a human gate: `plan → gate → ship`.
fn approval_workflow() -> Workflow {
    let src = "\
name: appr-flow
workspace: { type: worktree }
durable: true
steps:
  - { id: plan, run: \"echo planned\" }
  - id: gate
    approval: { message: \"ship it?\" }
    depends_on: [plan]
  - { id: ship, run: \"echo shipped\", depends_on: [gate] }
";
    Workflow::from_yaml_str(src).unwrap()
}

fn sign(secret: &str, body: &[u8]) -> String {
    use hmac::Mac as _;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// POSTs a raw HTTP/1.1 `/approve` request and returns the response status line.
async fn post_approve(addr: SocketAddr, body: &[u8], signature: Option<&str>) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let sig_header =
        signature.map_or_else(String::new, |s| format!("X-Hub-Signature-256: {s}\r\n"));
    let req = format!(
        "POST /approve HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
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

/// An engine + a bound approval server over a temp repo, with one run already PAUSED at its
/// gate. `/approve` resumes inline (no daemon needed — it calls the engine directly).
struct Harness {
    addr: SocketAddr,
    store: Arc<SqliteStore>,
    run_id: RunId,
    shutdown: CancellationToken,
    server_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    _repo: tempfile::TempDir,
}

impl Harness {
    async fn start(secret: Option<&str>) -> Self {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let store = Arc::new(SqliteStore::open(repo.path().join("state.db")).unwrap());
        let engine = EngineBuilder::new()
            .repo(repo.path())
            .store(store.clone())
            .build()
            .unwrap();

        let workflow = approval_workflow();
        // Run it once: it pauses AT the gate (downstream `ship` not yet run).
        let summary = engine.run(&workflow, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::AwaitingApproval,
            "the run must pause at the gate before we approve it over HTTP"
        );
        let run_id = summary.run_id;

        let mut server =
            WebhookServer::new("127.0.0.1:0".parse().unwrap(), secret.map(str::to_owned));
        server.enable_approvals(engine, Arc::from(vec![workflow]));
        let bound = server.bind().await.unwrap();
        let addr = bound.local_addr();
        let shutdown = CancellationToken::new();
        let server_task = tokio::spawn(bound.serve(shutdown.clone()));

        Self {
            addr,
            store,
            run_id,
            shutdown,
            server_task,
            _repo: repo,
        }
    }

    async fn status(&self) -> RunStatus {
        self.store
            .load_run(self.run_id)
            .await
            .unwrap()
            .expect("the run exists")
            .status
    }

    async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.server_task).await;
    }
}

const SECRET: &str = "test-secret";

fn approve_body(run_id: RunId) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "run_id": run_id.to_string(),
        "decision": "approved",
        "approver": "alice",
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_approve_resumes_the_paused_run_to_completion() {
    let h = Harness::start(Some(SECRET)).await;
    let body = approve_body(h.run_id);

    let status = post_approve(h.addr, &body, Some(&sign(SECRET, &body))).await;
    assert!(status.contains("200"), "expected 200 OK, got: {status}");

    // The handler resumes inline, so by the time it answers 200 the run is already terminal.
    assert_eq!(
        h.status().await,
        RunStatus::Succeeded,
        "an approved run should run to completion"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approve_with_a_bad_signature_is_rejected_and_the_run_stays_paused() {
    let h = Harness::start(Some(SECRET)).await;
    let body = approve_body(h.run_id);

    let status = post_approve(h.addr, &body, Some(&sign("wrong-secret", &body))).await;
    assert!(status.contains("401"), "expected 401, got: {status}");

    // The forged request must NOT have decided the gate.
    assert_eq!(h.status().await, RunStatus::AwaitingApproval);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_reject_fails_the_run_with_feedback() {
    let h = Harness::start(Some(SECRET)).await;
    let body = serde_json::to_vec(&serde_json::json!({
        "run_id": h.run_id.to_string(),
        "decision": "rejected",
        "approver": "bob",
        "note": "add tests first",
    }))
    .unwrap();

    let status = post_approve(h.addr, &body, Some(&sign(SECRET, &body))).await;
    assert!(status.contains("200"), "expected 200 OK, got: {status}");

    assert_eq!(
        h.status().await,
        RunStatus::Failed,
        "a reject fails the gate and the run"
    );
    // Non-vacuous: the gate carries the feedback, and `ship` was skipped.
    let run = h.store.load_run(h.run_id).await.unwrap().unwrap();
    let step = |id: &str| {
        run.steps
            .iter()
            .find(|(sid, _)| sid.as_str() == id)
            .map(|(_, st)| st)
            .unwrap()
    };
    let gate = step("gate");
    assert_eq!(gate.status, StepStatus::Failed);
    assert_eq!(
        gate.outputs.get("feedback").and_then(|v| v.as_str()),
        Some("add tests first"),
        "the reject note is recorded as the gate's feedback"
    );
    assert_eq!(step("ship").status, StepStatus::Skipped);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reject_without_a_note_is_rejected_and_the_run_stays_paused() {
    let h = Harness::start(Some(SECRET)).await;
    let body = serde_json::to_vec(&serde_json::json!({
        "run_id": h.run_id.to_string(),
        "decision": "rejected",
        "approver": "bob",
    }))
    .unwrap();

    let status = post_approve(h.addr, &body, Some(&sign(SECRET, &body))).await;
    assert!(
        status.contains("400"),
        "a reject without a note should be 400, got: {status}"
    );
    assert_eq!(h.status().await, RunStatus::AwaitingApproval);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_reject_with_rerun_fails_the_original_and_starts_a_fresh_run() {
    let h = Harness::start(Some(SECRET)).await;
    let body = serde_json::to_vec(&serde_json::json!({
        "run_id": h.run_id.to_string(),
        "decision": "rejected",
        "approver": "bob",
        "note": "redo it",
        "rerun": true,
    }))
    .unwrap();

    let status = post_approve(h.addr, &body, Some(&sign(SECRET, &body))).await;
    assert!(status.contains("200"), "expected 200 OK, got: {status}");

    // The original is failed; a fresh run was started and paused at its gate, carrying feedback.
    assert_eq!(h.status().await, RunStatus::Failed);
    let runs = h.store.recent(10).await.unwrap();
    assert_eq!(
        runs.len(),
        2,
        "reject --rerun leaves the failed original + a fresh run"
    );
    let rerun = runs
        .iter()
        .find(|r| r.status == RunStatus::AwaitingApproval)
        .expect("the rerun paused at its gate");
    assert_eq!(
        rerun.input.params.get("feedback").and_then(|v| v.as_str()),
        Some("redo it"),
        "the rerun carries the note as its feedback param"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rerun_with_an_approve_decision_is_400() {
    let h = Harness::start(Some(SECRET)).await;
    let body = serde_json::to_vec(&serde_json::json!({
        "run_id": h.run_id.to_string(),
        "decision": "approved",
        "rerun": true,
    }))
    .unwrap();

    let status = post_approve(h.addr, &body, Some(&sign(SECRET, &body))).await;
    assert!(
        status.contains("400"),
        "rerun only applies to a reject, got: {status}"
    );
    assert_eq!(h.status().await, RunStatus::AwaitingApproval);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approving_an_unknown_run_is_404() {
    let h = Harness::start(Some(SECRET)).await;
    let body = approve_body(RunId::new());

    let status = post_approve(h.addr, &body, Some(&sign(SECRET, &body))).await;
    assert!(
        status.contains("404"),
        "unknown run should be 404, got: {status}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dev_mode_accepts_an_unsigned_approve() {
    // No secret configured (dev mode): an unsigned approve is accepted and resumes the run.
    let h = Harness::start(None).await;
    let body = approve_body(h.run_id);

    let status = post_approve(h.addr, &body, None).await;
    assert!(
        status.contains("200"),
        "dev mode should accept unsigned, got: {status}"
    );
    assert_eq!(h.status().await, RunStatus::Succeeded);
    h.shutdown().await;
}
