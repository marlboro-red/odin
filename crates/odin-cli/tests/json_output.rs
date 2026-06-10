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
    assert!(v["diagnostics"].is_array());

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
}
