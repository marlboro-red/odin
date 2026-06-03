//! Drives `odin run` (persisted) then `odin status` (text + `--json`) against the real binary
//! and a real SQLite store — shell-only workflow, no provider/API cost.

use std::path::Path;
use std::process::Command;

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

fn odin(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_odin"))
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn status_renders_text_and_matching_json() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let repo_str = repo.path().to_str().unwrap();
    let wf = repo.path().join("wf.yaml");
    std::fs::write(
        &wf,
        "name: stat\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"true\"}\n  - {id: b, run: \"true\", depends_on: [a]}\n",
    )
    .unwrap();
    assert!(
        odin(&["run", wf.to_str().unwrap(), "--repo", repo_str])
            .status
            .success()
    );

    // Text view: the summary header + the run row.
    let text = odin(&["status", "--repo", repo_str]);
    assert!(text.status.success());
    let out = String::from_utf8_lossy(&text.stdout);
    assert!(out.contains("succeeded"), "status text:\n{out}");
    assert!(
        out.contains("stat"),
        "status text should name the workflow:\n{out}"
    );

    // JSON view: the RunView shape (same as the daemon's /api/runs).
    let json = odin(&["status", "--repo", repo_str, "--json"]);
    assert!(json.status.success());
    let views: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let run = &views[0];
    assert_eq!(run["workflow"], "stat");
    assert_eq!(run["status"], "succeeded");
    assert_eq!(run["steps"].as_array().unwrap().len(), 2);
    assert_eq!(run["steps"][0]["status"], "passed");
    assert!(run["gate"].is_null(), "a succeeded run has no gate");
    assert!(run["run_id"].as_str().unwrap().len() >= 8);

    // No database yet → graceful (empty JSON array, exit 0).
    let empty = odin(&[
        "status",
        "--repo",
        repo_str,
        "--db",
        "/tmp/nope-odin-xyz.db",
        "--json",
    ]);
    assert!(empty.status.success());
    assert_eq!(String::from_utf8_lossy(&empty.stdout).trim(), "[]");
}
