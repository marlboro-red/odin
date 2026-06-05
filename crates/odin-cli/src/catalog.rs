//! The workflow **recipe catalog**: a user-level directory of workflow `.yaml` files that `odin`
//! can run, validate, and inspect *by name* instead of by filesystem path.
//!
//! A recipe is just a workflow file — no separate format. Its **catalog key is the filename stem**
//! (`adversarial-review.yaml` → `adversarial-review`), so lookup is an O(1) path join; the file's
//! own `name:` / `description:` fields are read only for listing.
//!
//! The catalog directory is resolved with this precedence:
//! 1. an explicit `--recipes-dir <path>`,
//! 2. the `ODIN_RECIPES_DIR` environment variable,
//! 3. the platform data-local directory — [`directories::ProjectDirs::data_local_dir`] + `recipes`:
//!    - macOS: `~/Library/Application Support/odin/recipes`
//!    - Linux: `~/.local/share/odin/recipes` (honoring `$XDG_DATA_HOME`)
//!    - Windows: `%LOCALAPPDATA%\odin\data\recipes`

use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow};
use odin_core::Workflow;

/// Environment variable that overrides the catalog directory (below an explicit `--recipes-dir`).
pub(crate) const RECIPES_DIR_ENV: &str = "ODIN_RECIPES_DIR";

/// Resolves the catalog directory **without creating it** (read paths treat a missing dir as an
/// empty catalog). `override_dir` is the `--recipes-dir` flag value, if any.
///
/// # Errors
/// Fails only in the fallback case when no home/data directory can be determined for the platform.
pub(crate) fn dir(override_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(d) = override_dir {
        return Ok(d.to_path_buf());
    }
    // An unset *or empty* env var falls through to the platform default.
    if let Some(d) = std::env::var_os(RECIPES_DIR_ENV) {
        if !d.is_empty() {
            return Ok(PathBuf::from(d));
        }
    }
    let proj = directories::ProjectDirs::from("", "", "odin").ok_or_else(|| {
        anyhow!(
            "could not determine a home/data directory for the recipe catalog; \
             set {RECIPES_DIR_ENV} or pass --recipes-dir <path>"
        )
    })?;
    Ok(proj.data_local_dir().join("recipes"))
}

/// Resolves the catalog directory and creates it if missing — for write/seed operations.
///
/// # Errors
/// Fails if the directory cannot be resolved (see [`dir`]) or created.
pub(crate) fn dir_create(override_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    let d = dir(override_dir)?;
    std::fs::create_dir_all(&d)
        .with_context(|| format!("creating recipe catalog directory {}", d.display()))?;
    Ok(d)
}

/// The bundled **starter recipes**, embedded at build time so an installed `odin` can seed a
/// catalog with no source tree nearby. Each entry is `(catalog name, YAML contents)`; the name is
/// the example's filename stem (kept in sync with `examples/`). `recipe init` writes these out.
pub(crate) const STARTERS: &[(&str, &str)] = &[
    (
        "adversarial-review",
        include_str!("../../../examples/adversarial-review.yaml"),
    ),
    (
        "fix-flaky-test",
        include_str!("../../../examples/fix-flaky-test.yaml"),
    ),
    (
        "gated-deploy",
        include_str!("../../../examples/gated-deploy.yaml"),
    ),
    (
        "issue-to-pr",
        include_str!("../../../examples/issue-to-pr.yaml"),
    ),
    ("iterate", include_str!("../../../examples/iterate.yaml")),
    (
        "local-review",
        include_str!("../../../examples/local-review.yaml"),
    ),
    (
        "loop-with-case",
        include_str!("../../../examples/loop-with-case.yaml"),
    ),
    (
        "multi-agent-eval",
        include_str!("../../../examples/multi-agent-eval.yaml"),
    ),
    (
        "nightly-maintenance",
        include_str!("../../../examples/nightly-maintenance.yaml"),
    ),
    (
        "self-correct",
        include_str!("../../../examples/self-correct.yaml"),
    ),
    (
        "ship-release",
        include_str!("../../../examples/ship-release.yaml"),
    ),
    ("triage", include_str!("../../../examples/triage.yaml")),
];

/// Writes the bundled [`STARTERS`] into `dir`, skipping any that already exist unless `force`.
/// Returns `(written names, skipped names)`.
///
/// # Errors
/// Fails if a starter file cannot be written.
pub(crate) fn seed(
    dir: &Path,
    force: bool,
) -> anyhow::Result<(Vec<&'static str>, Vec<&'static str>)> {
    let mut wrote = Vec::new();
    let mut skipped = Vec::new();
    for (name, body) in STARTERS {
        let path = dir.join(format!("{name}.yaml"));
        if path.exists() && !force {
            skipped.push(*name);
            continue;
        }
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        wrote.push(*name);
    }
    Ok((wrote, skipped))
}

