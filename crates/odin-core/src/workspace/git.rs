//! A small async wrapper around the `git` CLI, returning [`WorkspaceError`].
//!
//! The workspace providers shell out to `git` rather than linking libgit2: worktree,
//! local-clone, reset, and clean are simpler and more robust via the CLI, and `git` is
//! already a hard dependency of the project.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use crate::error::WorkspaceError;

/// Runs `git <args>` in `cwd`, returning captured stdout on success.
///
/// # Errors
/// Returns [`WorkspaceError::Git`] if `git` is missing or exits non-zero, or
/// [`WorkspaceError::Io`] for other spawn failures.
pub(crate) async fn git(cwd: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .envs(crate::provider::process::GIT_PORTABLE_ENV.iter().copied())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                WorkspaceError::Git("`git` was not found on PATH".to_owned())
            }
            _ => WorkspaceError::Io(e),
        })?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(WorkspaceError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
