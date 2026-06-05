//! Security regression tests for `odind --recipes`: the fail-closed secret requirement must cover
//! workflows supplied by the **recipe catalog** (not just `--workflows`), so a catalog-declared
//! `github_webhook` trigger or `approval` gate can't open an unauthenticated mutating endpoint.
//! These boot the real `odind` binary (the fail-closed check lives in `main.rs::serve`, not the
//! `odin_daemon` lib).

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const WEBHOOK_WF: &str = "name: hooky\nworkspace: { type: worktree }\n\
    triggers:\n  - type: github_webhook\n    events: [\"pull_request.opened\"]\n\
    steps:\n  - {id: a, run: \"echo hi\"}\n";

const APPROVAL_WF: &str = "name: gated\ndurable: true\nworkspace: { type: worktree }\n\
    steps:\n  - {id: a, run: \"echo hi\"}\n  - id: g\n    approval: { message: ok }\n    depends_on: [a]\n";

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
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "init"]);
}

fn write_recipe(dir: &Path, file: &str, content: &str) {
    std::fs::write(dir.join(file), content).unwrap();
}

/// Runs `odind --recipes-dir <recs>` to completion **without** a secret — only valid when the
/// daemon is expected to bail at the fail-closed check (so `.output()` doesn't block forever).
fn odind_no_secret(repo: &Path, recs: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_odind"))
        .args([
            "--recipes-dir",
            recs.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--db",
            repo.join("state.db").to_str().unwrap(),
            "--webhook-addr",
            "127.0.0.1:0",
        ])
        .env_remove("ODIN_WEBHOOK_SECRET")
        .output()
        .unwrap()
}

#[test]
fn catalog_webhook_without_secret_fails_closed() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let recs = tempfile::tempdir().unwrap();
    write_recipe(recs.path(), "hooky.yaml", WEBHOOK_WF);

    let out = odind_no_secret(repo.path(), recs.path());
    assert!(!out.status.success(), "should fail closed");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no secret is configured"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn catalog_approval_gate_without_secret_fails_closed() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let recs = tempfile::tempdir().unwrap();
    write_recipe(recs.path(), "gated.yaml", APPROVAL_WF);

    let out = odind_no_secret(repo.path(), recs.path());
    assert!(
        !out.status.success(),
        "an approval gate also exposes /approve → fail closed"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no secret is configured"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn catalog_webhook_with_secret_passes_the_gate_and_listens() {
    // The positive control: with a secret, the same catalog webhook workflow gets past the
    // fail-closed check and the server reaches "listening" (then we stop it).
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let recs = tempfile::tempdir().unwrap();
    write_recipe(recs.path(), "hooky.yaml", WEBHOOK_WF);
    let logf = repo.path().join("odind.log");
    let log = std::fs::File::create(&logf).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_odind"))
        .args([
            "--recipes-dir",
            recs.path().to_str().unwrap(),
            "--repo",
            repo.path().to_str().unwrap(),
            "--db",
            repo.path().join("state.db").to_str().unwrap(),
            "--webhook-addr",
            "127.0.0.1:0",
        ])
        .env("ODIN_WEBHOOK_SECRET", "test-secret")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let listening = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Err(format!("odind exited early ({status})"));
        }
        if std::fs::read_to_string(&logf)
            .unwrap_or_default()
            .contains("listening")
        {
            break Ok(());
        }
        if Instant::now() > deadline {
            break Err("timed out waiting for 'listening'".to_owned());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        listening.is_ok(),
        "{}: log:\n{}",
        listening.unwrap_err(),
        std::fs::read_to_string(&logf).unwrap_or_default()
    );
}