/// A recipe as listed: its catalog key (filename stem), path, and best-effort parsed metadata.
/// `workflow_name`/`description` are `None` if the file does not parse as a workflow.
pub(crate) struct Recipe {
    pub name: String,
    pub path: PathBuf,
    pub workflow_name: Option<String>,
    pub description: Option<String>,
    /// The workflow's normalized `tags` (already trimmed/lowercased by the IR), empty if none or
    /// if the file does not parse.
    pub tags: Vec<String>,
}

/// A recipe name must be a single, plain path component — never a separator, `.`/`..`, or empty —
/// so a catalog lookup can never escape the catalog directory.
pub(crate) fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

fn is_yaml(p: &Path) -> bool {
    matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml"))
}

/// Resolves a recipe `name` to its file path within `dir` (`<name>.yaml`, then `<name>.yml`).
/// Returns `None` for an invalid name or a recipe that does not exist.
pub(crate) fn resolve(dir: &Path, name: &str) -> Option<PathBuf> {
    if !is_valid_name(name) {
        return None;
    }
    ["yaml", "yml"]
        .iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .find(|p| p.is_file())
}

/// Resolves a `run`/`validate` workflow argument to a file path: **an existing file path wins**;
/// otherwise the argument is treated as a recipe **name** and looked up in the catalog. This keeps
/// `odin run ./wf.yaml` working exactly as before while adding `odin run <recipe-name>`.
///
/// # Errors
/// Fails (with a hint listing available recipes) if `arg` is neither an existing file nor a recipe.
pub(crate) fn resolve_arg(arg: &Path, recipes_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    if arg.is_file() {
        return Ok(arg.to_path_buf());
    }
    let dir = dir(recipes_dir)?;
    if let Some(path) = arg.to_str().and_then(|name| resolve(&dir, name)) {
        return Ok(path);
    }
    let available = list(&dir).unwrap_or_default();
    let hint = if available.is_empty() {
        format!(
            "not a file, and the recipe catalog at {} is empty (`odin recipe init` to seed it)",
            dir.display()
        )
    } else {
        let names: Vec<&str> = available.iter().map(|r| r.name.as_str()).collect();
        format!(
            "not a file, and no such recipe in {} (available: {})",
            dir.display(),
            names.join(", ")
        )
    };
    anyhow::bail!("cannot find workflow '{}': {hint}", arg.display())
}

/// The raw body of a scaffold source plus a human label of where it came from.
#[derive(Debug)]
pub(crate) struct SourceBody {
    pub body: String,
    pub provenance: String,
}

/// Resolves a `recipe new --from <src>` source to its raw YAML, trying — in order — an existing
/// **file path**, a **catalog recipe** by name, then a **bundled starter** by name. (The
/// file-first rule matches [`resolve_arg`].) On a miss, the error lists both the catalog and the
/// built-in starter names.
///
/// # Errors
/// Fails if the source can't be found, or a found file/recipe can't be read.
pub(crate) fn resolve_source(from: &str, recipes_dir: Option<&Path>) -> anyhow::Result<SourceBody> {
    let as_path = Path::new(from);
    if as_path.is_file() {
        let body = std::fs::read_to_string(as_path)
            .with_context(|| format!("reading source file {from}"))?;
        return Ok(SourceBody {
            body,
            provenance: format!("file {from}"),
        });
    }
    let dir = dir(recipes_dir)?;
    if let Some(path) = resolve(&dir, from) {
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading recipe {from} at {}", path.display()))?;
        return Ok(SourceBody {
            body,
            provenance: format!("recipe {from}"),
        });
    }
    if let Some((_, body)) = STARTERS.iter().find(|(n, _)| *n == from) {
        return Ok(SourceBody {
            body: (*body).to_owned(),
            provenance: format!("built-in starter {from}"),
        });
    }
    let catalog = list(&dir).unwrap_or_default();
    let recipes = if catalog.is_empty() {
        "none".to_owned()
    } else {
        catalog
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let starters = STARTERS
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "no source {from:?}: not a file, not a recipe in {} (recipes: {recipes}), \
         and not a built-in starter (starters: {starters})",
        dir.display()
    )
}

