//! The machine-readable `--json` surfaces: scripts/bots must get parseable stdout (and the same
//! envelope shape) from `validate`, `run` (incl. on a validation failure), and `cancel`.

use std::process::Output;

fn odin(args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_odin"))
        .args(args)
        .output()
        .unwrap()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not JSON: {e}\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn git(dir: &std::path::Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn init_repo(dir: &std::path::Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "t@odin.invalid"]);
    git(dir, &["config", "user.name", "Odin Test"]);
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "init"]);
}

#[test]
fn validate_json_unified_envelope() {
    let dir = tempfile::tempdir().unwrap();

    let ok = dir.path().join("ok.yaml");
    std::fs::write(&ok, "name: ok\nsteps:\n  - {id: a, run: \"true\"}\n").unwrap();
    let out = odin(&["validate", ok.to_str().unwrap(), "--json"]);
    assert!(out.status.success());
    let v = json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["phase"], "validate");
    assert_eq!(v["error"], serde_json::Value::Null);
    assert_eq!(v["diagnostics"].as_array().unwrap().len(), 0);

    let bad = dir.path().join("bad.yaml");
    std::fs::write(&bad, "name: x\nsteps: [ {id: a, run: ").unwrap();
    let out = odin(&["validate", bad.to_str().unwrap(), "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let v = json(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["phase"], "parse");
    assert!(v["error"].is_string());
}

#[test]
fn run_json_emits_validation_envelope_on_invalid_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let dup = dir.path().join("dup.yaml");
    std::fs::write(
        &dup,
        "name: dup\nsteps:\n  - {id: a, run: \"true\"}\n  - {id: a, run: \"true\"}\n",
    )
    .unwrap();
    // Previously this emitted ZERO bytes on stdout; now it's the same envelope as `validate`.
    let out = odin(&["run", dup.to_str().unwrap(), "--no-store", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["phase"], "validate");
    assert!(
        v["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "ODIN003"),
        "expected the duplicate-step-id diagnostic: {v}"
    );
}

#[test]
fn cancel_json_reports_requested() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let out = odin(&[
        "cancel",
        "00000000-0000-0000-0000-000000000000",
        "--db",
        db.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2), "unknown run → exit 2");
    let v = json(&out);
    assert_eq!(v["requested"], false);
    assert_eq!(v["run_id"], "00000000-0000-0000-0000-000000000000");

    // A bad UUID under --json is still a parseable envelope on stdout, not an empty stream.
    let out = odin(&["cancel", "not-a-uuid", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let v = json(&out);
    assert_eq!(v["requested"], false);
    assert!(v["error"].is_string());
}

/// `approve --json` emits the resulting `RunSummary` (the same shape `run --json` does), and a
/// not-found approve emits a `{ok:false, error}` envelope on stdout — never empty.
#[test]
fn approve_json_emits_run_summary_and_error_envelope() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let wf = repo.path().join("appr.yaml");
    std::fs::write(
        &wf,
        "name: appr\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: plan, run: \"true\"}\n  - id: gate\n    approval: { message: \"ok?\" }\n    depends_on: [plan]\n  - {id: ship, run: \"true\", depends_on: [gate]}\n",
    )
    .unwrap();
    let repo_s = repo.path().to_str().unwrap();

    // Run durably → pauses at the gate (exit 0), JSON summary names the run.
    let out = odin(&["run", wf.to_str().unwrap(), "--repo", repo_s, "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out);
    assert_eq!(v["status"], "awaiting_approval");
    let run_id = v["run_id"].as_str().unwrap().to_owned();

    // Approve → run completes; JSON is a RunSummary (same shape).
    let out = odin(&[
        "approve",
        &run_id,
        "--workflow",
        wf.to_str().unwrap(),
        "--repo",
        repo_s,
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out);
    assert_eq!(v["run_id"], run_id);
    assert_eq!(v["status"], "succeeded");
    assert!(v["steps"].is_array(), "RunSummary shape: {v}");

    // Not-found approve under --json → error envelope on stdout, exit 2.
    let out = odin(&[
        "approve",
        "00000000-0000-0000-0000-000000000000",
        "--workflow",
        wf.to_str().unwrap(),
        "--repo",
        repo_s,
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let v = json(&out);
    assert_eq!(v["ok"], false);
    assert!(v["error"].is_string());
}
