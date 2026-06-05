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

const TEMPLATE: &str = concat!(
    "# odin:template\n",
    "#   provider: { default: claude }\n",
    "#   base_branch: { required: true }\n",
    "# end\n",
    "name: tpl-src\n",
    "description: a parameterized template\n",
    "steps:\n",
    "  - id: r\n",
    "    run: \"git diff @@base_branch@@...HEAD\"\n",
    "  - id: p\n",
    "    provider: @@provider@@\n",
    "    prompt: hi\n",
    "    depends_on: [r]\n",
);

fn write_template(cwd: &Path) -> std::path::PathBuf {
    let tpl = cwd.join("tpl.yaml");
    std::fs::write(&tpl, TEMPLATE).unwrap();
    tpl
}

#[test]
fn new_fills_a_template_with_set_and_defaults() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    let tpl = write_template(cwd.path());
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &[
            "recipe",
            "new",
            "filled",
            "--from",
            tpl.to_str().unwrap(),
            "--set",
            "base_branch=develop",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let written = std::fs::read_to_string(cwd.path().join("filled.yaml")).unwrap();
    assert!(written.contains("name: filled"), "got:\n{written}");
    assert!(
        written.contains("git diff develop...HEAD"),
        "--set not applied:\n{written}"
    );
    assert!(
        written.contains("provider: claude"),
        "default not applied:\n{written}"
    );
    assert!(
        !written.contains("# odin:template"),
        "header not stripped:\n{written}"
    );
    assert!(!written.contains("@@"), "markers remain:\n{written}");
}

#[test]
fn new_template_missing_required_errors() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    let tpl = write_template(cwd.path());
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &["recipe", "new", "x", "--from", tpl.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing required"));
}

#[test]
fn new_set_on_a_non_template_source_errors() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    // `iterate` is a plain starter (no `# odin:template` header).
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &["recipe", "new", "x", "--from", "iterate", "--set", "a=b"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("declares no template variables"));
}

#[test]
fn new_stdout_prints_and_writes_no_file() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &["recipe", "new", "piped", "--from", "iterate", "--stdout"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("name: piped"));
    assert!(
        !cwd.path().join("piped.yaml").exists(),
        "--stdout must not write a file"
    );
}

#[test]
fn new_catalog_installs_runnable_by_name() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    let made = odin_in(
        cwd.path(),
        rc.path(),
        &[
            "recipe",
            "new",
            "installed",
            "--from",
            "iterate",
            "--catalog",
        ],
    );
    assert!(
        made.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    // It now resolves in the catalog.
    let path = odin_in(cwd.path(), rc.path(), &["recipe", "path", "installed"]);
    assert!(path.status.success());
    assert!(String::from_utf8_lossy(&path.stdout).contains("installed.yaml"));
}

#[test]
fn new_explain_describes_scaffold_vars_and_writes_nothing() {
    let cwd = tempfile::tempdir().unwrap();
    let rc = tempfile::tempdir().unwrap();
    let tpl = write_template(cwd.path());
    let out = odin_in(
        cwd.path(),
        rc.path(),
        &[
            "recipe",
            "new",
            "preview",
            "--from",
            tpl.to_str().unwrap(),
            "--set",
            "base_branch=main",
            "--explain",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Scaffold-time variables"), "got:\n{stdout}");
    assert!(
        stdout.contains("@@base_branch@@") && stdout.contains("main"),
        "got:\n{stdout}"
    );
    assert!(
        stdout.contains("@@provider@@") && stdout.contains("claude"),
        "default not shown:\n{stdout}"
    );
    assert!(
        !cwd.path().join("preview.yaml").exists(),
        "--explain must not write a file"
    );
}