/// A `recipe new <name>` target must be a plain slug — a valid catalog name whose every character
/// is `[A-Za-z0-9._-]`. That makes it both a safe filename **and** a plain-scalar YAML `name:`
/// value (unlike [`is_valid_name`], which also permits YAML-indicator characters like `#`/`*`/`:`).
pub(crate) fn is_plain_name(name: &str) -> bool {
    is_valid_name(name)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Returns `src` with its top-level `name:` rewritten to `new_name`, re-parsing to assert the
/// rewrite round-trips exactly. `new_name` must already be a [`is_plain_name`]; the first column-0
/// `name:` line is the workflow name (steps/params are indented), and all bundled starters carry
/// an unquoted `name:` at column 0.
///
/// # Errors
/// Fails if the source has no top-level `name:` line or the rewrite no longer parses to `new_name`.
pub(crate) fn rewrite_workflow_name(src: &str, new_name: &str) -> anyhow::Result<String> {
    let mut lines: Vec<String> = src.lines().map(str::to_owned).collect();
    let Some(line) = lines.iter_mut().find(|l| l.starts_with("name:")) else {
        anyhow::bail!("source has no top-level `name:` line to rewrite");
    };
    *line = format!("name: {new_name}");
    let mut out = lines.join("\n");
    if src.ends_with('\n') {
        out.push('\n');
    }
    // The assert is the real guard: if the rewrite somehow hit the wrong line or produced an
    // invalid scalar, this catches it (worst case is a loud refusal, never a silently-wrong file).
    let wf = Workflow::from_yaml_str(&out)
        .with_context(|| format!("rewritten workflow (name: {new_name}) no longer parses"))?;
    anyhow::ensure!(
        wf.name.as_str() == new_name,
        "name rewrite did not round-trip (got {:?})",
        wf.name.as_str()
    );
    Ok(out)
}

/// Lists every recipe in `dir`, sorted by name, reading each file's metadata best-effort. A
/// missing directory is an **empty** catalog (not an error); an unreadable directory is an error.
///
/// # Errors
/// Fails if `dir` exists but cannot be read.
pub(crate) fn list(dir: &Path) -> anyhow::Result<Vec<Recipe>> {
    let read = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading recipe catalog {}", dir.display()));
        }
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_yaml(p))
        .collect();
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_owned();
            let (workflow_name, description, tags) = match Workflow::from_yaml_path(&path) {
                Ok(wf) => (Some(wf.name.to_string()), wf.description, wf.tags),
                Err(_) => (None, None, Vec::new()),
            };
            Recipe {
                name,
                path,
                workflow_name,
                description,
                tags,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, file: &str, body: &str) {
        std::fs::write(dir.join(file), body).unwrap();
    }

    const WF: &str =
        "name: demo\ndescription: a demo recipe\nsteps:\n  - id: s\n    run: \"echo hi\"\n";

    #[test]
    fn dir_prefers_explicit_override() {
        let p = PathBuf::from("/tmp/some/where");
        assert_eq!(dir(Some(&p)).unwrap(), p);
    }

    #[test]
    fn valid_name_rejects_traversal_and_separators() {
        assert!(is_valid_name("adversarial-review"));
        assert!(is_valid_name("a.b.c"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("."));
        assert!(!is_valid_name(".."));
        assert!(!is_valid_name("../escape"));
        assert!(!is_valid_name("nested/name"));
        assert!(!is_valid_name("back\\slash"));
    }

    #[test]
    fn resolve_finds_yaml_and_yml_only() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "alpha.yaml", WF);
        write(tmp.path(), "beta.yml", WF);
        write(tmp.path(), "notes.txt", "ignore me");
        assert_eq!(
            resolve(tmp.path(), "alpha"),
            Some(tmp.path().join("alpha.yaml"))
        );
        assert_eq!(
            resolve(tmp.path(), "beta"),
            Some(tmp.path().join("beta.yml"))
        );
        assert_eq!(resolve(tmp.path(), "notes"), None);
        assert_eq!(resolve(tmp.path(), "missing"), None);
        assert_eq!(resolve(tmp.path(), ".."), None);
    }

    #[test]
    fn list_is_sorted_and_reads_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "zeta.yaml", WF);
        write(tmp.path(), "alpha.yaml", WF);
        write(tmp.path(), "ignore.json", "{}");
        write(tmp.path(), "broken.yaml", "this: : not valid yaml: [");
        let got = list(tmp.path()).unwrap();
        let names: Vec<&str> = got.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["alpha", "broken", "zeta"]); // sorted, .json skipped
        let alpha = got.iter().find(|r| r.name == "alpha").unwrap();
        assert_eq!(alpha.workflow_name.as_deref(), Some("demo"));
        assert_eq!(alpha.description.as_deref(), Some("a demo recipe"));
        let broken = got.iter().find(|r| r.name == "broken").unwrap();
        assert!(broken.workflow_name.is_none()); // unparseable → no metadata, still listed
    }

    #[test]
    fn list_missing_dir_is_empty_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(list(&missing).unwrap().is_empty());
    }

    #[test]
    fn is_plain_name_is_stricter_than_is_valid_name() {
        assert!(is_plain_name("my-thing"));
        assert!(is_plain_name("a.b_c-1"));
        // is_valid_name permits these YAML-indicator names; is_plain_name must not.
        assert!(is_valid_name("#wip") && !is_plain_name("#wip"));
        assert!(is_valid_name("a b") && !is_plain_name("a b"));
        assert!(is_valid_name("*") && !is_plain_name("*"));
        assert!(!is_plain_name("..") && !is_plain_name("a/b"));
    }

    #[test]
    fn resolve_source_file_then_catalog_then_starter() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_dir = tmp.path().join("recipes");
        std::fs::create_dir_all(&catalog_dir).unwrap();

        // A bundled starter resolves on a fresh (empty) catalog.
        let s = resolve_source("local-review", Some(&catalog_dir)).unwrap();
        assert!(s.provenance.starts_with("built-in starter"));
        assert!(s.body.contains("name: local-review"));

        // A catalog recipe of the same name wins over the starter.
        write(
            &catalog_dir,
            "local-review.yaml",
            "name: local-review\nsteps:\n  - {id: s, run: x}\n",
        );
        let s = resolve_source("local-review", Some(&catalog_dir)).unwrap();
        assert!(s.provenance.starts_with("recipe"));

        // An existing file path wins over everything.
        let file = tmp.path().join("explicit.yaml");
        std::fs::write(&file, WF).unwrap();
        let s = resolve_source(file.to_str().unwrap(), Some(&catalog_dir)).unwrap();
        assert!(s.provenance.starts_with("file"));

        // A miss lists both recipes and starters.
        let err = resolve_source("does-not-exist", Some(&catalog_dir))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("recipes:") && err.contains("starters:"),
            "got: {err}"
        );
    }

    #[test]
    fn rewrite_workflow_name_round_trips() {
        let out = rewrite_workflow_name(WF, "renamed").unwrap();
        let wf = odin_core::Workflow::from_yaml_str(&out).unwrap();
        assert_eq!(wf.name.as_str(), "renamed");
        // description and steps survive the rewrite.
        assert_eq!(wf.description.as_deref(), Some("a demo recipe"));
        assert_eq!(wf.steps.len(), 1);
        // A source with no top-level name: is refused.
        assert!(rewrite_workflow_name("steps:\n  - {id: s, run: x}\n", "x").is_err());
    }

    #[test]
    fn list_reads_normalized_tags() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "tagged.yaml",
            "name: t\ntags: [Review, CI]\nsteps:\n  - id: s\n    run: \"echo hi\"\n",
        );
        write(tmp.path(), "untagged.yaml", WF);
        let got = list(tmp.path()).unwrap();
        let tagged = got.iter().find(|r| r.name == "tagged").unwrap();
        assert_eq!(tagged.tags, ["review", "ci"]); // IR-normalized to lowercase
        let untagged = got.iter().find(|r| r.name == "untagged").unwrap();
        assert!(untagged.tags.is_empty());
    }

    #[test]
    fn starters_all_parse_and_names_unique() {
        assert!(!STARTERS.is_empty());
        let mut seen = std::collections::HashSet::new();
        for (name, body) in STARTERS {
            assert!(
                is_valid_name(name),
                "starter name {name} is not a valid catalog name"
            );
            assert!(seen.insert(*name), "duplicate starter name {name}");
            Workflow::from_yaml_str(body)
                .unwrap_or_else(|e| panic!("bundled starter {name} does not parse: {e}"));
        }
    }

    #[test]
    fn resolve_arg_prefers_file_then_recipe() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_dir = tmp.path().join("recipes");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        write(&catalog_dir, "by-name.yaml", WF);

        // A real file path is returned as-is.
        let file = tmp.path().join("explicit.yaml");
        std::fs::write(&file, WF).unwrap();
        assert_eq!(resolve_arg(&file, Some(&catalog_dir)).unwrap(), file);

        // A bare name resolves against the catalog.
        assert_eq!(
            resolve_arg(Path::new("by-name"), Some(&catalog_dir)).unwrap(),
            catalog_dir.join("by-name.yaml")
        );

        // Neither a file nor a recipe → error.
        assert!(resolve_arg(Path::new("nope"), Some(&catalog_dir)).is_err());
    }

    #[test]
    fn seed_writes_then_skips_unless_forced() {
        let tmp = tempfile::tempdir().unwrap();
        let (wrote, skipped) = seed(tmp.path(), false).unwrap();
        assert_eq!(wrote.len(), STARTERS.len());
        assert!(skipped.is_empty());
        // Every seeded starter is now resolvable by name.
        assert!(resolve(tmp.path(), STARTERS[0].0).is_some());

        // A second seed skips everything (idempotent)…
        let (wrote2, skipped2) = seed(tmp.path(), false).unwrap();
        assert!(wrote2.is_empty());
        assert_eq!(skipped2.len(), STARTERS.len());

        // …unless forced, which rewrites them all.
        let (wrote3, skipped3) = seed(tmp.path(), true).unwrap();
        assert_eq!(wrote3.len(), STARTERS.len());
        assert!(skipped3.is_empty());
    }
}
