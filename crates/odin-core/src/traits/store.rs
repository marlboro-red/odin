//! The [`Store`] trait: durable, crash-resumable persistence of run state.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::workspace::WorkspaceHandle;
use crate::api::{RunInput, RunStatus, StepStatus};
use crate::error::StoreError;
use crate::ids::{ArtifactName, RunId, StepId, WorkflowId};
use crate::usage::Usage;

/// Durable persistence for run state. The SQLite implementation lands in a later
/// milestone; this trait is fixed now.
///
/// The contract is deliberately tiny: **checkpoint** the whole [`RunState`] at step
/// boundaries, **append** events to an audit log, and **load incomplete** runs on
/// startup so they resume. `RunState` is `Serialize`, so a backend can persist it as an
/// opaque blob with zero knowledge of the IR.
#[async_trait]
pub trait Store: Send + Sync {
    /// Persists a run-state checkpoint atomically. Called at every step boundary; a
    /// crash mid-call must leave either the old or the new state, never a partial one.
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend write fails.
    async fn checkpoint(&self, state: &RunState) -> Result<(), StoreError>;

    /// Appends one immutable event to the run's ordered audit log (cheap, frequent).
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend write fails.
    async fn append_event(&self, run_id: RunId, event: &RunEvent) -> Result<(), StoreError>;

    /// Loads all runs not in a terminal state — the crash-recovery entry point.
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend read fails.
    async fn load_incomplete(&self) -> Result<Vec<RunState>, StoreError>;

    /// Loads the most-recently-updated runs (newest first), up to `limit`, for listing.
    /// Defaults to empty, so listing is an optional capability.
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend read fails.
    async fn recent(&self, limit: usize) -> Result<Vec<RunState>, StoreError> {
        let _ = limit;
        Ok(Vec::new())
    }

    /// Loads a single run by id (`None` if unknown).
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend read fails.
    async fn load_run(&self, run_id: RunId) -> Result<Option<RunState>, StoreError>;

    /// Reads the event log for a run (for replay/inspection). Defaults to empty, so an
    /// audit log is an optional capability.
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend read fails.
    async fn events(&self, _run_id: RunId) -> Result<Vec<RunEvent>, StoreError> {
        Ok(Vec::new())
    }
}

/// The full durable state of a run — the checkpoint payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunState {
    /// Run identity.
    pub run_id: RunId,
    /// Which workflow this run executes.
    pub workflow: WorkflowId,
    /// Schema major the workflow declared (reproducibility).
    pub schema_major: u16,
    /// Overall status.
    pub status: RunStatus,
    /// Terminal error string, if the run failed. `None` while running or on success.
    #[serde(default)]
    pub error: Option<String>,
    /// Per-step progress, keyed by step id, in execution order.
    pub steps: IndexMap<StepId, StepState>,
    /// Resolved artifact catalogue: name → path relative to the workdir.
    pub artifacts: IndexMap<ArtifactName, String>,
    /// Provider versions actually used (reproducibility).
    pub provider_versions: IndexMap<String, String>,
    /// The inputs the run started with (deterministic resume & audit).
    pub input: RunInput,
    /// Workspace lease in use, to reattach on resume.
    pub workspace: Option<WorkspaceHandle>,
    /// When the run was created.
    pub created_at: DateTime<Utc>,
    /// When the run state was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Per-step durable progress.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StepState {
    /// Step status.
    pub status: StepStatus,
    /// Attempts so far (`>= 1` once started).
    pub attempts: u8,
    /// Last process exit code, if the step ran.
    pub exit_code: Option<i32>,
    /// Named outputs exposed to templating as `steps.<id>.outputs.*`.
    pub outputs: IndexMap<String, serde_json::Value>,
    /// Usage for this step's invocations.
    pub usage: Option<Usage>,
    /// Gate name → passed, for the step's last attempt.
    #[serde(default)]
    pub gates: IndexMap<String, bool>,
    /// LLM-as-judge score, if a judge ran.
    #[serde(default)]
    pub judge_score: Option<f32>,
}

/// An immutable audit-log entry. Both the enum and each variant are `#[non_exhaustive]`,
/// so new event kinds *and* new fields on existing kinds are additive — a `Store` that
/// matches with `..` keeps compiling.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunEvent {
    /// The run started.
    #[non_exhaustive]
    RunStarted {
        /// When.
        at: DateTime<Utc>,
    },
    /// A step started an attempt.
    #[non_exhaustive]
    StepStarted {
        /// Which step.
        step: StepId,
        /// Attempt number.
        attempt: u8,
        /// When.
        at: DateTime<Utc>,
    },
    /// A gate finished.
    #[non_exhaustive]
    GateResult {
        /// Which step.
        step: StepId,
        /// Gate name.
        gate: String,
        /// Whether it passed.
        passed: bool,
        /// When.
        at: DateTime<Utc>,
    },
    /// A judge finished.
    #[non_exhaustive]
    JudgeResult {
        /// Which step.
        step: StepId,
        /// Judge score.
        score: f32,
        /// Whether it passed the threshold.
        passed: bool,
        /// When.
        at: DateTime<Utc>,
    },
    /// A step finished an attempt.
    #[non_exhaustive]
    StepFinished {
        /// Which step.
        step: StepId,
        /// Resulting status.
        status: StepStatus,
        /// Exit code, if any.
        exit_code: Option<i32>,
        /// When.
        at: DateTime<Utc>,
    },
    /// The run finished.
    #[non_exhaustive]
    RunFinished {
        /// Terminal status.
        status: RunStatus,
        /// When.
        at: DateTime<Utc>,
    },
}
