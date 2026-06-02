//! # odin-core
//!
//! `odin-core` is the embeddable heart of **Odin**, a durable workflow engine that
//! orchestrates autonomous coding-agent CLIs (Claude Code, OpenAI Codex, GitHub
//! Copilot CLI) to perform software-engineering work without supervision.
//!
//! The crate is *library-first*: the `odin` CLI and the `odind` daemon are thin
//! runners built on top of the types and traits defined here. Everything an
//! integrator needs to embed Odin in another program — define workflows, plug in
//! new providers/workspaces/actions, and drive runs — lives in this crate.
//!
//! ## Architecture at a glance
//!
//! ```text
//! Foundation : Workflow IR · Diagnostics · Templating · Errors
//! Pluggable  : Provider · Workspace · Store · Action · Trigger   (integration surface)
//! Execution  : Step exec · Gates · Judge · Retry · Concurrency
//! Engine     : Run lifecycle · Scheduler · Durable resume
//! ```
//!
//! ## Embedding Odin
//!
//! Build an engine over a git repo, register any custom plugins, and drive workflows. The
//! returned [`api::RunSummary`] is plain, serializable data — no engine internals. (This
//! example is a compiled doctest, so it stays in sync with the API; see
//! `docs/integration-guide.md` for the full guide.)
//!
//! ```no_run
//! use std::sync::Arc;
//! use odin_core::{EngineBuilder, RunInput, RunStatus, SqliteStore, Workflow};
//!
//! # async fn embed() -> odin_core::Result<()> {
//! // An engine over a git repo, checkpointing to SQLite for crash-resume.
//! let engine = EngineBuilder::new()
//!     .repo("/path/to/repo")
//!     .store(Arc::new(SqliteStore::open("runs.db")?))
//!     .build()?;
//!
//! // Load and run a workflow; pass typed inputs via `RunInput`.
//! let workflow = Workflow::from_yaml_str("name: demo\nsteps: [{ id: greet, run: \"echo hi\" }]\n")?;
//! let summary = engine.run(&workflow, RunInput::manual().param("who", "world")).await?;
//!
//! if summary.status == RunStatus::Succeeded {
//!     println!("{} step(s), ${:.4}", summary.steps.len(), summary.usage.cost_usd());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Feature flags
//!
//! - `ir` — parse + validate workflows. No async runtime, no templating engine.
//! - `templating` — render prompts/conditionals and statically check template refs.
//! - `runtime` — the five integration traits, the registry, and the provider/store/
//!   workspace/action implementations.
//! - `mock` — in-memory test doubles ([`mock::EchoProvider`], [`mock::MemStore`], …) for
//!   downstream tests. Opt-in; *not* included in `full`.
//! - `full` (default) — `ir` + `templating` + `runtime` (not `mock`).
//!
//! The [`Engine`] façade requires **both** `runtime` and `templating` (it renders prompts),
//! so it is available under `full`.
//!
//! A parse-only embedder (a linter or LSP) can depend on `odin-core` with
//! `default-features = false, features = ["ir"]` and pull in none of `tokio`.
#![doc(html_root_url = "https://docs.rs/odin-core/0.0.1")]

#[cfg(feature = "runtime")]
pub mod action;
pub mod api;
#[cfg(feature = "templating")]
pub mod context;
pub mod error;
pub mod ids;
pub mod ir;
pub mod usage;
pub mod validate;

#[cfg(all(feature = "runtime", feature = "templating"))]
pub mod engine;
#[cfg(all(feature = "runtime", any(test, feature = "mock")))]
pub mod mock;
#[cfg(feature = "runtime")]
pub mod provider;
#[cfg(feature = "runtime")]
pub mod registry;
#[cfg(feature = "runtime")]
pub mod storage;
#[cfg(feature = "runtime")]
pub mod traits;
#[cfg(feature = "runtime")]
pub mod workspace;

pub use api::{RunInput, RunStatus, RunSummary, SideEffect, StepResult, StepStatus};
pub use error::{
    ActionError, Error, ProviderError, Result, StoreError, TriggerError, WorkspaceError,
};
pub use ids::{ArtifactName, GateName, ParamName, ProviderRef, RunId, StepId, WorkflowId};
pub use ir::{
    Backoff, ParamSpec, ParamType, ResetMode, RetrySpec, SchemaVersion, Step, StepKind, Workflow,
    WorkspaceConfig,
};
pub use usage::Usage;
pub use validate::{
    DiagCode, Diagnostic, KnownNames, Severity, ValidationReport, validate, validate_source,
};

#[cfg(feature = "runtime")]
pub use action::{GitCommit, GitPush, OpenPr, ShellExec};
#[cfg(all(feature = "runtime", feature = "templating"))]
pub use engine::{Engine, EngineBuilder};
#[cfg(feature = "runtime")]
pub use provider::{ClaudeProvider, CodexProvider, CopilotProvider};
#[cfg(feature = "runtime")]
pub use registry::Registry;
#[cfg(feature = "runtime")]
pub use storage::SqliteStore;
#[cfg(feature = "runtime")]
pub use traits::{Action, Provider, Store, Trigger, Workspace};
#[cfg(feature = "runtime")]
pub use workspace::{SlotPoolWorkspace, WorktreeWorkspace};

/// The version string of `odin-core`, taken from `Cargo.toml` at build time.
///
/// Useful for stamping run records so a durable run can be correlated with the
/// engine version that produced it.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_reported() {
        assert!(!version().is_empty());
    }
}
