//! The [`Store`] trait: durable, crash-resumable persistence of run state.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::workspace::WorkspaceHandle;
use crate::api::{ApprovalDecision, RunInput, RunStatus, SideEffect, StepStatus};
use crate::error::StoreError;
use crate::ids::{ArtifactName, RunId, StepId, WorkflowId};
use crate::usage::Usage;

/// Durable persistence for run state. The built-in SQLite implementation is
/// [`crate::storage::SqliteStore`]; this trait is the pluggable contract.
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

    /// Requests cancellation of a run from **another process** (e.g. the CLI `odin cancel` against
    /// a run executing in the daemon): records a durable cancel signal that the executing engine's
    /// per-run watcher polls via [`is_cancel_requested`](Store::is_cancel_requested). Returns `true`
    /// iff a non-terminal run with `run_id` existed and was marked (so the caller can report "no
    /// such cancellable run"). The default returns `false` — a store without a cancel-signal table
    /// can't carry a cross-process cancel.
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend write fails.
    async fn request_cancel(&self, run_id: RunId) -> Result<bool, StoreError> {
        let _ = run_id;
        Ok(false)
    }

    /// Whether a cross-process [`request_cancel`](Store::request_cancel) is pending for `run_id`.
    /// The engine polls this for each in-flight durable run and fires its cancel token when set.
    /// Defaults to `false`.
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend read fails.
    async fn is_cancel_requested(&self, run_id: RunId) -> Result<bool, StoreError> {
        let _ = run_id;
        Ok(false)
    }

    /// Atomically claims an `awaiting_approval` run for resumption, flipping its status column to
    /// `running` and returning `true` iff **this** caller won the flip (the row existed and was
    /// `awaiting_approval`). Lets two processes that share one store — e.g. the CLI `odin approve`
    /// and the daemon's HTTP `/approve` — fence a decision: only the winner resumes the run, so its
    /// downstream side effects run once. The default implementation returns `true` (no
    /// cross-process compare-and-swap); a backend with atomic updates should override it. The
    /// caller still re-checks the loaded state's status, so a `true` default stays correct
    /// in-process.
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend write fails.
    async fn claim_awaiting(&self, run_id: RunId) -> Result<bool, StoreError> {
        let _ = run_id;
        Ok(true)
    }

    /// Reads the event log for a run (for replay/inspection). Defaults to empty, so an
    /// audit log is an optional capability.
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend read fails.
    async fn events(&self, _run_id: RunId) -> Result<Vec<RunEvent>, StoreError> {
        Ok(Vec::new())
    }

    /// A cheap aggregate snapshot for operational metrics (e.g. a Prometheus `/metrics`
    /// endpoint): run counts grouped by workflow and status. Defaults to empty, so metrics are
    /// an optional capability.
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend read fails.
    async fn metrics(&self) -> Result<StoreMetrics, StoreError> {
        Ok(StoreMetrics::default())
    }

    /// Deletes **terminal** runs (and their events) matching `policy`, returning what would be /
    /// was removed. Non-terminal runs (`pending`/`running`/`awaiting_approval`) are NEVER
    /// touched — they are mid-flight or awaiting a human decision. With `dry_run`, selects the
    /// eligible runs and reports them but deletes nothing. Defaults to a no-op, so retention is
    /// an optional capability.
    ///
    /// An implementation MUST keep the metrics counter sound under deletion (so a pruned run
    /// still counts toward `odin_runs_total`); the SQLite store folds the pruned counts into a
    /// persistent tally before deleting. The returned [`PruneReport::run_ids`] let the caller
    /// reclaim each run's external state (e.g. git snapshot refs).
    ///
    /// # Errors
    /// Returns a [`StoreError`] if the backend read/write fails.
    async fn prune(&self, policy: &PrunePolicy, dry_run: bool) -> Result<PruneReport, StoreError> {
        let _ = (policy, dry_run);
        Ok(PruneReport::default())
    }
}

/// A retention policy for [`Store::prune`]. A run is eligible only if it is **terminal** AND
/// satisfies every limit that is set (the limits are AND-combined — the conservative reading).
/// The [`Default`] is a no-op (`max_age`/`keep_last` both `None`): a misconfigured policy
/// deletes nothing.
#[derive(Clone, Debug, Default)]
pub struct PrunePolicy {
    /// Prune terminal runs last updated longer ago than this. `None` = no age limit.
    pub max_age: Option<chrono::Duration>,
    /// Keep at most this many terminal runs **per workflow** (newest by `updated_at`); prune the
    /// rest. `None` = no count limit.
    pub keep_last: Option<u32>,
    /// Restrict pruning to this one workflow; `None` prunes across all workflows.
    pub workflow: Option<WorkflowId>,
}

impl PrunePolicy {
    /// Whether the policy has no age or count limit set — in which case [`Store::prune`] deletes
    /// nothing (callers should refuse such a policy rather than silently no-op).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.max_age.is_none() && self.keep_last.is_none()
    }
}

