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
//! - `mock` — in-memory test doubles (`mock::EchoProvider`, `mock::MemStore`, …) for
//!   downstream tests. Opt-in; *not* included in `full` — so these are plain code spans, not
//!   intra-doc links (the `mock` module is absent from a default-feature doc build).
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
#[cfg(feature = "telemetry")]
pub mod telemetry;
#[cfg(feature = "runtime")]
pub mod traits;
#[cfg(feature = "runtime")]
pub mod view;
#[cfg(feature = "runtime")]
pub mod workspace;

pub use api::{
    ApprovalDecision, Decision, RerunOutcome, RunInput, RunStatus, RunSummary, SideEffect,
    StepResult, StepStatus,
};
pub use error::{
    ActionError, Error, ProviderError, Result, StoreError, TriggerError, WorkspaceError,
};
pub use ids::{ArtifactName, GateName, ParamName, ProviderRef, RunId, StepId, WorkflowId};
pub use ir::{
    ApprovalStep, Backoff, CaseBranch, CaseStep, FeedbackMode, LoopStep, ParamSpec, ParamType,
    ResetMode, RetrySpec, SchemaVersion, Step, StepKind, Workflow, WorkspaceConfig,
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
// The context/outcome structs a trait implementor exchanges with the engine — re-exported at
// the crate root so a plugin author imports everything from `odin_core::` (not a mix of
// `odin_core::` and `odin_core::traits::`).
#[cfg(feature = "runtime")]
pub use traits::{
    AcquireCtx, ActionCtx, ActionOutcome, CancelToken, InvocationCtx, InvocationOutcome,
    PrunePolicy, PruneReport, PrunedCount, RunEvent, RunState, RunStatusCount, StepState,
    StoreMetrics, TriggerEvent, WorkspaceHandle,
};
#[cfg(feature = "runtime")]
pub use view::{GateView, RunDetailView, RunView, StepView};
#[cfg(feature = "runtime")]
pub use workspace::{SlotPoolWorkspace, WorktreeWorkspace};

/// Re-export of the [`mod@async_trait`] attribute macro. Every integration trait is
/// `#[async_trait]`, so implementors annotate their `impl` with it — use
/// `odin_core::async_trait` to get a version guaranteed to match this crate's, instead of
/// adding (and version-matching) the `async-trait` dependency yourself.
#[cfg(feature = "runtime")]
pub use async_trait::async_trait;

/// Re-export of [`mod@serde_json`]. The public API exchanges `serde_json::Value` (param values,
/// the trigger payload, step outputs, action `with:` args), so embedders can build those —
/// `odin_core::serde_json::json!(…)` — without depending on a possibly-mismatched `serde_json`.
pub use serde_json;

/// Re-export of [`mod@anyhow`]. Every integration trait's error type has an
/// `Other(#[from] anyhow::Error)` variant, so a custom `Provider`/`Action`/… impl wraps its
/// own errors through `anyhow` — use `odin_core::anyhow` to match this crate's version.
#[cfg(feature = "runtime")]
pub use anyhow;

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
