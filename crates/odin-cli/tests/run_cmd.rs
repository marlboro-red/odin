//! Drives the built `odin` binary end-to-end on shell-only workflows (no provider/API
//! cost), exercising the CLI → engine → worktree → executor path.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
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
fn run_executes_a_shell_only_workflow() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let wf = repo.path().join("wf.yaml");
    std::fs::write(
        &wf,
        "name: cli\nworkspace: { type: worktree }\nparams:\n  who: { required: true }\nsteps:\n  - id: edit\n    run: \"echo hi-{{ params.who }} >> README.md\"\n  - id: check\n    run: \"grep -q hi-bob README.md\"\n    depends_on: [edit]\n",
    )
    .unwrap();

    let out = odin(&[
        "run",
        wf.to_str().unwrap(),
        "--repo",
        repo.path().to_str().unwrap(),
        "--no-store",
        "--param",
        "who=bob",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "exit={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    assert!(stdout.contains("succeeded"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("✓ edit") && stdout.contains("✓ check"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn run_exits_nonzero_on_failure() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let wf = repo.path().join("wf.yaml");
    std::fs::write(
        &wf,
        "name: fail\nworkspace: { type: worktree }\nsteps:\n  - {id: boom, run: \"exit 1\"}\n",
    )
    .unwrap();

    let out = odin(&[
        "run",
        wf.to_str().unwrap(),
        "--repo",
        repo.path().to_str().unwrap(),
        "--no-store",
    ]);
    assert_eq!(out.status.code(), Some(1), "a failed run must exit 1");
}
