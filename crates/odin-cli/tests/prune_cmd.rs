//! Drives the built `odin prune` binary end-to-end against a real SQLite store (shell-only
//! workflow, no provider/API cost): the no-limit guard, dry-run, non-TTY auto-decline, and a
//! real `--yes` prune that bounds the store.

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
    // Null stdin so `prune`'s TTY check is deterministic (never inherits the test runner's
    // terminal): without `--yes` it must auto-decline rather than block on a prompt.
    Command::new(env!("CARGO_BIN_EXE_odin"))
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn list_count(repo: &str) -> usize {
    let out = odin(&["list", "--repo", repo, "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v.as_array().map_or(0, Vec::len)
}

#[test]
fn prune_guards_dry_runs_and_deletes_terminal_runs() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let repo_str = repo.path().to_str().unwrap();
    let wf = repo.path().join("wf.yaml");
    std::fs::write(
        &wf,
        "name: pr\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"echo hi\"}\n",
    )
    .unwrap();

    // Three succeeded (terminal) runs.
    for _ in 0..3 {
        let run = odin(&["run", wf.to_str().unwrap(), "--repo", repo_str]);
        assert!(
            run.status.success(),
            "run failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
    assert_eq!(list_count(repo_str), 3);

    // No age/count limit → refuses (exit 2), deletes nothing.
    let no_limit = odin(&["prune", "--repo", repo_str]);
    assert!(!no_limit.status.success(), "must refuse with no limit");
    assert!(
        String::from_utf8_lossy(&no_limit.stderr).contains("no age or count limit"),
        "stderr: {}",
        String::from_utf8_lossy(&no_limit.stderr)
    );
    assert_eq!(list_count(repo_str), 3);

    // Dry run reports but deletes nothing.
    let dry = odin(&["prune", "--repo", repo_str, "--keep-last", "1", "--dry-run"]);
    assert!(dry.status.success());
    assert!(
        String::from_utf8_lossy(&dry.stdout).contains("Would prune 2 run(s)"),
        "dry stdout: {}",
        String::from_utf8_lossy(&dry.stdout)
    );
    assert_eq!(list_count(repo_str), 3, "dry run deletes nothing");

    // Without --yes and with no TTY (piped stdout), it auto-declines.
    let declined = odin(&["prune", "--repo", repo_str, "--keep-last", "1"]);
    assert!(declined.status.success());
    assert_eq!(list_count(repo_str), 3, "non-TTY without --yes is a no-op");

    // Real prune with --yes.
    let pruned = odin(&["prune", "--repo", repo_str, "--keep-last", "1", "--yes"]);
    assert!(pruned.status.success());
    assert!(
        String::from_utf8_lossy(&pruned.stdout).contains("Pruned 2 run(s)"),
        "prune stdout: {}",
        String::from_utf8_lossy(&pruned.stdout)
    );
    assert_eq!(list_count(repo_str), 1, "keep-last 1 leaves one run");

    // --json emits a machine-readable report.
    let json = odin(&[
        "prune",
        "--repo",
        repo_str,
        "--keep-last",
        "0",
        "--yes",
        "--json",
    ]);
    assert!(json.status.success());
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["runs_pruned"], 1);
    assert_eq!(list_count(repo_str), 0);
}
