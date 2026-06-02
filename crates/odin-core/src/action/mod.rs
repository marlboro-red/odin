//! Built-in [`crate::traits::Action`]s — named, reusable side effects available in
//! `action:` steps.
//!
//! - [`ShellExec`] (`shell.exec`) — run a shell command.
//! - [`GitCommit`] (`git.commit`) — stage and commit the workspace.
//! - [`GitPush`] (`git.push`) — push the run's branch.
//! - [`OpenPr`] (`github.open_pr`) — open a pull request via the `gh` CLI.
//!
//! They shell out to `git` / `gh` / `sh` rather than linking libraries; the executor
//! runs them in the run's workspace and folds their [`crate::api::SideEffect`]s into the
//! [`crate::api::RunSummary`].

mod git;
mod github;
mod shell;

pub use git::{GitCommit, GitPush};
pub use github::OpenPr;
pub use shell::ShellExec;

use std::path::Path;

use indexmap::IndexMap;
use serde_json::Value;

use crate::error::ActionError;
use crate::provider::{ProcessOptions, ProcessOutput, run_process};
use crate::traits::CancelToken;

/// Runs `program args...` in `workdir`, mapping a spawn failure to an [`ActionError`].
async fn exec(program: &str, args: &[&str], workdir: &Path) -> Result<ProcessOutput, ActionError> {
    let owned: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    let opts = ProcessOptions {
        workdir: Some(workdir.to_path_buf()),
        ..ProcessOptions::default()
    };
    run_process(program, &owned, &opts, &CancelToken::new())
        .await
        .map_err(|e| ActionError::Other(anyhow::anyhow!("{e}")))
}

/// Like [`exec`] but requires exit code 0, returning stdout.
async fn checked(program: &str, args: &[&str], workdir: &Path) -> Result<String, ActionError> {
    let out = exec(program, args, workdir).await?;
    if out.exit_code == 0 {
        Ok(out.stdout)
    } else {
        Err(ActionError::Other(anyhow::anyhow!(
            "{program} {} failed: {}",
            args.join(" "),
            out.stderr.trim()
        )))
    }
}

/// Extracts a required string argument.
fn arg_str<'a>(args: &'a IndexMap<String, Value>, key: &str) -> Result<&'a str, ActionError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ActionError::Other(anyhow::anyhow!("missing required arg `{key}`")))
}

/// Extracts an optional string argument, falling back to `default`.
fn arg_str_or(args: &IndexMap<String, Value>, key: &str, default: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_owned()
}
