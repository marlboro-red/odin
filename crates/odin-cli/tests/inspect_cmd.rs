//! Drives `odin run` (persisted) then `odin list` / `show` / `logs` against the real
//! binary and a real SQLite store — no provider/API cost (shell-only workflow).

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
        .output()
        .unwrap()
}

#[test]
fn list_show_logs_a_persisted_run() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let repo_str = repo.path().to_str().unwrap();
    let wf = repo.path().join("wf.yaml");
    std::fs::write(
        &wf,
        "name: insp\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"echo hi >> README.md\"}\n",
    )
    .unwrap();

    // Run it, persisting to <repo>/.odin/state.db.
    let run = odin(&["run", wf.to_str().unwrap(), "--repo", repo_str]);
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    let run_id = stdout
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();

    // list shows it.
    let list = odin(&["list", "--repo", repo_str]);
    assert!(list.status.success());
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(listed.contains(&run_id), "list missing run id:\n{listed}");
    assert!(
        listed.contains("insp") && listed.contains("succeeded"),
        "list:\n{listed}"
    );

    // show prints details including the step.
    let show = odin(&["show", &run_id, "--repo", repo_str]);
    assert!(show.status.success());
    let shown = String::from_utf8_lossy(&show.stdout);
    assert!(
        shown.contains("succeeded") && shown.contains("Passed"),
        "show:\n{shown}"
    );

    // logs (JSON) contains the run lifecycle events.
    let logs = odin(&["logs", &run_id, "--repo", repo_str, "--json"]);
    assert!(logs.status.success());
    let logged = String::from_utf8_lossy(&logs.stdout);
    assert!(
        logged.contains("run_started") && logged.contains("run_finished"),
        "logs:\n{logged}"
    );

    // An unknown run id exits non-zero.
    let missing = odin(&[
        "show",
        "00000000-0000-0000-0000-000000000000",
        "--repo",
        repo_str,
    ]);
    assert_eq!(missing.status.code(), Some(1));
}

#[test]
fn list_json_on_missing_db_emits_empty_array() {
    // A fresh dir with no .odin/state.db — `--json` must still emit valid JSON to stdout.
    let dir = tempfile::tempdir().unwrap();
    let out = odin(&["list", "--repo", dir.path().to_str().unwrap(), "--json"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "[]");
}
