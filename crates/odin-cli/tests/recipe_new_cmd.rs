//! Drives `odin recipe new` end-to-end on the built binary (scaffold a workflow from a starter,
//! recipe, or file). Uses a temp cwd + a temp `ODIN_RECIPES_DIR`, matching the other `*_cmd` tests.

use std::path::Path;
use std::process::{Command, Output};

fn odin_in(cwd: &Path, recipes_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_odin"))
        .current_dir(cwd)
        .env("ODIN_RECIPES_DIR", recipes_dir)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn new_scaffolds_from_a_bundled_starter() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap(); // empty catalog — the bundled starter still resolves
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &["recipe", "new", "my-review", "--from", "local-review"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let written = std::fs::read_to_string(cwd.path().join("my-review.yaml")).unwrap();
    assert!(written.contains("name: my-review"), "got:\n{written}");
    assert!(
        !written.contains("name: local-review"),
        "old name not rewritten:\n{written}"
    );
}

#[test]
fn new_out_as_file_and_as_directory() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    // --out with a .yaml extension is an explicit file path.
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &[
            "recipe",
            "new",
            "x",
            "--from",
            "iterate",
            "--out",
            "custom.yaml",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cwd.path().join("custom.yaml").exists());
    // --out without an extension is a directory to create and write <name>.yaml into.
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &["recipe", "new", "y", "--from", "iterate", "--out", "sub"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cwd.path().join("sub").join("y.yaml").exists());
}

#[test]
fn new_refuses_overwrite_without_force() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    assert!(
        odin_in(
            cwd.path(),
            rc.path(),
            &["recipe", "new", "dup", "--from", "iterate"]
        )
        .status
        .success()
    );
    let again = odin_in(
        cwd.path(),
        rc.path(),
        &["recipe", "new", "dup", "--from", "iterate"],
    );
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));
    assert!(
        odin_in(
            cwd.path(),
            rc.path(),
            &["recipe", "new", "dup", "--from", "iterate", "--force"]
        )
        .status
        .success()
    );
}

#[test]
fn new_bad_source_lists_starters() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &["recipe", "new", "x", "--from", "does-not-exist"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("starters:"));
}

#[test]
fn new_rejects_a_non_plain_name() {
    // `is_valid_name` would accept `#bad`, but a workflow `name:` value must be a plain scalar —
    // the error points at the NAME arg, not the source file.
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &["recipe", "new", "#bad", "--from", "iterate"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid recipe name"));
}
