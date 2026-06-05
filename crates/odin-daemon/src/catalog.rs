//! Bin-local recipe-catalog **directory resolution** for `odind`.
//!
//! This is a deliberate, byte-for-byte copy of `odin-cli`'s `catalog::dir` resolution (the
//! `--recipes-dir` > `$ODIN_RECIPES_DIR` > platform data-local precedence, including the
//! empty-env-var-falls-through subtlety). The two MUST stay in sync — a recipe runnable via
//! `odin run <name>` should be the same file `odind --recipes` loads. It lives in `main.rs`'s
//! module tree (not the `odin_daemon` lib) because `main.rs` is a separate crate that cannot see
//! a `pub(crate)` lib item. Cross-reference: `crates/odin-cli/src/catalog.rs`.

use std::path::{Path, PathBuf};

use anyhow::anyhow;

/// Environment variable that overrides the catalog directory (below an explicit `--recipes-dir`).
pub(crate) const RECIPES_DIR_ENV: &str = "ODIN_RECIPES_DIR";

/// Resolves the recipe-catalog directory: `--recipes-dir` (here `override_dir`) >
/// `$ODIN_RECIPES_DIR` > `directories::ProjectDirs::data_local_dir()` + `recipes`.
///
/// # Errors
/// Fails only in the fallback case when no home/data directory can be determined for the platform.
pub(crate) fn dir(override_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(d) = override_dir {
        return Ok(d.to_path_buf());
    }
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
