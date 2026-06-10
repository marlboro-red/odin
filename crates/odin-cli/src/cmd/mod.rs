//! Subcommand implementations for the `odin` CLI.

use std::path::{Path, PathBuf};

/// The step-log spool directory for a given state database — `<db-parent>/logs` (so it sits beside
/// `state.db`, e.g. `<repo>/.odin/logs`). The single source of truth shared by `run`, `prune`, and
/// `approve`/`reject` so they all spool to / clean up the SAME place (the daemon mirrors this).
pub(crate) fn logs_dir_for(db: &Path) -> Option<PathBuf> {
    db.parent().map(|p| p.join("logs"))
}

pub(crate) mod approval;
pub(crate) mod cancel;
pub(crate) mod inspect;
pub(crate) mod prune;
pub(crate) mod recipe;
pub(crate) mod run;
pub(crate) mod status;
pub(crate) mod validate;
