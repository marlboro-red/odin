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

/// A recipe as listed: its catalog key (filename stem), path, and best-effort parsed metadata.
/// `workflow_name`/`description` are `None` if the file does not parse as a workflow.
pub(crate) struct Recipe {
    pub name: String,
    pub path: PathBuf,
    pub workflow_name: Option<String>,
    pub description: Option<String>,
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
            let (workflow_name, description) = match Workflow::from_yaml_path(&path) {
                Ok(wf) => (Some(wf.name.to_string()), wf.description),
                Err(_) => (None, None),
            };
            Recipe {
                name,
                path,
                workflow_name,
                description,
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
}