/// What a [`Store::prune`] removed (or, under `dry_run`, would remove).
#[derive(Clone, Debug, Default, Serialize)]
#[non_exhaustive]
pub struct PruneReport {
    /// Number of `runs` rows deleted (or eligible, under `dry_run`).
    pub runs_pruned: u64,
    /// Number of `events` rows deleted (or eligible, under `dry_run`).
    pub events_pruned: u64,
    /// Per-(workflow, status) breakdown of what was pruned, for logging/JSON.
    pub per_workflow: Vec<PrunedCount>,
    /// The pruned run ids, so the caller can reclaim each run's external state (snapshot refs).
    pub run_ids: Vec<RunId>,
    /// Whether this was a dry run (nothing was actually deleted).
    pub dry_run: bool,
}

/// One line of a [`PruneReport`]: how many runs of a (workflow, status) were pruned.
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct PrunedCount {
    /// The workflow name.
    pub workflow: String,
    /// The terminal run status (lowercase serde string).
    pub status: String,
    /// How many runs of this (workflow, status) were pruned.
    pub count: u64,
}

/// A cheap aggregate snapshot of run state for operational metrics (e.g. a Prometheus
/// `/metrics` endpoint): the number of runs in each (workflow, status) group.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct StoreMetrics {
    /// One entry per (workflow, status) group present in the store.
    pub runs: Vec<RunStatusCount>,
}

impl StoreMetrics {
    /// Builds a snapshot from per-`(workflow, status)` counts.
    #[must_use]
    pub fn new(runs: Vec<RunStatusCount>) -> Self {
        Self { runs }
    }
}

/// The number of runs of one workflow in one status. `status` is the lowercase serde string
/// (`"succeeded"`, `"running"`, `"awaiting_approval"`, …).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RunStatusCount {
    /// The workflow name.
    pub workflow: String,
    /// The run status, as its lowercase serde string.
    pub status: String,
    /// How many runs are in this (workflow, status) group.
    pub count: u64,
}

impl RunStatusCount {
    /// Builds one `(workflow, status)` count.
    #[must_use]
    pub fn new(workflow: impl Into<String>, status: impl Into<String>, count: u64) -> Self {
        Self {
            workflow: workflow.into(),
            status: status.into(),
            count,
        }
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
    /// Reserved for reproducibility (provider CLI/model versions used). **Not yet populated**
    /// by the engine — `Provider::version` is not currently captured into run state.
    pub provider_versions: IndexMap<String, String>,
    /// The inputs the run started with (deterministic resume & audit).
    pub input: RunInput,
    /// Human decisions recorded for `approval` gates, keyed by step id. Consulted when the
    /// engine reaches a gate on resume: an approved gate proceeds, a rejected one fails
    /// (carrying the note as feedback). Empty until a gate is decided.
    #[serde(default)]
    pub approvals: IndexMap<StepId, ApprovalDecision>,
    /// Workspace lease in use, to reattach on resume.
    pub workspace: Option<WorkspaceHandle>,
    /// The commit the run's workspace started at; `DIFF` and snapshots are taken against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Latest per-step workspace snapshot commit (off-branch). On resume the workdir is
    /// restored to it so a step interrupted mid-edit re-runs from a clean state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Per-iteration progress of any in-flight `loop:` steps, keyed by loop step id (see
    /// [`LoopProgress`]). Empty unless a loop is mid-flight; cleared when each loop settles.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub loop_state: IndexMap<StepId, LoopProgress>,
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
    /// Outward effects the step recorded (a PR opened, a branch pushed, a commit, a comment, an
    /// artifact). Persisted so a resumed run can reconstruct the full set without re-running the
    /// already-finished steps that produced them — otherwise a crash would silently drop every
    /// side effect from before it. Empty for steps that produced none.
    #[serde(default)]
    pub side_effects: Vec<SideEffect>,
    /// Why the step failed (exit code + stderr tail, a failed gate, a sub-threshold judge, a
    /// provider/action error) — or, for a `Skipped` step, why it was skipped (an upstream
    /// dependency failed). Persisted so a failed run is debuggable. `None` for a passed step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Per-iteration durable progress of a running `loop:` step, so a crash mid-loop resumes from the
/// last completed iteration rather than re-running the whole loop. Keyed by the loop step's id in
/// [`RunState::loop_state`]; present only while the loop runs, cleared when it settles.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LoopProgress {
    /// Iterations that fully completed (1-based count); resume re-enters at the next one.
    pub last_completed_iteration: u32,
    /// Off-branch snapshot of the workspace as of `last_completed_iteration`, anchored by
    /// `refs/odin/loop/<run>/<loop-id>`. `None` when no clean snapshot exists (an inner step
    /// committed, moving HEAD off base) — then resume restarts the loop from iteration 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration_snapshot: Option<String>,
    /// The `loop.feedback` to seed the next iteration with (the prior iteration's failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
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
        /// Why it failed, if it did (mirrors `StepState.error`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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
