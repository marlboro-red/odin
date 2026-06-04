//! Error taxonomy.
//!
//! The crate-level [`enum@Error`] is organized by *phase* (parse → validate → run). Each
//! integration trait has its **own** small error type ([`ProviderError`], …) so a
//! third-party implementor returns a focused 3–4 variant enum rather than a crate-wide
//! god-error. The crate `Error` `#[from]`-wraps each of them.

use thiserror::Error;

/// Convenience alias for the crate-level [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// The crate-level error, organized by the phase in which it arises.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// YAML/serde parse failure (syntax, unknown leaf field, bad enum/duration).
    #[error("failed to parse workflow: {0}")]
    Parse(#[from] serde_yaml_ng::Error),

    /// I/O failure (e.g. reading a workflow file).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The workflow parsed but failed semantic validation. Carries ALL diagnostics.
    #[error("workflow validation failed with {} error(s)", .0.error_count())]
    Validation(crate::validate::ValidationReport),

    /// An unsupported schema major version was declared.
    #[error(
        "unsupported schema_version major {found_major} \
         (this engine speaks major {supported_major})"
    )]
    SchemaVersion {
        /// The major the file declared.
        found_major: u16,
        /// The major this engine understands.
        supported_major: u16,
    },

    /// A name referenced in a workflow had no registered implementation.
    #[error("no {kind} registered under name '{name}'")]
    Unregistered {
        /// What kind of plugin was missing (`"provider"`, `"action"`, …).
        kind: &'static str,
        /// The unresolved name.
        name: String,
    },

    /// A [`crate::api::RunInput`] did not satisfy the workflow's declared params.
    #[error("invalid run input: {0}")]
    Input(String),

    /// A frozen-API surface whose implementation is a later milestone was invoked.
    #[error("{feature} is not implemented yet")]
    Unimplemented {
        /// The not-yet-implemented feature.
        feature: &'static str,
    },

    /// Template render/eval failure at run time (only with the `templating` feature).
    #[cfg(feature = "templating")]
    #[error("template error in {context}: {source}")]
    Template {
        /// Where the template came from (e.g. `step "review" prompt`).
        context: String,
        /// The underlying minijinja error.
        #[source]
        source: minijinja::Error,
    },

    /// A [`crate::traits::Provider`] failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// A [`crate::traits::Workspace`] failed.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// A [`crate::traits::Store`] failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A [`crate::traits::Action`] failed.
    #[error(transparent)]
    Action(#[from] ActionError),
    /// A [`crate::traits::Trigger`] failed.
    #[error(transparent)]
    Trigger(#[from] TriggerError),
}

/// Error returned by a [`crate::traits::Provider`] invocation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// The provider CLI was not found on `PATH`.
    #[error("provider CLI not found on PATH: {0}")]
    NotFound(String),
    /// No POSIX shell could be resolved to run `run:` / gate / `shell.exec` commands.
    #[error("{0}")]
    ShellNotFound(String),
    /// The provider exceeded its timeout.
    #[error("provider timed out after {0:?}")]
    Timeout(std::time::Duration),
    /// The provider process exited non-zero.
    #[error("provider exited with code {code}: {stderr}")]
    Exited {
        /// The process exit code.
        code: i32,
        /// Captured stderr (truncated by the caller as needed).
        stderr: String,
    },
    /// Any other provider-specific failure.
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Error returned by a [`crate::traits::Workspace`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// A git operation failed.
    #[error("git error: {0}")]
    Git(String),
    /// A finite slot pool had no free slot.
    #[error("no free slot in pool")]
    PoolExhausted,
    /// An I/O failure while provisioning the workspace.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Any other workspace-specific failure.
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Error returned by a [`crate::traits::Store`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The backend (SQLite, etc.) reported an error.
    #[error("store backend error: {0}")]
    Backend(String),
    /// Serializing/deserializing run state failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// Any other store-specific failure.
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Error returned by a [`crate::traits::Action`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ActionError {
    /// The action name is not registered.
    #[error("unknown action: {0}")]
    Unknown(String),
    /// Any other action-specific failure.
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Error returned by a [`crate::traits::Trigger`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TriggerError {
    /// The underlying event source failed.
    #[error("trigger source error: {0}")]
    Source(String),
    /// Any other trigger-specific failure.
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
