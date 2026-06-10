//! The built-in executor.
//!
//! [`LocalEngine`] walks a workflow's dependency graph. For each step it builds the
//! template context from the run state so far, evaluates `when`, renders the prompt /
//! command / args, dispatches to the pinned provider, a shell `run:` hook, or a
//! registered action, runs the gates, auto-captures the git `DIFF`, and checkpoints the
//! [`RunState`] — so a crashed run can be resumed from the last completed step. A step
//! that declares a `judge:` is scored by that provider and fails below its threshold; a
//! failed step is retried per its `retry:` policy with backoff.
//!
//! ## Concurrency
//!
//! By default (`max_parallel` unset or `1`) steps run one at a time. With `max_parallel >
//! 1` the executor is a bounded ready-set scheduler: a non-`scratch` step mutates the
//! single shared workspace and so runs **exclusively** (never beside another step), while
//! `scratch: true` steps run **concurrently** in isolated throwaway worktrees (their edits
//! never touch the shared tree; each one's diff is exposed as `steps.<id>.outputs.diff`).
//! This makes multi-agent fan-out safe without merging concurrent agent edits.
//!
//! ## Resume semantics
//!
//! Each step is checkpointed `Running` before dispatch and to its terminal status after,
//! so a step interrupted mid-flight by a crash is distinguishable from one not yet
//! started. On resume, only `Passed`/`Skipped` steps are skipped; a `Running` (or absent)
//! step is **re-executed from scratch**. For a **durable** run the executor takes an
//! off-branch git snapshot of the workspace after each shared-workdir step and, on resume,
//! restores the workdir to the last snapshot before re-running — so a step interrupted
//! mid-edit re-applies from a clean tree rather than double-applying its file changes. The
//! snapshots are dangling commits anchored by a per-run ref (`refs/odin/run/<id>`) that is
//! dropped when the run finishes, so they never reach the workflow's branch or its PR.
//!
//! Snapshot/restore covers the *uncommitted* working-tree phase only. Once a step **commits**
//! (HEAD leaves base), snapshotting disengages for the rest of the run — git's own commits
//! are the durable record, and rewinding past them would corrupt the run branch — so steps
//! after the first commit re-run on resume without a snapshot rewind (they may double-apply,
//! the documented pre-snapshot behavior). Also outside the snapshot's reach: side effects
//! beyond the workspace (a pushed branch, an opened PR), `.gitignore`d paths, and nested
//! untracked git repos (which `git clean` leaves in place).

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use indexmap::IndexMap;
use serde_json::Value;

use crate::api::{
    ApprovalDecision, Decision, RunStatus, RunSummary, SideEffect, StepResult, StepStatus,
};
use crate::error::Result;
use crate::ids::{RunId, StepId};
use crate::ir::{Step, StepKind, Workflow};
use crate::provider::StreamMux;
use crate::registry::Registry;
use crate::traits::{CancelToken, RunEvent, RunState, StepState, Store, Workspace};
use crate::usage::Usage;

mod api;
mod ctx;
mod dispatch;
mod gitio;
mod loop_driver;
mod provision;
use ctx::{build_ctx, collect_side_effects, effective_timeout, skipped_outcome, step_result};

/// The reserved, auto-captured artifact name.
const DIFF: &str = "DIFF";

/// The concrete engine returned by [`super::EngineBuilder::build`].
pub(crate) struct LocalEngine {
    registry: Registry,
    store: Option<Arc<dyn Store>>,
    repo_root: PathBuf,
    /// Serializes `git worktree add/remove/prune`: git's worktree metadata is not safe for
    /// concurrent mutation, and scratch steps provision worktrees in parallel.
    worktree_lock: tokio::sync::Mutex<()>,
    /// Cache of the built-in workspace per (kind, config) so every run — and every resume —
    /// shares ONE instance. This is what makes `slot_pool`'s concurrency cap hold across
    /// concurrent runs (its lease state is in-memory and per-instance); a fresh instance per
    /// run would each get a full set of permits and hand out the same slot to everyone.
    workspaces: std::sync::Mutex<HashMap<String, Arc<dyn Workspace>>>,
    /// Run ids currently executing. A resume (crash-recovery sweep or an approval decision)
    /// claims its run id here first; a second concurrent attempt to execute the SAME run is
    /// refused, so an approval racing another approval — or a sweep — can't double-run a run's
    /// side effects.
    running: std::sync::Mutex<HashSet<RunId>>,
    /// Cancel tokens of runs currently executing, keyed by run id, so an external
    /// [`cancel_run`](crate::traits::Engine::cancel_run) /
    /// [`cancel_all_active`](crate::traits::Engine::cancel_all_active) can fire a run's token —
    /// killing its in-flight step's subprocess and ending the run as `Cancelled`. Each execute
    /// pass registers its token for its duration (see [`LocalEngine::register_cancel`]) and the
    /// guard removes it on completion, so the map holds exactly the in-flight runs.
    active_cancels: std::sync::Mutex<HashMap<RunId, CancelToken>>,
    /// When set, each provider / `run:` / gate step tees its live subprocess output to this mux
    /// (prefixed by step id), in addition to capturing it — the `odin run --stream` view. `None`
    /// (the default, and always for the daemon) keeps output capture-only.
    stream: Option<StreamMux>,
    /// Optional push-based progress callback fired for every `RunEvent` of every run (durable or
    /// not), in addition to the durable audit log — the embedder's live view. Set via
    /// [`super::EngineBuilder::on_event`]; see [`super::EventHook`].
    on_event: Option<super::EventHook>,
    /// In-memory view of runs that are NOT persisted to the store (a `durable: false` run, or any
    /// run when no store is configured), so `summary()` / `recent()` can still see them live. Each
    /// entry is a **light** snapshot ([`Self::light_snapshot`]): per-step status/exit/usage but NOT
    /// step output, the diff, or the trigger payload — bounded memory, not full run state. Process-
    /// local and lost on restart; a finished entry lingers until evicted or exit. Persisted runs
    /// are never mirrored, so the two sources normally don't overlap (see `recent`'s dedup for the
    /// one case they can — a `durable` flag flipped mid-run).
    mirror: std::sync::Mutex<IndexMap<RunId, RunState>>,
}

/// Max runs held in the in-memory mirror. When full, a NEW run evicts the oldest *terminal* run;
/// only if all 256 are in-flight (≥256 concurrent unpersisted runs — pathological) does it evict
/// the oldest live run, which then reappears at its next step boundary.
const MIRROR_CAP: usize = 256;

/// Max serialized size of a single param value retained in a mirror [`light_snapshot`]; larger
/// values (e.g. a webhook param mapped from a big payload field) are replaced with a placeholder.
const MIRROR_PARAM_CAP: usize = 4096;

/// An execution claim on a run id, released when dropped. Held across a whole resume so the run
/// executes once even under concurrent decisions. See [`LocalEngine::claim_run`].
struct RunClaim<'a> {
    engine: &'a LocalEngine,
    run_id: RunId,
}

impl Drop for RunClaim<'_> {
    fn drop(&mut self) {
        self.engine.running.lock().unwrap().remove(&self.run_id);
    }
}

/// A registration of a run's cancel token in [`LocalEngine::active_cancels`], removed when dropped
/// — so a completed/suspended/failed run is no longer cancellable and the map can't leak entries.
struct CancelGuard<'a> {
    engine: &'a LocalEngine,
    run_id: RunId,
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        self.engine
            .active_cancels
            .lock()
            .unwrap()
            .remove(&self.run_id);
    }
}

/// How often a run's watcher polls the store for a cross-process `odin cancel` request.
const CANCEL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// A background poller that fires a run's cancel token when a cross-process `odin cancel` request
/// lands in the store (the in-process `active_cancels` map can't be reached from another process,
/// e.g. the CLI cancelling a run executing in the daemon). Aborted on drop — held only for the
/// run's execute duration.
struct CancelWatcher(tokio::task::JoinHandle<()>);

impl Drop for CancelWatcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What executing a single step produced (richer than the persisted `StepState`).
pub(crate) struct StepOutcome {
    status: StepStatus,
    exit_code: Option<i32>,
    outputs: IndexMap<String, Value>,
    usage: Option<Usage>,
    gates: IndexMap<String, bool>,
    side_effects: Vec<SideEffect>,
    error: Option<String>,
    /// The raw, *un-wrapped* failure diagnostic (gate stdout+stderr, the exit stderr, or the
    /// judge verdict) — what `retry.feedback` surfaces so a retried step can self-correct. Unlike
    /// `error` (a human/log summary like `gate "test" failed\nstderr:\n…`), this has no synthetic
    /// headline, so the *first line* is real content. `None` on success or when there is no detail
    /// beyond `error`. Transient — not persisted to `StepState`.
    failure_detail: Option<String>,
    /// Captured stderr from the dispatch (provider/`run:`), folded into `error` on failure.
    /// Transient — not persisted to `StepState`.
    stderr: String,
    attempts: u8,
    judge_score: Option<f32>,
}

impl StepOutcome {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            status: StepStatus::Failed,
            exit_code: None,
            outputs: IndexMap::new(),
            usage: None,
            gates: IndexMap::new(),
            side_effects: Vec::new(),
            error: Some(error.into()),
            failure_detail: None,
            stderr: String::new(),
            attempts: 1,
            judge_score: None,
        }
    }

    fn passing(exit_code: i32, outputs: IndexMap<String, Value>, usage: Option<Usage>) -> Self {
        Self {
            status: StepStatus::Passed,
            exit_code: Some(exit_code),
            outputs,
            usage,
            gates: IndexMap::new(),
            side_effects: Vec::new(),
            error: None,
            failure_detail: None,
            stderr: String::new(),
            attempts: 1,
            judge_score: None,
        }
    }

    /// The persisted projection of this outcome (drops the transient `stderr`/`failure_detail`).
    fn to_state(&self) -> StepState {
        StepState {
            status: self.status,
            attempts: self.attempts,
            exit_code: self.exit_code,
            outputs: self.outputs.clone(),
            usage: self.usage,
            gates: self.gates.clone(),
            judge_score: self.judge_score,
            side_effects: self.side_effects.clone(),
            error: self.error.clone(),
        }
    }
}

/// The summary-shaped result of an execution pass.
struct ExecResult {
    steps: Vec<StepResult>,
    side_effects: Vec<SideEffect>,
    usage: Usage,
    diff: Option<String>,
    /// `Some(step)` if the run PAUSED at an undecided approval gate rather than completing —
    /// the run is left `AwaitingApproval`, its workspace kept, to resume on a decision.
    suspended: Option<StepId>,
}

/// Boxes a step future so the scheduler's `FuturesUnordered` can hold both real step runs and
/// the immediately-ready outcomes of synchronously-resolved approval gates.
type StepFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = (StepId, StepOutcome)> + Send + 'a>>;

fn boxed<'a>(
    f: impl std::future::Future<Output = (StepId, StepOutcome)> + Send + 'a,
) -> StepFuture<'a> {
    Box::pin(f)
}

impl LocalEngine {
    pub(crate) fn new(
        registry: Registry,
        store: Option<Arc<dyn Store>>,
        repo_root: PathBuf,
        stream: Option<StreamMux>,
        on_event: Option<super::EventHook>,
    ) -> Self {
        Self {
            registry,
            store,
            repo_root,
            worktree_lock: tokio::sync::Mutex::new(()),
            workspaces: std::sync::Mutex::new(HashMap::new()),
            running: std::sync::Mutex::new(HashSet::new()),
            active_cancels: std::sync::Mutex::new(HashMap::new()),
            stream,
            on_event,
            mirror: std::sync::Mutex::new(IndexMap::new()),
        }
    }

    /// Records a not-persisted run's latest state in the in-memory mirror (persisted runs go to the
    /// store instead). Stores a [`Self::light_snapshot`] (no step output / diff / payload), bounded
    /// to [`MIRROR_CAP`]: an update keeps the run's slot; a new run over the cap evicts the oldest
    /// terminal run, else (all in-flight) the oldest overall.
    fn mirror_put(&self, state: &RunState) {
        let light = Self::light_snapshot(state);
        let mut mirror = self.mirror.lock().unwrap();
        if !mirror.contains_key(&state.run_id) && mirror.len() >= MIRROR_CAP {
            let victim = mirror
                .iter()
                .find(|(_, s)| s.status.is_terminal())
                .map(|(id, _)| *id)
                .or_else(|| mirror.keys().next().copied());
            if let Some(victim) = victim {
                mirror.shift_remove(&victim);
            }
        }
        mirror.insert(state.run_id, light);
    }

    /// A memory-bounded copy of `state` for the mirror: keeps identity, status, per-step
    /// status/exit/usage/gates, side effects, timings, and an approval gate's `message`, but DROPS
    /// the heavy, potentially unbounded fields — each step's captured output, the `DIFF` artifact,
    /// the trigger payload, and any oversize param value. So `summary()`/`recent()` of a non-durable
    /// run report status and shape, not full output (use the [`on_event`](super::EventHook) hook or
    /// `--stream` for live output).
    fn light_snapshot(state: &RunState) -> RunState {
        let steps = state
            .steps
            .iter()
            .map(|(id, st)| {
                // Keep only an approval gate's `message` output (so a paused run's gate message
                // still surfaces in the RunView); drop everything else (step stdout, up to 1 MiB).
                let outputs = if st.status == StepStatus::AwaitingApproval {
                    st.outputs
                        .iter()
                        .filter(|(k, _)| k.as_str() == "message")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                } else {
                    IndexMap::new()
                };
                (
                    id.clone(),
                    StepState {
                        status: st.status,
                        attempts: st.attempts,
                        exit_code: st.exit_code,
                        outputs,
                        usage: st.usage,
                        gates: st.gates.clone(),
                        judge_score: st.judge_score,
                        side_effects: st.side_effects.clone(),
                        error: st.error.clone(),
                    },
                )
            })
            .collect();
        // Keep typed params, but cap any single value's size — a webhook param can be mapped from a
        // (large) payload field, which would otherwise re-open the unbounded-memory hole.
        let params = state
            .input
            .params
            .iter()
            .map(|(k, v)| {
                let capped = if serde_json::to_string(v).map_or(0, |s| s.len()) > MIRROR_PARAM_CAP {
                    Value::String("[omitted: large param]".to_owned())
                } else {
                    v.clone()
                };
                (k.clone(), capped)
            })
            .collect();
        let input = crate::api::RunInput {
            trigger: state.input.trigger.clone(),
            trigger_payload: Value::Null, // dropped (unbounded; attacker-controlled for webhooks)
            params,
            idempotency_key: state.input.idempotency_key.clone(),
        };
        RunState {
            run_id: state.run_id,
            workflow: state.workflow.clone(),
            schema_major: state.schema_major,
            status: state.status,
            error: state.error.clone(),
            steps,
            artifacts: IndexMap::new(), // dropped (the uncapped DIFF)
            provider_versions: state.provider_versions.clone(),
            approvals: state.approvals.clone(),
            input,
            workspace: state.workspace.clone(),
            base_commit: state.base_commit.clone(),
            snapshot: state.snapshot.clone(),
            loop_state: state.loop_state.clone(),
            created_at: state.created_at,
            updated_at: state.updated_at,
        }
    }

    /// The live-output sink for `step`, derived from the engine's [`StreamMux`] (the `--stream`
    /// view) — `None` when streaming is off, so the capture-only fast path stays the default.
    fn step_stream(&self, label: &str) -> Option<crate::provider::StreamSink> {
        self.stream.as_ref().map(|m| m.sink(label))
    }

    /// Registers `token` as `run_id`'s cancel handle for an execute pass, returning a guard that
    /// deregisters it on drop. Held across the whole pass so `cancel_run` can reach the run.
    fn register_cancel(&self, run_id: RunId, token: CancelToken) -> CancelGuard<'_> {
        self.active_cancels.lock().unwrap().insert(run_id, token);
        CancelGuard {
            engine: self,
            run_id,
        }
    }

    /// Spawns a background poller that fires `cancel` (a USER cancel → terminal `Cancelled`) when a
    /// cross-process `odin cancel` request for `run_id` lands in the store. Returns `None` for a
    /// non-durable run or when no store is configured (nothing to poll). The guard stops the poller
    /// on drop, so it lives exactly as long as the run executes.
    fn spawn_cancel_watcher(
        &self,
        run_id: RunId,
        durable: bool,
        cancel: CancelToken,
    ) -> Option<CancelWatcher> {
        if !durable {
            return None;
        }
        let store = self.store.clone()?;
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // The run ended (guard dropped → task aborted) or was cancelled another way.
                    () = cancel.cancelled() => break,
                    () = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {
                        if matches!(store.is_cancel_requested(run_id).await, Ok(true)) {
                            cancel.cancel();
                            break;
                        }
                    }
                }
            }
        });
        Some(CancelWatcher(handle))
    }

    /// Claims `run_id` for execution, returning a guard that releases it on drop. Returns
    /// `None` if the run is already executing — the caller must then NOT execute it (running a
    /// run concurrently with itself would duplicate its side effects).
    fn claim_run(&self, run_id: RunId) -> Option<RunClaim<'_>> {
        if self.running.lock().unwrap().insert(run_id) {
            Some(RunClaim {
                engine: self,
                run_id,
            })
        } else {
            None
        }
    }

    async fn checkpoint(&self, durable: bool, state: &RunState) -> Result<()> {
        // A run is *persisted* iff it's durable AND a store is configured. Everything else — a
        // `durable: false` run, OR a durable run with no store — would otherwise be invisible, so
        // mirror it in memory. Keying on "persisted" (not just `durable`) closes the gap where a
        // durable run without a store vanished from `summary()`/`recent()`.
        if durable {
            if let Some(store) = &self.store {
                store.checkpoint(state).await?;
                // If this run was ever mirrored (its `durable` flag flipped false→true mid-run),
                // drop the now-redundant mirror entry so the two sources can't overlap.
                self.mirror.lock().unwrap().shift_remove(&state.run_id);
                return Ok(());
            }
        }
        self.mirror_put(state);
        Ok(())
    }

    /// The single choke point for every [`RunEvent`]. Fires the push-based progress hook for
    /// **every** run (durable or not) — the embedder's live view — then appends to the durable
    /// audit log for **durable** runs only.
    ///
    /// The audit log stays durable-only on purpose: a non-durable run has no `runs` row, so its
    /// events would be orphaned — invisible to `odin status` and unreclaimable by `prune` (which
    /// deletes events alongside the row), growing the `events` table without bound. The hook has
    /// no such persistence, so it can safely carry non-durable runs (see [`Self::on_event`]).
    async fn emit(&self, run_id: RunId, durable: bool, event: RunEvent) {
        // Push-based hook first, inline. No engine lock is held across this call (the `running` /
        // `active_cancels` guards are scoped tightly), so a hook that synchronously calls back into
        // the engine can't deadlock — but it MUST be non-blocking (see `EventHook`). A panic is
        // caught so it can't abort the run; note `catch_unwind` catches only under `panic =
        // "unwind"` (the default) — under `panic = "abort"` a panicking hook still aborts. The warn
        // fires per panicking call (not once) and names the event kind.
        if let Some(hook) = &self.on_event {
            let fired =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(run_id, &event)));
            if fired.is_err() {
                tracing::warn!(run_id = %run_id, event = ?std::mem::discriminant(&event), "on_event callback panicked; event dropped");
            }
        }
        if durable {
            if let Some(store) = &self.store {
                let _ = store.append_event(run_id, &event).await;
            }
        }
    }

    /// Emits a [`RunEvent::RunSuspended`] (the run paused, not finished) with its reason.
    async fn emit_suspended(
        &self,
        run_id: RunId,
        durable: bool,
        reason: crate::traits::SuspendReason,
    ) {
        self.emit(
            run_id,
            durable,
            RunEvent::RunSuspended {
                reason,
                at: Utc::now(),
            },
        )
        .await;
    }

    /// Emits a [`RunEvent::RunCancelled`] carrying *why* (user cancel vs graceful shutdown),
    /// derived from the fired token. Called at the terminal point of a run that ended `Cancelled`,
    /// just before its `RunFinished`.
    async fn emit_cancelled(&self, run_id: RunId, durable: bool, cancel: &CancelToken) {
        let reason = if cancel.is_shutdown() {
            crate::traits::CancelReason::Shutdown
        } else {
            crate::traits::CancelReason::User
        };
        self.emit(
            run_id,
            durable,
            RunEvent::RunCancelled {
                reason,
                at: Utc::now(),
            },
        )
        .await;
    }

    /// Appends the gate/judge/finished audit events for a completed step (durable runs only).
    async fn emit_step_events(
        &self,
        run_id: RunId,
        durable: bool,
        id: &StepId,
        outcome: &StepOutcome,
    ) {
        for (gate, passed) in &outcome.gates {
            self.emit(
                run_id,
                durable,
                RunEvent::GateResult {
                    step: id.clone(),
                    gate: gate.clone(),
                    passed: *passed,
                    at: Utc::now(),
                },
            )
            .await;
        }
        if let Some(score) = outcome.judge_score {
            self.emit(
                run_id,
                durable,
                RunEvent::JudgeResult {
                    step: id.clone(),
                    score,
                    passed: outcome.status == StepStatus::Passed,
                    at: Utc::now(),
                },
            )
            .await;
        }
        self.emit(
            run_id,
            durable,
            RunEvent::StepFinished {
                step: id.clone(),
                status: outcome.status,
                exit_code: outcome.exit_code,
                error: outcome.error.clone(),
                at: Utc::now(),
            },
        )
        .await;
    }

    /// Records a step `Running` and checkpoints it — the durability boundary written
    /// before dispatch, so a step interrupted mid-flight is distinguishable from one not
    /// yet started. (The `StepStarted` event is emitted per attempt by `run_with_retry`.)
    async fn mark_running(
        &self,
        workflow: &Workflow,
        state: &mut RunState,
        id: &StepId,
    ) -> Result<()> {
        state.steps.insert(
            id.clone(),
            StepState {
                status: StepStatus::Running,
                attempts: 1,
                exit_code: None,
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: None,
            },
        );
        state.updated_at = Utc::now();
        self.checkpoint(workflow.durable, state).await
    }

    /// Marks an approval gate `AwaitingApproval` (recording the message for `odin list`/the
    /// approver) and flips the RUN to `AwaitingApproval`, then checkpoints — persisting the
    /// pause so a crash leaves the run correctly parked (not auto-resumed) until a decision.
    /// (A workflow with an approval step is required to be `durable` — ODIN032 — since a pause
    /// is unresumable without persistence; so this checkpoint persists, to the store when one is
    /// configured, else to the in-memory mirror.)
    async fn mark_awaiting(
        &self,
        workflow: &Workflow,
        state: &mut RunState,
        id: &StepId,
        message: Option<&str>,
    ) -> Result<()> {
        let mut outputs = IndexMap::new();
        if let Some(m) = message {
            outputs.insert("message".to_owned(), Value::String(m.to_owned()));
        }
        state.steps.insert(
            id.clone(),
            StepState {
                status: StepStatus::AwaitingApproval,
                attempts: 0,
                exit_code: None,
                outputs,
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: None,
            },
        );
        state.status = RunStatus::AwaitingApproval;
        state.updated_at = Utc::now();
        self.checkpoint(workflow.durable, state).await
    }

    /// Walks the DAG as a bounded concurrent ready-set, executing or skipping each step and
    /// checkpointing as it goes. (A long single function: it *is* the scheduler.)
    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        workflow: &Workflow,
        state: &mut RunState,
        workdir: &Path,
        params: &IndexMap<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ExecResult> {
        let order = crate::validate::graph::topo_order(&workflow.steps)
            .unwrap_or_else(|_| workflow.steps.iter().map(|s| s.id.clone()).collect());
        let by_id: HashMap<&str, &Step> =
            workflow.steps.iter().map(|s| (s.id.as_str(), s)).collect();
        let max_parallel = workflow.max_parallel.map_or(1, NonZeroUsize::get);
        let run_id = state.run_id;

        // Reclaim any scratch worktrees orphaned by a previous (crashed) attempt of this run.
        if workflow.steps.iter().any(|s| s.scratch) {
            self.cleanup_scratch(run_id, workdir, cancel).await;
        }

        // Record the run's base commit once, on a FRESH run only (used as the diff base for
        // all runs, and the snapshot anchor for durable ones). On a resume we must NOT
        // re-read HEAD — steps may have advanced it — so a pre-feature run with no recorded
        // base simply keeps `None` (DIFF falls back to plain `git diff`).
        let resuming = !state.steps.is_empty();
        if !resuming && state.base_commit.is_none() {
            state.base_commit = self.git_head(workdir, cancel).await;
        }
        let base_commit = state.base_commit.clone();

        // On resume, restore the workdir to the last snapshot so a step interrupted mid-edit
        // re-runs from a clean tree — but ONLY while HEAD is still at base. `state.snapshot`
        // is written `Some` only with HEAD at base, yet a step can commit (moving HEAD) and
        // then crash BEFORE the post-step block clears it; so we re-verify `at_base` here and
        // skip the restore once anything has been committed (rewinding past a commit would
        // corrupt the run branch and drop committed work). When at base and there's no current
        // snapshot, restore to base only if no shared-workdir step has passed yet.
        if workflow.durable && resuming {
            let any_shared_passed = state.steps.iter().any(|(id, st)| {
                matches!(st.status, StepStatus::Passed)
                    && !by_id.get(id.as_str()).is_some_and(|s| s.scratch)
            });
            let at_base =
                base_commit.is_some() && self.git_head(workdir, cancel).await == base_commit;
            let target = match &state.snapshot {
                Some(snapshot) if at_base => Some(snapshot.clone()),
                None if at_base && !any_shared_passed => base_commit.clone(),
                _ => None,
            };
            if let Some(target) = target {
                self.restore_workdir(workdir, &target, cancel).await;
            }
        }

        let mut side_effects = Vec::new();
        let mut usage = Usage::default();
        // Once any step commits (HEAD leaves base) we stop snapshotting for the rest of the
        // run; this also avoids re-reading HEAD on every subsequent step.
        let mut snapshots_disengaged = false;
        // Seed from the persisted DIFF so a resumed run carries it forward (already-passed
        // steps cannot re-capture it); otherwise a downstream `{{ artifacts.DIFF }}` would
        // be undefined after a crash.
        let mut diff: Option<String> = state
            .artifacts
            .get(&crate::ids::ArtifactName::new(DIFF))
            .cloned();

        // Results, keyed by id; emitted in topological order at the end (completion order
        // is nondeterministic under concurrency).
        let mut results: IndexMap<StepId, StepResult> = IndexMap::new();
        // `settled` = steps in a terminal status (Passed/Failed/Skipped); a step is *ready*
        // once all its deps are settled. Seeded from a resume with already-finished steps.
        //
        // A `Failed` step is seeded too: its retries were already exhausted before the crash, so
        // re-running it would grant attempts beyond its policy AND leave the run inconsistent —
        // its dependents are recorded `Skipped` (and seeded as such), so if the failed step
        // re-ran and now passed, those dependents would stay frozen-skipped forever. Seeding the
        // failure keeps the resumed run deterministic (the failure and its skip-cascade hold) and
        // a genuinely independent branch still proceeds.
        let mut settled: HashSet<StepId> = HashSet::new();
        for (id, st) in &state.steps {
            if matches!(
                st.status,
                StepStatus::Passed | StepStatus::Failed | StepStatus::Skipped
            ) {
                settled.insert(id.clone());
                results.insert(id.clone(), step_result(id, st));
                // Carry forward side effects already recorded by finished steps (including a
                // failed step's — e.g. a push that landed before a later gate failed); they won't
                // re-run, so without this a resumed run's summary would drop every PR/commit/push
                // from before the crash.
                side_effects.extend(st.side_effects.iter().cloned());
            }
        }
        let mut started: HashSet<StepId> = settled.clone();
        let mut in_flight = FuturesUnordered::new();
        // A non-`scratch` step mutates the shared workdir, so it runs *exclusively* — never
        // alongside another step. `scratch` steps run in isolated worktrees, so any number
        // may run concurrently (bounded by `max_parallel`).
        let mut exclusive_running = false;
        // Set when an undecided approval gate is reached: stop scheduling new steps, drain
        // in-flight, then return suspended (the run pauses, keeping its workspace).
        let mut suspended_gate: Option<StepId> = None;

        loop {
            // Fill the ready-set up to the concurrency limit, honoring the exclusivity rule. A
            // fired cancel token stops launching NEW steps (like a suspended gate); the in-flight
            // ones are killed by the token in the process layer and settle, then the loop drains.
            while in_flight.len() < max_parallel
                && !exclusive_running
                && suspended_gate.is_none()
                && !cancel.is_cancelled()
            {
                let Some(step) = order
                    .iter()
                    .filter_map(|id| by_id.get(id.as_str()).copied())
                    .find(|s| {
                        !started.contains(&s.id)
                            && s.depends_on.iter().all(|d| settled.contains(d))
                            // An exclusive (non-scratch) step can only start when the workdir is
                            // idle. Encoding that here — rather than `break`-ing on the first
                            // not-yet-startable exclusive step — lets later scratch steps fill the
                            // remaining slots instead of being blocked head-of-line behind it.
                            && (s.scratch || in_flight.is_empty())
                    })
                else {
                    break;
                };
                let exclusive = !step.scratch;

                let deps_passed = step.depends_on.iter().all(|d| {
                    matches!(
                        state.steps.get(d).map(|s| s.status),
                        Some(StepStatus::Passed)
                    )
                });

                // An approval gate is resolved by the scheduler (it holds the recorded
                // decisions), never dispatched: approve → pass, reject → fail (with feedback),
                // and an *undecided* gate PAUSES the whole run here.
                if let StepKind::Approval(appr) = &step.kind {
                    let id = step.id.clone();
                    started.insert(id.clone());
                    if !deps_passed {
                        let mut o = skipped_outcome();
                        o.error = Some("an upstream dependency did not pass".to_owned());
                        in_flight.push(boxed(std::future::ready((id, o))));
                    } else if let Some(decision) = state.approvals.get(&id).cloned() {
                        // The gate resolves now (a decision is applied), so it "runs": emit a
                        // StepStarted to pair with the StepFinished `emit_step_events` writes when
                        // this settles. (A skipped or still-awaiting gate never started, matching
                        // the skip convention for regular steps.)
                        self.emit(
                            run_id,
                            workflow.durable,
                            RunEvent::StepStarted {
                                step: id.clone(),
                                attempt: 1,
                                at: Utc::now(),
                            },
                        )
                        .await;
                        let o = match decision.decision {
                            Decision::Approved => StepOutcome::gate_approved(&decision),
                            Decision::Rejected => StepOutcome::gate_rejected(&decision),
                        };
                        in_flight.push(boxed(std::future::ready((id, o))));
                    } else {
                        self.mark_awaiting(workflow, state, &id, appr.message.as_deref())
                            .await?;
                        suspended_gate = Some(id);
                    }
                    continue;
                }

                // A `loop:` step is run by the scheduler, not dispatched: it is exclusive
                // (non-scratch), so by the exclusivity check above `in_flight` is empty here —
                // `&mut state` is uncontended and the shared workdir is idle. `run_loop` drives
                // the body as a sequential mini-driver and returns one aggregate outcome, folded
                // by the post-step block like any other step.
                if let StepKind::Loop(l) = &step.kind {
                    let id = step.id.clone();
                    started.insert(id.clone());
                    let outcome = if deps_passed {
                        self.mark_running(workflow, state, &id).await?;
                        // Pair a StepStarted with the StepFinished emitted when this settles; the
                        // loop's own inner steps emit their own paired events inside `run_loop`.
                        self.emit(
                            run_id,
                            workflow.durable,
                            RunEvent::StepStarted {
                                step: id.clone(),
                                attempt: 1,
                                at: Utc::now(),
                            },
                        )
                        .await;
                        self.run_loop(
                            run_id,
                            step,
                            l,
                            state,
                            workdir,
                            params,
                            base_commit.as_deref(),
                            diff.as_deref(),
                            workflow,
                            cancel,
                        )
                        .await?
                    } else {
                        let mut o = skipped_outcome();
                        o.error = Some("an upstream dependency did not pass".to_owned());
                        o
                    };
                    in_flight.push(boxed(std::future::ready((id, outcome))));
                    continue;
                }

                let timeout = effective_timeout(step, &workflow.defaults);
                let ctx = build_ctx(
                    params,
                    &state.input.trigger_payload,
                    &state.steps,
                    diff.as_deref(),
                    state,
                    workflow,
                );

                // Durability boundary before acting (see `mark_running`).
                self.mark_running(workflow, state, &step.id).await?;
                started.insert(step.id.clone());
                if exclusive {
                    exclusive_running = true;
                }
                in_flight.push(boxed(self.run_one(
                    run_id,
                    workflow.durable,
                    step,
                    ctx,
                    workdir,
                    timeout,
                    deps_passed,
                    workflow.defaults.retry.as_ref(),
                    cancel,
                )));
                if exclusive {
                    break; // nothing else starts beside it
                }
            }

            let Some((id, outcome)) = in_flight.next().await else {
                break; // nothing running and nothing ready — done
            };
            exclusive_running = false; // an exclusive step ran alone, so this clears it

            tracing::info!(
                step = %id,
                status = ?outcome.status,
                exit_code = outcome.exit_code,
                attempts = outcome.attempts,
                "step finished"
            );

            // Graceful-shutdown rewind: a durable run interrupted by `shutdown` (daemon stop)
            // had this step's subprocess killed, so it came back `Failed`. Leave it back at
            // `Running` — NOT terminal `Failed` — and checkpoint, so `resume_all` re-runs it on
            // the next start (exactly like a hard crash, but reached promptly). Don't settle it,
            // don't fold its partial usage/side-effects, don't emit `StepFinished`: the re-run
            // produces the real outcome. A USER cancel keeps the kill terminal (the run won't
            // resume), and a non-durable run can't resume regardless — both fall through.
            if workflow.durable
                && cancel.is_shutdown()
                && matches!(outcome.status, StepStatus::Failed)
            {
                if let Some(st) = state.steps.get_mut(&id) {
                    st.status = StepStatus::Running;
                    st.error = None;
                }
                state.updated_at = Utc::now();
                self.checkpoint(workflow.durable, state).await?;
                continue;
            }

            if let Some(u) = &outcome.usage {
                usage.add(*u);
            }
            side_effects.extend(outcome.side_effects.iter().cloned());
            // The persisted projection; side effects are kept so a later resume can reconstruct
            // the run's full set without re-running the step (see the resume seed below).
            let step_state = outcome.to_state();
            state.steps.insert(id.clone(), step_state.clone());
            if matches!(
                outcome.status,
                StepStatus::Passed | StepStatus::Failed | StepStatus::Skipped
            ) {
                settled.insert(id.clone());
            }
            // Only a shared-workdir (non-scratch) step refreshes the run's DIFF and takes a
            // resume snapshot; a scratch step's diff is its own `outputs.diff` (run_one) and
            // it never touches the shared tree.
            let is_scratch = by_id.get(id.as_str()).is_some_and(|s| s.scratch);
            if matches!(outcome.status, StepStatus::Passed) && !is_scratch {
                diff = self
                    .capture_diff(workdir, base_commit.as_deref(), cancel)
                    .await;
                if let Some(d) = &diff {
                    state.artifacts.insert(DIFF.into(), d.clone());
                }
                if workflow.durable {
                    // Snapshot only while nothing has been committed yet (HEAD still at base):
                    // once a step commits, git is the durable record and rewinding past it
                    // would corrupt the run branch, so we disengage for the rest of the run.
                    // Set `snapshot` UNCONDITIONALLY — a failed or disengaged snapshot must
                    // become `None`, never a stale pointer a later resume would rewind to
                    // (which would discard this completed step's work).
                    let head = if snapshots_disengaged {
                        None
                    } else {
                        self.git_head(workdir, cancel).await
                    };
                    let at_base = head.is_some() && head == base_commit;
                    state.snapshot = if let Some(base) = base_commit.as_deref().filter(|_| at_base)
                    {
                        // Best-effort: a returned `None` here is a transient snapshot failure,
                        // not a disengage — the next step retries while still at base.
                        self.snapshot_workdir(workdir, base, run_id, cancel).await
                    } else {
                        // Disengage for good only on a CONFIRMED commit (HEAD read OK and moved
                        // off base). A transient HEAD-read failure (head is None) leaves this
                        // step without a snapshot but lets the next step retry.
                        if head.is_some() && head != base_commit {
                            snapshots_disengaged = true;
                        }
                        None
                    };
                }
            }
            state.updated_at = Utc::now();
            self.checkpoint(workflow.durable, state).await?;
            self.emit_step_events(run_id, workflow.durable, &id, &outcome)
                .await;
            results.insert(id.clone(), step_result(&id, &step_state));
        }

        // Emit results in topological order regardless of completion order.
        let summary = order
            .iter()
            .filter_map(|id| results.shift_remove(id))
            .collect();
        Ok(ExecResult {
            steps: summary,
            side_effects,
            usage,
            diff,
            suspended: suspended_gate,
        })
    }

    /// Builds the summary for a run that PAUSED at an approval gate (status `AwaitingApproval`,
    /// `finished_at` unset). Steps + side effects are reconstructed from persisted `state` so the
    /// paused gate itself (which produced no execution outcome) is included.
    fn suspended_summary(
        run_id: RunId,
        workflow: &Workflow,
        exec: &ExecResult,
        started_at: chrono::DateTime<Utc>,
        steps: &IndexMap<StepId, StepState>,
    ) -> RunSummary {
        RunSummary {
            run_id,
            workflow: workflow.name.clone(),
            status: RunStatus::AwaitingApproval,
            steps: steps.iter().map(|(id, st)| step_result(id, st)).collect(),
            usage: exec.usage,
            side_effects: collect_side_effects(steps),
            diff: exec.diff.clone(),
            error: None,
            started_at,
            finished_at: None,
        }
    }

    /// Builds the summary for a durable run SUSPENDED by graceful shutdown: it kept its workspace
    /// and snapshot ref and is checkpointed non-terminal (`Running`, `finished_at` unset), so
    /// `resume_all` completes it on the next start. Shaped like [`suspended_summary`] but carries
    /// the live `Running` status.
    ///
    /// [`suspended_summary`]: Self::suspended_summary
    fn interrupted_summary(
        run_id: RunId,
        workflow: &Workflow,
        exec: &ExecResult,
        started_at: chrono::DateTime<Utc>,
        steps: &IndexMap<StepId, StepState>,
    ) -> RunSummary {
        RunSummary {
            run_id,
            workflow: workflow.name.clone(),
            status: RunStatus::Running,
            steps: steps.iter().map(|(id, st)| step_result(id, st)).collect(),
            usage: exec.usage,
            side_effects: collect_side_effects(steps),
            diff: exec.diff.clone(),
            error: None,
            started_at,
            finished_at: None,
        }
    }

    fn summarize(
        run_id: RunId,
        workflow: &Workflow,
        exec: ExecResult,
        error: Option<String>,
        started_at: chrono::DateTime<Utc>,
    ) -> (RunStatus, RunSummary) {
        let failed = exec
            .steps
            .iter()
            .any(|s| matches!(s.status, StepStatus::Failed));
        let status = if error.is_some() || failed {
            RunStatus::Failed
        } else {
            RunStatus::Succeeded
        };
        // Surface *why* the run failed: prefer an explicit run-level error, else the first
        // failed step's recorded reason (so `summary.error` isn't a contentless placeholder).
        let error = error.or_else(|| {
            exec.steps
                .iter()
                .find(|s| matches!(s.status, StepStatus::Failed))
                .map(|s| {
                    s.error.clone().map_or_else(
                        || format!("step {:?} failed", s.id.as_str()),
                        |e| format!("step {:?} failed: {e}", s.id.as_str()),
                    )
                })
        });
        let summary = RunSummary {
            run_id,
            workflow: workflow.name.clone(),
            status,
            steps: exec.steps,
            usage: exec.usage,
            side_effects: exec.side_effects,
            diff: exec.diff,
            error,
            started_at,
            finished_at: Some(Utc::now()),
        };
        (status, summary)
    }
}

impl StepOutcome {
    /// Converts an outcome into a skipped one, preserving the explanatory error as info.
    fn skipped(mut self) -> Self {
        self.status = StepStatus::Skipped;
        self
    }

    /// Outcome for an approval gate that a human approved — the gate passes; the decision is
    /// recorded in the gate's outputs for the audit trail.
    fn gate_approved(d: &ApprovalDecision) -> Self {
        let mut outputs = IndexMap::new();
        outputs.insert("decision".to_owned(), Value::String("approved".to_owned()));
        outputs.insert("approver".to_owned(), Value::String(d.approver.clone()));
        if let Some(note) = &d.note {
            outputs.insert("note".to_owned(), Value::String(note.clone()));
        }
        StepOutcome {
            attempts: 1,
            ..StepOutcome::passing(0, outputs, None)
        }
    }

    /// Outcome for an approval gate that a human rejected — the gate FAILS, and the reviewer's
    /// note is surfaced as `outputs.feedback` (the input to act on) and the failure reason.
    fn gate_rejected(d: &ApprovalDecision) -> Self {
        let note = d.note.clone().unwrap_or_default();
        let reason = if note.is_empty() {
            format!("rejected by {}", d.approver)
        } else {
            format!("rejected by {}: {note}", d.approver)
        };
        let mut outputs = IndexMap::new();
        outputs.insert("decision".to_owned(), Value::String("rejected".to_owned()));
        outputs.insert("approver".to_owned(), Value::String(d.approver.clone()));
        outputs.insert("feedback".to_owned(), Value::String(note));
        StepOutcome {
            outputs,
            attempts: 1,
            ..StepOutcome::failed(reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr as _;

    use crate::api::RunInput;
    use crate::engine::{Engine, EngineBuilder};
    use crate::error::Error;
    use crate::mock::{EchoProvider, FailingProvider};
    use crate::storage::SqliteStore;
    use crate::traits::{AcquireCtx, LoopProgress, PrunePolicy};
    use crate::workspace::WorktreeWorkspace;
    use crate::workspace::testutil::init_repo;
    use serde_json::json;

    fn parse(yaml: &str) -> Workflow {
        Workflow::from_yaml_str(yaml).unwrap()
    }

    /// A minimal `RunState` for exercising the mirror directly.
    fn mk_mirror_state(status: RunStatus) -> RunState {
        RunState {
            run_id: RunId::new(),
            workflow: crate::ids::WorkflowId::new("w"),
            schema_major: 1,
            status,
            error: None,
            steps: IndexMap::new(),
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: None,
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// An engine over `repo` with an `echo` provider registered alongside the built-ins.
    fn engine(repo: &Path, store: Arc<dyn Store>) -> Arc<dyn Engine> {
        let mut builder = EngineBuilder::new().repo(repo).store(store);
        builder
            .registry_mut()
            .register_provider(Arc::new(EchoProvider::new("echo")));
        builder.build().unwrap()
    }

    const HAPPY: &str = r#"
name: e2e
durable: true
workspace: { type: worktree }
params:
  who: { required: true }
steps:
  - id: greet
    provider: echo
    prompt: "hello {{ params.who }}"
  - id: edit
    run: "echo more >> README.md"
    depends_on: [greet]
    gates:
      file_exists: "test -f README.md"
  - id: review
    provider: echo
    prompt: "diff is:\n{{ artifacts.DIFF }}"
    depends_on: [edit]
"#;

    /// `--stream` (an engine [`StreamMux`]) tees a `run:` step's live output to the sink as it
    /// runs, prefixed by step id — while the output is still captured for the final summary.
    #[tokio::test]
    async fn stream_mux_tees_run_step_output_prefixed_by_step() {
        let repo = init_repo().await;
        let (mux, captured) = crate::provider::StreamMux::capturing();
        let eng = EngineBuilder::new()
            .repo(repo.path())
            .store(Arc::new(SqliteStore::open_in_memory().unwrap()))
            .stream(mux)
            .build()
            .unwrap();
        let wf = parse(
            "name: s\nworkspace: { type: worktree }\n\
             steps:\n  - {id: emit, run: \"echo streamed-marker\"}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(summary.status, RunStatus::Succeeded);
        let teed = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(
            teed.contains("emit │ streamed-marker"),
            "the step's output should be teed live, prefixed by step id: {teed:?}"
        );
    }

    /// A deliberately portable end-to-end canary: a single `echo` `run:` step — no path
    /// interpolation, no fragile shell syntax. Runs wherever a POSIX shell resolves (Unix `sh`,
    /// Git Bash on Windows), so the Windows CI lane uses it to prove the full
    /// workflow → engine → shell → git path works there.
    #[tokio::test]
    async fn cancel_all_active_stops_a_running_step_as_cancelled() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        assert_eq!(eng.cancel_all_active(), 0, "nothing running yet");
        assert!(
            !eng.cancel_run(RunId::new()),
            "an unknown run id can't be cancelled"
        );

        // A long-sleeping step in a NON-durable run; a graceful shutdown abandons it, so it ends
        // Cancelled promptly. (A *durable* run is instead suspended for resume — see
        // `shutdown_suspends_a_durable_run_then_resume_completes_it`.)
        let wf = parse(
            "name: c\ndurable: false\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"sleep 30\"}\n",
        );
        let eng2 = eng.clone();
        let handle = tokio::spawn(async move { eng2.run(&wf, RunInput::manual()).await });

        // Poll until the run registers as active, then fire its token.
        let mut fired = 0;
        for _ in 0..120 {
            fired = eng.cancel_all_active();
            if fired >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            fired >= 1,
            "the run should have been active and cancellable"
        );

        let summary = tokio::time::timeout(std::time::Duration::from_secs(20), handle)
            .await
            .expect("a cancelled run must finish promptly, not wait out the step")
            .unwrap()
            .unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Cancelled,
            "error: {:?}",
            summary.error
        );
        assert_eq!(
            eng.cancel_all_active(),
            0,
            "the registry is empty after the run ends"
        );
    }

    /// Graceful shutdown (`cancel_all_active`) of a **durable** run must SUSPEND it — leave it
    /// non-terminal and resumable — not strand it as terminal `Cancelled`. A subsequent
    /// `resume_all` completes it. This is the durability contract that `kill -9` already honored
    /// but a clean ctrl-C/redeploy previously broke.
    #[tokio::test]
    async fn shutdown_suspends_a_durable_run_then_resume_completes_it() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());

        // First step sleeps (so we can interrupt it mid-flight); a quick second step proves the
        // run completes after resume. The sleep is short enough that re-running it on resume is
        // cheap, but the token fires before the sleep even starts, so the first attempt is killed
        // immediately and deterministically.
        let wf = parse(
            "name: sd\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  \
             - {id: s, run: \"sleep 2\"}\n  - {id: done, run: \"true\", depends_on: [s]}\n",
        );
        let eng2 = eng.clone();
        let wf2 = wf.clone();
        let handle = tokio::spawn(async move { eng2.run(&wf2, RunInput::manual()).await });

        let mut fired = 0;
        for _ in 0..120 {
            fired = eng.cancel_all_active();
            if fired >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(fired >= 1, "the durable run should have been active");

        let suspended = tokio::time::timeout(std::time::Duration::from_secs(20), handle)
            .await
            .expect("a shutdown must suspend promptly, not wait out the sleep")
            .unwrap()
            .unwrap();
        assert_eq!(
            suspended.status,
            RunStatus::Running,
            "a durable run interrupted by shutdown is suspended (non-terminal), not Cancelled; \
             error: {:?}",
            suspended.error
        );

        // The store must show it resumable (non-terminal) — the exact thing the bug broke.
        let incomplete = store.load_incomplete().await.unwrap();
        assert!(
            incomplete.iter().any(|s| s.run_id == suspended.run_id),
            "the suspended run must be in load_incomplete so resume_all picks it up"
        );

        // Resume completes it. (A fresh, un-cancelled token is used for the resumed pass.)
        let resumed = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        let done = resumed
            .iter()
            .find(|s| s.run_id == suspended.run_id)
            .expect("resume_all must complete the suspended run");
        assert_eq!(done.status, RunStatus::Succeeded, "error: {:?}", done.error);
        assert!(
            store
                .load_incomplete()
                .await
                .unwrap()
                .iter()
                .all(|s| s.run_id != suspended.run_id),
            "the resumed run is terminal and no longer incomplete"
        );
    }

    /// A cross-process `odin cancel` (a request written to the store, as the CLI would, against a
    /// run executing here) stops the running durable run terminally — the watcher polls the store
    /// and fires the token.
    #[tokio::test]
    async fn a_store_cancel_request_stops_a_running_durable_run() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: xc\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"sleep 30\"}\n",
        );
        let eng2 = eng.clone();
        let wf2 = wf.clone();
        let handle = tokio::spawn(async move { eng2.run(&wf2, RunInput::manual()).await });

        let mut run_id = None;
        for _ in 0..120 {
            if let Some(s) = store
                .load_incomplete()
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.status == RunStatus::Running)
            {
                run_id = Some(s.run_id);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let run_id = run_id.expect("the run should have registered as Running");
        assert!(
            store.request_cancel(run_id).await.unwrap(),
            "a running run is cancellable via the store"
        );

        let summary = tokio::time::timeout(std::time::Duration::from_secs(15), handle)
            .await
            .expect("the watcher must cancel the run within a poll interval")
            .unwrap()
            .unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Cancelled,
            "error: {:?}",
            summary.error
        );
        // The audit/progress log records WHY it was cancelled (a user/store cancel, not a shutdown).
        let events = store.events(run_id).await.unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::RunCancelled {
                    reason: crate::traits::CancelReason::User,
                    ..
                }
            )),
            "missing RunCancelled(User): {events:?}"
        );
    }

    /// `resume_all` recovers MULTIPLE incomplete runs (concurrently) and completes them all — the
    /// daemon-restart recovery path. Suspends two durable runs via shutdown, then resumes both.
    #[tokio::test]
    async fn resume_all_recovers_multiple_runs() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: rm\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  \
             - {id: s, run: \"sleep 2\"}\n  - {id: done, run: \"true\", depends_on: [s]}\n",
        );

        // Start two runs and keep firing shutdown so each is suspended as it registers.
        let mut handles = Vec::new();
        for _ in 0..2 {
            let e = eng.clone();
            let w = wf.clone();
            handles.push(tokio::spawn(
                async move { e.run(&w, RunInput::manual()).await },
            ));
        }
        let killer = {
            let e = eng.clone();
            tokio::spawn(async move {
                for _ in 0..150 {
                    e.cancel_all_active();
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
            })
        };
        for h in handles {
            let s = tokio::time::timeout(std::time::Duration::from_secs(20), h)
                .await
                .expect("each run suspends promptly")
                .unwrap()
                .unwrap();
            assert_eq!(s.status, RunStatus::Running, "suspended, not terminal");
        }
        killer.abort(); // stop firing shutdown BEFORE resuming, or it would re-suspend them

        let resumed = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        assert_eq!(resumed.len(), 2, "both incomplete runs are recovered");
        assert!(
            resumed.iter().all(|s| s.status == RunStatus::Succeeded),
            "every recovered run completes: {:?}",
            resumed
                .iter()
                .map(|s| (s.run_id, s.status))
                .collect::<Vec<_>>()
        );
    }

    /// A USER cancel (`cancel_run`) of a durable run is TERMINAL `Cancelled` — it must NOT be
    /// resumed (the distinction from graceful shutdown above).
    #[tokio::test]
    async fn user_cancel_of_a_durable_run_is_terminal_not_resumed() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());

        let wf = parse(
            "name: uc\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  \
             - {id: s, run: \"sleep 30\"}\n",
        );
        let eng2 = eng.clone();
        let wf2 = wf.clone();
        let handle = tokio::spawn(async move { eng2.run(&wf2, RunInput::manual()).await });

        // Find the active run's id from the store, then user-cancel it specifically.
        let mut run_id = None;
        for _ in 0..120 {
            if let Some(s) = store
                .load_incomplete()
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.status == RunStatus::Running)
            {
                run_id = Some(s.run_id);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let run_id = run_id.expect("the run should have registered as Running");
        // The run row is checkpointed just before its cancel token registers, so retry briefly.
        let mut cancelled = false;
        for _ in 0..120 {
            if eng.cancel_run(run_id) {
                cancelled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(cancelled, "the active run must become cancellable");

        let summary = tokio::time::timeout(std::time::Duration::from_secs(20), handle)
            .await
            .expect("a cancelled run finishes promptly")
            .unwrap()
            .unwrap();
        assert_eq!(summary.status, RunStatus::Cancelled);
        // It must NOT be resumable.
        assert!(
            store
                .load_incomplete()
                .await
                .unwrap()
                .iter()
                .all(|s| s.run_id != run_id),
            "a user-cancelled run is terminal and never resumed"
        );
        assert!(
            eng.resume_all(std::slice::from_ref(&wf))
                .await
                .unwrap()
                .is_empty(),
            "resume_all must not pick up a user-cancelled run"
        );
    }

    #[tokio::test]
    async fn smoke_a_run_step_executes_cross_platform() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: smoke\nworkspace: { type: worktree }\nsteps:\n  - {id: hello, run: \"echo hello\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let hello = s.steps.iter().find(|x| x.id.as_str() == "hello").unwrap();
        assert_eq!(hello.status, StepStatus::Passed);
        assert!(
            hello
                .outputs
                .get("stdout")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .contains("hello")
        );
    }

    #[tokio::test]
    async fn shell_exec_failure_keeps_its_stderr() {
        // A failed `shell.exec` must surface its stderr in the step error (and so retry.feedback),
        // not lose it — the action now carries stderr through ActionOutcome.
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: w\nworkspace: { type: worktree }\nsteps:\n  - {id: s, action: shell.exec, with: {command: \"echo BOOM 1>&2; exit 1\"}}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Failed);
        let st = s.steps.iter().find(|x| x.id.as_str() == "s").unwrap();
        assert!(
            st.error.as_deref().unwrap_or_default().contains("BOOM"),
            "the failed shell.exec must keep its stderr: {:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn an_action_honors_the_step_timeout() {
        // Before the fix the action arm dropped the timeout and ran the command to completion;
        // now the step timeout reaches the subprocess and kills it. The command sleeps 10s under a
        // 1s timeout: honored, it returns at ~1s + the bounded kill-drain grace (a `sh`-forked
        // grandchild can hold the pipe up to `KILL_DRAIN_GRACE`); not honored, it runs the full
        // 10s. The wide gap keeps the `< 6s` bound robust on slow CI runners (incl. Windows, which
        // has no process-group teardown) while still failing loudly if the timeout is ignored.
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: w\nworkspace: { type: worktree }\nsteps:\n  - {id: s, action: shell.exec, with: {command: \"sleep 10\"}, timeout: \"1s\"}\n",
        );
        let start = std::time::Instant::now();
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Failed);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(6),
            "the action timeout was not honored (took {:?})",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn runs_a_workflow_end_to_end() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store);
        let summary = eng
            .run(&parse(HAPPY), RunInput::manual().param("who", "world"))
            .await
            .unwrap();

        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        assert_eq!(summary.steps.len(), 3);
        assert!(summary.steps.iter().all(|s| s.status == StepStatus::Passed));

        // The provider received the rendered prompt.
        let greet = summary
            .steps
            .iter()
            .find(|s| s.id.as_str() == "greet")
            .unwrap();
        assert_eq!(greet.outputs["stdout"], json!("hello world"));

        // The shell step's edit shows up in the auto-captured DIFF...
        assert!(
            summary.diff.as_deref().unwrap_or("").contains("more"),
            "diff: {:?}",
            summary.diff
        );
        // ...and that DIFF flowed into the later step's templated prompt.
        let review = summary
            .steps
            .iter()
            .find(|s| s.id.as_str() == "review")
            .unwrap();
        assert!(review.outputs["stdout"].as_str().unwrap().contains("more"));

        // It was persisted.
        let loaded = eng.summary(summary.run_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Succeeded);
    }

    #[tokio::test]
    async fn a_failing_step_fails_the_run_and_skips_dependents() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store);
        let wf = parse(
            "name: failing\nworkspace: { type: worktree }\nsteps:\n  - {id: boom, run: \"exit 7\"}\n  - {id: after, run: \"true\", depends_on: [boom]}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();

        assert_eq!(summary.status, RunStatus::Failed);
        let boom = summary
            .steps
            .iter()
            .find(|s| s.id.as_str() == "boom")
            .unwrap();
        assert_eq!(boom.status, StepStatus::Failed);
        assert_eq!(boom.exit_code, Some(7));
        let after = summary
            .steps
            .iter()
            .find(|s| s.id.as_str() == "after")
            .unwrap();
        assert_eq!(
            after.status,
            StepStatus::Skipped,
            "dependent of a failed step is skipped"
        );
    }

    /// An engine over `repo` with both the `echo` provider and a `FailingProvider` registered under
    /// `boom` — for exercising the provider-failure paths the always-succeeding echo can't reach.
    fn engine_with_failing(
        repo: &Path,
        mode_provider: Arc<dyn crate::traits::Provider>,
    ) -> Arc<dyn Engine> {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mut builder = EngineBuilder::new().repo(repo).store(store);
        builder
            .registry_mut()
            .register_provider(Arc::new(EchoProvider::new("echo")))
            .register_provider(mode_provider);
        builder.build().unwrap()
    }

    #[tokio::test]
    async fn a_provider_error_fails_the_run_and_skips_dependents() {
        // The provider's `invoke` returns Err (a crashed/missing CLI, an API error) — the engine
        // must fail the step with the error surfaced, and skip its dependents.
        let repo = init_repo().await;
        let eng = engine_with_failing(repo.path(), Arc::new(FailingProvider::error("boom")));
        let wf = parse(
            "name: pf\nworkspace: { type: worktree }\nsteps:\n  - {id: bad, provider: boom, prompt: hi}\n  - {id: after, run: \"true\", depends_on: [bad]}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Failed);
        let bad = s.steps.iter().find(|x| x.id.as_str() == "bad").unwrap();
        assert_eq!(bad.status, StepStatus::Failed);
        assert!(
            s.error
                .as_deref()
                .unwrap_or_default()
                .contains("provider error"),
            "the run error should name the provider failure: {:?}",
            s.error
        );
        let after = s.steps.iter().find(|x| x.id.as_str() == "after").unwrap();
        assert_eq!(after.status, StepStatus::Skipped);
    }

    #[tokio::test]
    async fn a_provider_nonzero_exit_fails_the_step() {
        // The provider returns Ok but a non-zero exit (the agent ran and reported failure — e.g. a
        // normalized claude is_error) — the engine must still fail the step.
        let repo = init_repo().await;
        let eng = engine_with_failing(repo.path(), Arc::new(FailingProvider::exit("boom", 3)));
        let wf = parse(
            "name: pe\nworkspace: { type: worktree }\nsteps:\n  - {id: bad, provider: boom, prompt: hi}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Failed);
        let bad = s.steps.iter().find(|x| x.id.as_str() == "bad").unwrap();
        assert_eq!(bad.status, StepStatus::Failed);
        assert_eq!(bad.exit_code, Some(3), "the non-zero exit code is recorded");
    }

    #[tokio::test]
    async fn failed_step_records_reason_and_stderr() {
        // A failed step must surface WHY it failed — the exit code + the stderr tail — on the
        // StepResult, in the persisted StepState, and in the run-level summary.error.
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store);
        let wf = parse(
            "name: f\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: boom, run: \"echo the-real-error 1>&2; exit 3\"}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();

        assert_eq!(summary.status, RunStatus::Failed);
        let boom = summary
            .steps
            .iter()
            .find(|s| s.id.as_str() == "boom")
            .unwrap();
        assert_eq!(boom.status, StepStatus::Failed);
        let err = boom.error.as_deref().expect("step error is recorded");
        assert!(
            err.contains("exited with code 3"),
            "exit code in error: {err}"
        );
        assert!(
            err.contains("the-real-error"),
            "stderr folded into error: {err}"
        );

        // The run-level error names the failed step and its reason (not a bare placeholder).
        let run_err = summary.error.as_deref().expect("summary.error is set");
        assert!(
            run_err.contains("boom") && run_err.contains("exited with code 3"),
            "summary.error: {run_err}"
        );

        // And it survives a reload from the store (StepState.error is persisted).
        let reloaded = eng.summary(summary.run_id).await.unwrap().unwrap();
        let boom2 = reloaded
            .steps
            .iter()
            .find(|s| s.id.as_str() == "boom")
            .unwrap();
        assert!(
            boom2
                .error
                .as_deref()
                .unwrap_or("")
                .contains("the-real-error"),
            "persisted error survives reload"
        );
    }

    #[tokio::test]
    async fn when_false_skips_the_step() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store);
        let wf = parse(
            "name: cond\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"true\"}\n  - {id: maybe, run: \"false\", depends_on: [a], when: \"false\"}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();

        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "skipped step must not fail the run"
        );
        let maybe = summary
            .steps
            .iter()
            .find(|s| s.id.as_str() == "maybe")
            .unwrap();
        assert_eq!(maybe.status, StepStatus::Skipped);
    }

    #[tokio::test]
    async fn when_gate_reads_upstream_step_status() {
        // Regression: a `when:` guard must see an upstream step's status as the
        // snake_case string the docs and examples promise — `steps.<id>.status == 'passed'`.
        // (The previously-shipped `steps.<id>.outputs.passed == true` referenced an output
        // the engine never sets, so the gate silently evaluated false and skipped the step.)
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store);
        let wf = parse(
            "name: gate\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"true\"}\n  - {id: gated, run: \"true\", depends_on: [a], when: \"steps.a.status == 'passed'\"}\n  - {id: ungated, run: \"true\", depends_on: [a], when: \"steps.a.status == 'failed'\"}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();

        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        let status = |id: &str| {
            summary
                .steps
                .iter()
                .find(|s| s.id.as_str() == id)
                .unwrap()
                .status
        };
        assert_eq!(
            status("gated"),
            StepStatus::Passed,
            "`status == 'passed'` gate must fire after the upstream step passed"
        );
        assert_eq!(
            status("ungated"),
            StepStatus::Skipped,
            "`status == 'failed'` gate must not fire when the upstream step passed"
        );
    }

    #[tokio::test]
    async fn run_rejects_a_param_whose_value_mismatches_its_type() {
        // A declared `type: number` param given a string is a typed-input error at run start.
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store);
        let wf = parse(
            "name: t\nworkspace: { type: worktree }\nparams:\n  n: {type: number}\nsteps:\n  - {id: a, run: \"true\"}\n",
        );
        let err = eng
            .run(&wf, RunInput::manual().param("n", "not-a-number"))
            .await
            .expect_err("a mistyped param must be rejected");
        assert!(
            matches!(err, Error::Input(_)),
            "expected Error::Input, got: {err:?}"
        );

        // The same workflow with a correctly-typed value runs fine.
        let summary = eng
            .run(&wf, RunInput::manual().param("n", 42))
            .await
            .unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
    }

    #[tokio::test]
    async fn registered_workspace_overrides_the_builtin_kind() {
        // A custom Workspace registered under a built-in kind ("worktree") must be used by the
        // engine in place of the built-in — the live path for `Registry::register_workspace`.
        use crate::error::WorkspaceError;
        use crate::traits::{AcquireCtx, Workspace, WorkspaceHandle};
        use std::sync::atomic::{AtomicBool, Ordering};

        struct RecordingWorkspace {
            dir: std::path::PathBuf,
            used: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl Workspace for RecordingWorkspace {
            #[allow(clippy::unnecessary_literal_bound)]
            fn kind(&self) -> &str {
                "worktree"
            }
            async fn acquire(
                &self,
                ctx: AcquireCtx,
            ) -> std::result::Result<WorkspaceHandle, WorkspaceError> {
                self.used.store(true, Ordering::SeqCst);
                Ok(WorkspaceHandle::new(ctx.run_id, self.dir.clone(), None, ""))
            }
            async fn release(&self, _: WorkspaceHandle) -> std::result::Result<(), WorkspaceError> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let used = Arc::new(AtomicBool::new(false));
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mut builder = EngineBuilder::new().repo(repo.path()).store(store);
        builder
            .registry_mut()
            .register_workspace(Arc::new(RecordingWorkspace {
                dir: dir.path().to_path_buf(),
                used: used.clone(),
            }));
        let eng = builder.build().unwrap();

        let wf =
            parse("name: ws\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"true\"}\n");
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();

        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        assert!(
            used.load(Ordering::SeqCst),
            "the registered workspace must override the built-in worktree kind"
        );
    }

    #[tokio::test]
    async fn resume_continues_an_incomplete_run() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(HAPPY);

        // Craft a half-finished run: `greet` already passed, on a real worktree.
        let ws = WorktreeWorkspace::new(repo.path());
        let run_id = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id,
                config: wf.workspace.clone(),
            })
            .await
            .unwrap();
        let mut steps = IndexMap::new();
        steps.insert(
            StepId::new("greet"),
            StepState {
                status: StepStatus::Passed,
                attempts: 1,
                exit_code: Some(0),
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: None,
            },
        );
        let state = RunState {
            run_id,
            workflow: wf.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps,
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual().param("who", "resumed"),
            workspace: Some(handle),
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        assert_eq!(s.steps.len(), 3);
        let by = |id: &str| s.steps.iter().find(|x| x.id.as_str() == id).unwrap().status;
        assert_eq!(
            by("greet"),
            StepStatus::Passed,
            "pre-completed step preserved"
        );
        assert_eq!(
            by("edit"),
            StepStatus::Passed,
            "remaining steps executed on resume"
        );
        assert_eq!(by("review"), StepStatus::Passed);
    }

    #[tokio::test]
    async fn resume_reclaims_a_leftover_snapshot_ref_when_durable_was_flipped_off() {
        // A durable run that crashed leaves refs/odin/run/<id> behind plus a persisted state.
        // Resuming it against a workflow whose `durable` is now false must STILL reclaim that
        // ref (cleanup is unconditional, matching run()) — the old durable-gated cleanup leaked
        // it. (Non-durable runs aren't persisted, so a flipped flag is how this state arises.)
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let nondurable = parse(
            "name: rr\ndurable: false\nworkspace: { type: worktree }\nsteps:\n  - {id: first, provider: echo, prompt: hi}\n  - {id: rest, run: \"true\", depends_on: [first]}\n",
        );

        let ws = WorktreeWorkspace::new(repo.path());
        let run_id = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id,
                config: nondurable.workspace.clone(),
            })
            .await
            .unwrap();

        // Seed the leftover ref a crashed durable run would have left in the shared repo.
        let snapshot_ref = format!("refs/odin/run/{run_id}");
        let seeded = std::process::Command::new("git")
            .args([
                "-C",
                repo.path().to_str().unwrap(),
                "update-ref",
                &snapshot_ref,
                "HEAD",
            ])
            .status()
            .unwrap()
            .success();
        assert!(seeded, "seed the leftover snapshot ref");

        let mut steps = IndexMap::new();
        steps.insert(
            StepId::new("first"),
            StepState {
                status: StepStatus::Passed,
                attempts: 1,
                exit_code: Some(0),
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: None,
            },
        );
        let state = RunState {
            run_id,
            workflow: nondurable.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps,
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: Some(handle),
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng
            .resume_all(std::slice::from_ref(&nondurable))
            .await
            .unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].status,
            RunStatus::Succeeded,
            "error: {:?}",
            summaries[0].error
        );

        let still_there = std::process::Command::new("git")
            .args([
                "-C",
                repo.path().to_str().unwrap(),
                "show-ref",
                "--verify",
                "--quiet",
                &snapshot_ref,
            ])
            .status()
            .unwrap()
            .success();
        assert!(
            !still_there,
            "resume must reclaim the leftover snapshot ref even for a non-durable workflow"
        );
    }

    #[tokio::test]
    async fn resume_reconstructs_persisted_side_effects_in_the_summary() {
        // A side effect recorded by a step that FINISHED before the crash must still appear in
        // the resumed run's summary — that step isn't re-run, so the effect comes from the
        // persisted StepState.side_effects, not from re-execution.
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: se\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: first, provider: echo, prompt: hi}\n  - {id: rest, run: \"true\", depends_on: [first]}\n",
        );
        let ws = WorktreeWorkspace::new(repo.path());
        let run_id = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id,
                config: wf.workspace.clone(),
            })
            .await
            .unwrap();
        let mut steps = IndexMap::new();
        steps.insert(
            StepId::new("first"),
            StepState {
                status: StepStatus::Passed,
                attempts: 1,
                exit_code: Some(0),
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: vec![SideEffect::commit("abc123", Some("main".to_owned()))],
                error: None,
            },
        );
        let state = RunState {
            run_id,
            workflow: wf.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps,
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: Some(handle),
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].status,
            RunStatus::Succeeded,
            "error: {:?}",
            summaries[0].error
        );
        assert!(
            summaries[0]
                .side_effects
                .iter()
                .any(|s| matches!(s, SideEffect::Commit { .. })),
            "the pre-crash side effect must be reconstructed from persisted state: {:?}",
            summaries[0].side_effects
        );
    }

    #[tokio::test]
    async fn resume_keeps_a_failed_step_failed_and_its_dependents_skipped() {
        // A step that FAILED before the crash (retries exhausted) must NOT re-run on resume —
        // re-running would grant attempts beyond its policy and, since its dependents are recorded
        // Skipped, leave them frozen-skipped if it now passed. Here `b`'s command is `true` (it
        // would PASS if re-run), so the buggy behavior resumes to Succeeded; the fix keeps `b`
        // Failed and `c` Skipped → the run stays Failed, and `b`'s pre-crash side effect survives.
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: f\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"true\"}\n  - {id: b, run: \"true\", depends_on: [a]}\n  - {id: c, run: \"true\", depends_on: [b]}\n",
        );
        let ws = WorktreeWorkspace::new(repo.path());
        let run_id = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id,
                config: wf.workspace.clone(),
            })
            .await
            .unwrap();
        let mut steps = IndexMap::new();
        let passed = |code: i32| StepState {
            status: StepStatus::Passed,
            attempts: 1,
            exit_code: Some(code),
            outputs: IndexMap::new(),
            usage: None,
            gates: IndexMap::new(),
            judge_score: None,
            side_effects: Vec::new(),
            error: None,
        };
        steps.insert(StepId::new("a"), passed(0));
        steps.insert(
            StepId::new("b"),
            StepState {
                status: StepStatus::Failed,
                attempts: 2,
                exit_code: Some(1),
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: vec![SideEffect::commit("def456", Some("main".to_owned()))],
                error: Some("boom".to_owned()),
            },
        );
        steps.insert(
            StepId::new("c"),
            StepState {
                status: StepStatus::Skipped,
                attempts: 0,
                exit_code: None,
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: Some("an upstream dependency did not pass".to_owned()),
            },
        );
        let state = RunState {
            run_id,
            workflow: wf.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps,
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: Some(handle),
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(
            s.status,
            RunStatus::Failed,
            "a settled failure must stay failed"
        );
        let by = |id: &str| s.steps.iter().find(|x| x.id.as_str() == id).unwrap();
        assert_eq!(by("b").status, StepStatus::Failed, "b must not re-run");
        assert_eq!(by("c").status, StepStatus::Skipped, "c must stay skipped");
        assert!(
            s.side_effects
                .iter()
                .any(|e| matches!(e, SideEffect::Commit { .. })),
            "the failed step's pre-crash side effect must survive: {:?}",
            s.side_effects
        );
    }

    #[tokio::test]
    async fn resume_restores_workdir_so_an_appending_step_is_not_double_applied() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        // One durable, idempotency-sensitive step: appending twice would be wrong.
        let wf = parse(
            "name: idem\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - { id: append, run: \"echo line >> f.txt\" }\n",
        );

        let ws = WorktreeWorkspace::new(repo.path());
        let run_id = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id,
                config: wf.workspace.clone(),
            })
            .await
            .unwrap();
        let wpath = handle.path.clone();
        // Simulate a crash MID-step: a partial append is already on disk and `append` is
        // still `Running`. base_commit is the clean HEAD the restore should rewind to.
        std::fs::write(wpath.join("f.txt"), "line\n").unwrap();
        let base = String::from_utf8(
            std::process::Command::new("git")
                .current_dir(&wpath)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();

        let mut steps = IndexMap::new();
        steps.insert(
            StepId::new("append"),
            StepState {
                status: StepStatus::Running,
                attempts: 1,
                exit_code: None,
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: None,
            },
        );
        let state = RunState {
            run_id,
            workflow: wf.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps,
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: Some(handle),
            base_commit: Some(base),
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        // The DIFF must show `line` added exactly once: the pre-crash partial edit was
        // discarded by the resume restore, then `append` re-applied a single time. Without
        // the restore the file would hold two `line`s.
        let diff = s.diff.as_deref().unwrap_or_default();
        assert_eq!(
            diff.matches("+line").count(),
            1,
            "expected one added line (no double-apply), diff:\n{diff}"
        );
    }

    #[tokio::test]
    async fn resume_does_not_rewind_past_a_passed_step_when_its_snapshot_is_missing() {
        // The data-loss blocker: a non-scratch step PASSED but its best-effort snapshot
        // failed (state.snapshot is None). Resume must NOT rewind to base and delete that
        // step's completed work — it should skip the restore entirely.
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: nodataloss\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - { id: a, run: \"echo aaa > a.txt\" }\n  - { id: b, run: \"echo bbb > b.txt\", depends_on: [a] }\n",
        );

        let ws = WorktreeWorkspace::new(repo.path());
        let run_id = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id,
                config: wf.workspace.clone(),
            })
            .await
            .unwrap();
        let wpath = handle.path.clone();
        // `a` already passed and produced a.txt; its snapshot is MISSING (None).
        std::fs::write(wpath.join("a.txt"), "aaa\n").unwrap();
        let base = String::from_utf8(
            std::process::Command::new("git")
                .current_dir(&wpath)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let mut steps = IndexMap::new();
        steps.insert(
            StepId::new("a"),
            StepState {
                status: StepStatus::Passed,
                attempts: 1,
                exit_code: Some(0),
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: None,
            },
        );
        let state = RunState {
            run_id,
            workflow: wf.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps,
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: Some(handle),
            base_commit: Some(base),
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        let s = &summaries[0];
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        // Both files must be present: `a`'s work survived (no rewind to base), and `b` ran.
        let diff = s.diff.as_deref().unwrap_or_default();
        assert!(
            diff.contains("a.txt"),
            "a's work was discarded! diff:\n{diff}"
        );
        assert!(diff.contains("b.txt"), "b did not run. diff:\n{diff}");
    }

    #[tokio::test]
    async fn durable_workflow_that_commits_mid_run_succeeds_with_a_base_relative_diff() {
        // A step that COMMITs moves HEAD off base; snapshotting must disengage (not rewind
        // the branch), and the DIFF must stay cumulative-vs-base (committed + uncommitted).
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: commits\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - { id: commit_step, run: \"echo f > f.txt && git add -A && git commit -q -m s1\" }\n  - { id: more, run: \"echo g > g.txt\", depends_on: [commit_step] }\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        // DIFF vs base shows the committed file AND the later uncommitted one.
        let diff = summary.diff.as_deref().unwrap_or_default();
        assert!(
            diff.contains("f.txt"),
            "committed file missing from DIFF:\n{diff}"
        );
        assert!(
            diff.contains("g.txt"),
            "later file missing from DIFF:\n{diff}"
        );
    }

    #[tokio::test]
    async fn resume_does_not_rewind_past_a_commit_when_the_snapshot_is_stale() {
        // The re-review blocker: a step commits (HEAD leaves base) then crashes BEFORE the
        // post-step block clears state.snapshot, so the persisted snapshot is a STALE
        // pre-commit pointer. Resume must NOT restore to it (that would rewind the worktree
        // below HEAD and corrupt the run branch) — the at-base guard must skip it.
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: commitcrash\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - { id: committer, run: \"true\" }\n  - { id: after, run: \"echo a > after.txt\", depends_on: [committer] }\n",
        );

        let ws = WorktreeWorkspace::new(repo.path());
        let run_id = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id,
                config: wf.workspace.clone(),
            })
            .await
            .unwrap();
        let wpath = handle.path.clone();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(&wpath)
                .args(args)
                .output()
                .unwrap()
        };
        let base = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        // `committer` already passed AND committed b.txt — HEAD has moved off base.
        std::fs::write(wpath.join("b.txt"), "bbb\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "s1"]);

        let mut steps = IndexMap::new();
        steps.insert(
            StepId::new("committer"),
            StepState {
                status: StepStatus::Passed,
                attempts: 1,
                exit_code: Some(0),
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: None,
            },
        );
        let state = RunState {
            run_id,
            workflow: wf.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps,
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: Some(handle),
            base_commit: Some(base.clone()),
            // STALE: points at the pre-commit base, but HEAD has advanced past it.
            snapshot: Some(base),
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        let s = &summaries[0];
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        // The committed file must survive (not rewound away), and `after` must have run.
        let diff = s.diff.as_deref().unwrap_or_default();
        assert!(
            diff.contains("b.txt"),
            "committed work was rewound away — branch corrupted! diff:\n{diff}"
        );
        assert!(
            diff.contains("after.txt"),
            "`after` did not run. diff:\n{diff}"
        );
    }

    #[tokio::test]
    async fn resume_uses_the_persisted_diff() {
        // greet+edit pre-completed with a persisted DIFF; only `review` (which templates
        // {{ artifacts.DIFF }}) remains. The DIFF must come from persisted state, since
        // already-passed steps cannot re-capture it. (Regression for the resume blocker.)
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(HAPPY);

        let ws = WorktreeWorkspace::new(repo.path());
        let run_id = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id,
                config: wf.workspace.clone(),
            })
            .await
            .unwrap();
        let mut steps = IndexMap::new();
        for id in ["greet", "edit"] {
            steps.insert(
                StepId::new(id),
                StepState {
                    status: StepStatus::Passed,
                    attempts: 1,
                    exit_code: Some(0),
                    outputs: IndexMap::new(),
                    usage: None,
                    gates: IndexMap::new(),
                    judge_score: None,
                    side_effects: Vec::new(),
                    error: None,
                },
            );
        }
        let mut artifacts = IndexMap::new();
        artifacts.insert(
            crate::ids::ArtifactName::new(DIFF),
            "+a tracked change\n".to_owned(),
        );
        let state = RunState {
            run_id,
            workflow: wf.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps,
            artifacts,
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual().param("who", "resumed"),
            workspace: Some(handle),
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        let review = summaries[0]
            .steps
            .iter()
            .find(|s| s.id.as_str() == "review")
            .unwrap();
        assert_eq!(
            summaries[0].status,
            RunStatus::Succeeded,
            "error: {:?}",
            summaries[0].error
        );
        assert_eq!(review.status, StepStatus::Passed);
        assert!(
            review.outputs["stdout"]
                .as_str()
                .unwrap()
                .contains("a tracked change")
        );
    }

    #[tokio::test]
    async fn diff_captures_newly_created_files() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: newfile\nworkspace: { type: worktree }\nsteps:\n  - {id: make, run: \"echo content > brand-new.txt\"}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(summary.status, RunStatus::Succeeded);
        assert!(
            summary
                .diff
                .as_deref()
                .unwrap_or("")
                .contains("brand-new.txt"),
            "diff must include the created file, got: {:?}",
            summary.diff
        );
    }

    #[tokio::test]
    async fn resume_fails_a_run_whose_workspace_is_gone_without_aborting() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(HAPPY);
        let run_id = RunId::new();
        let state = RunState {
            run_id,
            workflow: wf.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps: IndexMap::new(),
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual().param("who", "x"),
            workspace: Some(crate::traits::WorkspaceHandle {
                run_id,
                path: repo.path().join(".odin/worktrees/does-not-exist"),
                branch: None,
                token: "x".to_owned(),
            }),
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].status, RunStatus::Failed);
        assert!(
            summaries[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("workspace is gone")
        );
        // Marked terminal, so it won't be re-resumed forever.
        assert!(store.load_incomplete().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reloaded_summary_surfaces_the_failure() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store);
        let wf = parse(
            "name: boom\nworkspace: { type: worktree }\nsteps:\n  - {id: x, run: \"exit 2\"}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(summary.status, RunStatus::Failed);

        let reloaded = eng.summary(summary.run_id).await.unwrap().unwrap();
        assert_eq!(reloaded.status, RunStatus::Failed);
        assert!(
            reloaded.error.is_some(),
            "a reloaded failed run must surface its error"
        );
    }

    #[tokio::test]
    async fn action_step_commits_and_records_a_side_effect() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: act\nworkspace: { type: worktree }\nsteps:\n  - {id: edit, run: \"echo more >> README.md\"}\n  - id: save\n    action: git.commit\n    with: { message: \"automated change\" }\n    depends_on: [edit]\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        assert!(
            summary
                .side_effects
                .iter()
                .any(|s| matches!(s, SideEffect::Commit { .. })),
            "expected a Commit side-effect, got {:?}",
            summary.side_effects
        );
    }

    #[tokio::test]
    async fn reloaded_summary_reconstructs_side_effects_from_persisted_state() {
        // A consumer reading a finished run by id via `summary(run_id)` must see its side
        // effects, reconstructed from persisted StepState — not just the live `run()` return.
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: act\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: edit, run: \"echo more >> README.md\"}\n  - id: save\n    action: git.commit\n    with: { message: \"automated change\" }\n    depends_on: [edit]\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );

        let reloaded = eng.summary(summary.run_id).await.unwrap().unwrap();
        assert!(
            reloaded
                .side_effects
                .iter()
                .any(|s| matches!(s, SideEffect::Commit { .. })),
            "reloaded summary must reconstruct the Commit side-effect: {:?}",
            reloaded.side_effects
        );
    }

    const APPROVAL_WF: &str = "name: appr\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: plan, run: \"echo planned > plan.txt\"}\n  - id: gate\n    approval: { message: \"ok to proceed?\" }\n    depends_on: [plan]\n  - {id: ship, run: \"echo shipped > ship.txt\", depends_on: [gate]}\n";

    fn step_status(s: &RunSummary, id: &str) -> Option<StepStatus> {
        s.steps
            .iter()
            .find(|x| x.id.as_str() == id)
            .map(|x| x.status)
    }

    const CASE_WF: &str = "name: casewf\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: classify, run: \"echo bug\"}\n  - id: route\n    depends_on: [classify]\n    case:\n      branches:\n        - {label: bug,  when: \"steps.classify.outputs.stdout | trim == 'bug'\"}\n        - {label: docs, when: \"steps.classify.outputs.stdout | trim == 'docs'\"}\n      else: other\n  - {id: fix,   run: \"echo fixing\",  depends_on: [route], when: \"steps.route.outputs.selected == 'bug'\"}\n  - {id: write, run: \"echo writing\", depends_on: [route], when: \"steps.route.outputs.selected == 'docs'\"}\n  - {id: done,  run: \"echo done\",    depends_on: [route], when: \"steps.route.outputs.selected == 'bug'\"}\n";

    #[tokio::test]
    async fn case_selects_one_branch_and_join_merges_back() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(CASE_WF);
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);

        // The selector ran and recorded the decision.
        assert_eq!(step_status(&s, "route"), Some(StepStatus::Passed));
        let route = s.steps.iter().find(|x| x.id.as_str() == "route").unwrap();
        assert_eq!(
            route.outputs.get("selected").and_then(|v| v.as_str()),
            Some("bug")
        );

        // Exactly the `bug` branch ran; the `docs` branch skipped.
        assert_eq!(step_status(&s, "fix"), Some(StepStatus::Passed));
        assert_eq!(step_status(&s, "write"), Some(StepStatus::Skipped));
        // The join depends on the (always-passing) selector + gates on the decision → it runs.
        assert_eq!(step_status(&s, "done"), Some(StepStatus::Passed));
    }

    #[tokio::test]
    async fn case_falls_through_to_else_when_no_guard_matches() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // classify prints "feature", which matches neither bug nor docs → else `other`.
        let wf = parse(&CASE_WF.replace("echo bug", "echo feature"));
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let route = s.steps.iter().find(|x| x.id.as_str() == "route").unwrap();
        assert_eq!(
            route.outputs.get("selected").and_then(|v| v.as_str()),
            Some("other")
        );
        // Neither branch ran (both gated on bug/docs).
        assert_eq!(step_status(&s, "fix"), Some(StepStatus::Skipped));
        assert_eq!(step_status(&s, "write"), Some(StepStatus::Skipped));
    }

    #[tokio::test]
    async fn case_selects_the_first_matching_branch() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // Both guards are true; the FIRST branch (alpha) must win.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - id: r\n    case:\n      branches:\n        - {label: alpha, when: \"1 == 1\"}\n        - {label: beta,  when: \"2 == 2\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        let r = s.steps.iter().find(|x| x.id.as_str() == "r").unwrap();
        assert_eq!(
            r.outputs.get("selected").and_then(|v| v.as_str()),
            Some("alpha")
        );
    }

    #[tokio::test]
    async fn case_with_no_match_and_no_else_selects_empty() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: c, run: \"echo zzz\"}\n  - id: r\n    depends_on: [c]\n    case:\n      branches:\n        - {label: bug, when: \"steps.c.outputs.stdout | trim == 'bug'\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        let r = s.steps.iter().find(|x| x.id.as_str() == "r").unwrap();
        assert_eq!(
            r.outputs.get("selected").and_then(|v| v.as_str()),
            Some(""),
            "no guard matched and no else ⇒ selected is empty"
        );
        assert_eq!(s.status, RunStatus::Succeeded); // the selector still passes
    }

    #[tokio::test]
    async fn retry_feedback_injects_the_prior_failure_into_the_next_attempt() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // Attempt 1 fails (exit 1); attempt 2 echoes `retry.feedback`, which must carry the prior
        // failure reason ("exited with code 1") — proving the feedback loop, not a blind re-run.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - id: flaky\n    run: \"if [ {{ retry.attempt }} -eq 1 ]; then exit 1; fi; echo 'fb=[{{ retry.feedback }}]'\"\n    retry: { max: 1, feedback: concise }\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let flaky = s.steps.iter().find(|x| x.id.as_str() == "flaky").unwrap();
        assert_eq!(flaky.attempts, 2, "should pass on the second attempt");
        let stdout = flaky
            .outputs
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            stdout.contains("exited with code 1"),
            "retry.feedback should carry the prior failure; stdout: {stdout:?}"
        );
    }

    #[tokio::test]
    async fn retry_attempt_is_one_with_feedback_off() {
        // Without feedback, `retry.attempt` is still available (== 1 on the only attempt) and
        // `retry.feedback` is empty.
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"echo a=[{{ retry.attempt }}] f=[{{ retry.feedback }}]\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        let st = s.steps.iter().find(|x| x.id.as_str() == "s").unwrap();
        let stdout = st
            .outputs
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            stdout.contains("a=[1]") && stdout.contains("f=[]"),
            "stdout: {stdout:?}"
        );
    }

    #[tokio::test]
    async fn retry_feedback_carries_gate_stdout_not_just_the_label() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // The gate writes its diagnostic to STDOUT (as test runners do) and fails on attempt 1,
        // then passes on attempt 2. `retry.feedback` must carry that stdout — not the bare
        // `gate "check" failed` label — or a self-correct loop has nothing to act on.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - id: s\n    run: \"echo 'fb=[{{ retry.feedback }}]'\"\n    gates:\n      check: \"if [ {{ retry.attempt }} -eq 1 ]; then echo 'ASSERT_FAIL left=4 right=5'; exit 1; fi\"\n    retry: { max: 1, feedback: verbose }\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let st = s.steps.iter().find(|x| x.id.as_str() == "s").unwrap();
        assert_eq!(st.attempts, 2);
        let stdout = st
            .outputs
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            stdout.contains("ASSERT_FAIL left=4 right=5"),
            "gate stdout should reach retry.feedback; stdout: {stdout:?}"
        );
    }

    #[tokio::test]
    async fn retry_root_resolves_in_a_when_guard_without_failing() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // A step-level `when:` is evaluated once, before any attempt, against the base context.
        // `retry.*` must still resolve there (pre-attempt: attempt 1, empty feedback) rather than
        // erroring under strict undefined-behavior and failing a workflow that validates clean.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"echo ok\", when: \"retry.attempt == 1 and retry.feedback == ''\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let st = s.steps.iter().find(|x| x.id.as_str() == "s").unwrap();
        assert_eq!(
            st.status,
            StepStatus::Passed,
            "the guard should be true, not error"
        );
    }

    // ── loop: execution ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn loop_converges_on_a_later_iteration() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // `check` fails until the 3rd iteration (`loop.counter >= 3`), then passes; `until` then
        // holds. The loop reports it converged on iteration 3.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - id: fix\n    loop:\n      until: \"steps.check.status == 'passed'\"\n      max: 5\n      steps:\n        - {id: check, run: \"[ {{ loop.counter }} -ge 3 ]\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let fix = s.steps.iter().find(|x| x.id.as_str() == "fix").unwrap();
        assert_eq!(fix.status, StepStatus::Passed);
        assert_eq!(
            fix.outputs
                .get("iterations")
                .and_then(serde_json::Value::as_u64),
            Some(3),
            "should converge on the 3rd iteration"
        );
        assert_eq!(
            fix.outputs
                .get("converged")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn loop_exhausts_max_and_fails() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // `until` never holds → the loop fails once `max` iterations elapse → the run fails.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - id: fix\n    loop:\n      until: \"false\"\n      max: 2\n      steps:\n        - {id: noop, run: \"true\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Failed);
        let fix = s.steps.iter().find(|x| x.id.as_str() == "fix").unwrap();
        assert_eq!(fix.status, StepStatus::Failed);
        assert!(
            fix.error
                .as_deref()
                .unwrap_or_default()
                .contains("within 2"),
            "error: {:?}",
            fix.error
        );
        // A hit-the-cap failure is still introspectable.
        assert_eq!(
            fix.outputs
                .get("iterations")
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
        assert_eq!(
            fix.outputs
                .get("converged")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
    }

    #[tokio::test]
    async fn loop_accumulates_edits_across_iterations() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // Each iteration appends a line to the SHARED workdir; convergence needs 3 lines. If the
        // loop rewound between iterations (like retry), the file would reset and never reach 3.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - id: fix\n    loop:\n      until: \"steps.check.status == 'passed'\"\n      max: 6\n      steps:\n        - {id: append, run: \"echo x >> acc.txt\"}\n        - {id: check, run: \"[ $(wc -l < acc.txt) -ge 3 ]\", depends_on: [append]}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let fix = s.steps.iter().find(|x| x.id.as_str() == "fix").unwrap();
        assert_eq!(
            fix.outputs
                .get("iterations")
                .and_then(serde_json::Value::as_u64),
            Some(3),
            "accumulating edits should converge on the 3rd iteration"
        );
    }

    #[tokio::test]
    async fn loop_feedback_carries_to_the_next_iteration() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // The gate passes only if `loop.feedback` carries "UNLOCK"; iteration 1 fails and emits
        // UNLOCK to stderr (which becomes the next iteration's feedback), so converging at all
        // proves the prior failure was fed forward.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - id: fix\n    loop:\n      until: \"steps.gate.status == 'passed'\"\n      max: 4\n      steps:\n        - {id: gate, run: \"if echo '{{ loop.feedback }}' | grep -q UNLOCK; then exit 0; else echo UNLOCK >&2; exit 1; fi\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let fix = s.steps.iter().find(|x| x.id.as_str() == "fix").unwrap();
        assert_eq!(
            fix.outputs
                .get("iterations")
                .and_then(serde_json::Value::as_u64),
            Some(2),
            "should converge on iteration 2 once the feedback carries UNLOCK"
        );
    }

    #[tokio::test]
    async fn loop_inner_step_reads_an_outer_step() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // The inner step interpolates an OUTER step's output; if that ref didn't resolve, the
        // command would fail and the loop would never converge.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: setup, run: \"echo hello\"}\n  - id: fix\n    depends_on: [setup]\n    loop:\n      until: \"steps.use.status == 'passed'\"\n      max: 1\n      steps:\n        - {id: use, run: \"test '{{ steps.setup.outputs.stdout | trim }}' = hello\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
    }

    #[tokio::test]
    async fn loop_clears_its_progress_when_it_settles() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        // A loop that takes a few iterations writes per-iteration loop_state, which must be cleared
        // when it converges — otherwise a later, unrelated resume would see stale progress.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - id: fix\n    loop:\n      until: \"loop.counter >= 3\"\n      max: 5\n      steps:\n        - {id: noop, run: \"true\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let persisted = store.load_run(s.run_id).await.unwrap().unwrap();
        assert!(
            persisted.loop_state.is_empty(),
            "loop_state must be cleared once the loop settles: {:?}",
            persisted.loop_state
        );
    }

    #[tokio::test]
    async fn loop_resumes_from_the_last_completed_iteration() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nparams:\n  logfile: { type: string, required: true }\nsteps:\n  - id: fix\n    loop:\n      until: \"steps.check.status == 'passed'\"\n      max: 10\n      steps:\n        - {id: log, run: \"echo x >> {{ params.logfile }}\"}\n        - {id: check, run: \"[ {{ loop.counter }} -ge 4 ]\", depends_on: [log]}\n",
        );

        let ws = WorktreeWorkspace::new(repo.path());
        let run_id = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id,
                config: wf.workspace.clone(),
            })
            .await
            .unwrap();
        let wpath = handle.path.clone();
        let base = String::from_utf8(
            std::process::Command::new("git")
                .current_dir(&wpath)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();

        // An external log (outside the worktree, so untouched by the snapshot/restore) records
        // every body run; seeded with 2 lines as if iterations 1-2 ran before the crash.
        let logfile = std::env::temp_dir().join(format!("odin-loop-log-{}", uuid::Uuid::new_v4()));
        std::fs::write(&logfile, "L\nL\n").unwrap();
        // Forward slashes so the absolute path is a literal in the shell `run:` command — a
        // backslash is an escape in `sh` (and Windows accepts `/` in paths). No-op on Unix.
        let logfile_arg = logfile.to_string_lossy().replace('\\', "/");

        // Crash state: `fix` is Running with 2 iterations completed; the snapshot is the base
        // commit itself (a valid commit → restore is a no-op, but `start` is set to 2 so the loop
        // re-enters at iteration 3).
        let mut loop_state = IndexMap::new();
        loop_state.insert(
            StepId::new("fix"),
            LoopProgress {
                last_completed_iteration: 2,
                iteration_snapshot: Some(base.clone()),
                feedback: None,
            },
        );
        let mut steps = IndexMap::new();
        steps.insert(
            StepId::new("fix"),
            StepState {
                status: StepStatus::Running,
                attempts: 1,
                exit_code: None,
                outputs: IndexMap::new(),
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: None,
            },
        );
        let state = RunState {
            run_id,
            workflow: wf.name.clone(),
            schema_major: 1,
            status: RunStatus::Running,
            error: None,
            steps,
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual().param("logfile", logfile_arg.as_str()),
            workspace: Some(handle),
            base_commit: Some(base),
            snapshot: None,
            loop_state,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.checkpoint(&state).await.unwrap();

        let summaries = eng.resume_all(std::slice::from_ref(&wf)).await.unwrap();
        assert_eq!(
            summaries[0].status,
            RunStatus::Succeeded,
            "error: {:?}",
            summaries[0].error
        );
        let fix = summaries[0]
            .steps
            .iter()
            .find(|x| x.id.as_str() == "fix")
            .unwrap();
        assert_eq!(
            fix.outputs
                .get("iterations")
                .and_then(serde_json::Value::as_u64),
            Some(4),
            "the counter must continue from the resumed iteration"
        );
        let lines = std::fs::read_to_string(&logfile).unwrap().lines().count();
        let _ = std::fs::remove_file(&logfile);
        assert_eq!(
            lines, 4,
            "only iterations 3-4 should re-run (2 pre-crash + 2 = 4); a from-scratch restart \
             would re-run all four and give 6"
        );
    }

    #[tokio::test]
    async fn approval_gate_pauses_then_resumes_on_approve() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(APPROVAL_WF);

        // First run: stops AT the gate, downstream not yet run.
        let s1 = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            s1.status,
            RunStatus::AwaitingApproval,
            "the run must pause at the gate"
        );
        assert_eq!(step_status(&s1, "plan"), Some(StepStatus::Passed));
        assert_eq!(step_status(&s1, "gate"), Some(StepStatus::AwaitingApproval));
        assert_eq!(
            step_status(&s1, "ship"),
            None,
            "downstream must not run while paused"
        );

        // Approve → resumes → ship runs → Succeeded.
        let s2 = eng
            .submit_approval(
                s1.run_id,
                Decision::Approved,
                "alice".to_owned(),
                None,
                std::slice::from_ref(&wf),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s2.status, RunStatus::Succeeded, "error: {:?}", s2.error);
        assert_eq!(step_status(&s2, "gate"), Some(StepStatus::Passed));
        assert_eq!(step_status(&s2, "ship"), Some(StepStatus::Passed));
    }

    #[tokio::test]
    async fn a_non_durable_run_emits_no_events() {
        // A non-durable run is never checkpointed (no `runs` row), so emitting events would orphan
        // them — invisible to `status` and unreclaimable by `prune`. `emit` is gated on durability.
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        // Explicitly non-durable (the default is durable), but a store IS present.
        let wf = parse(
            "name: w\ndurable: false\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"true\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        assert!(
            store.events(s.run_id).await.unwrap().is_empty(),
            "a non-durable run must not write orphan events"
        );
    }

    #[tokio::test]
    async fn prune_gcs_the_per_run_worktree_branch() {
        // `WorktreeWorkspace::release` keeps the `odin/run/<id>` branch (it may hold committed
        // work to push/PR); once the run record is pruned the branch is dead and must be GC'd,
        // else these branches accumulate without bound for every run ever served.
        fn branch_exists(repo: &std::path::Path, branch: &str) -> bool {
            let out = std::process::Command::new("git")
                .current_dir(repo)
                .args(["branch", "--list", branch])
                .output()
                .unwrap();
            !String::from_utf8_lossy(&out.stdout).trim().is_empty()
        }
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"true\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        let branch = format!("odin/run/{}", s.run_id);
        assert!(
            branch_exists(repo.path(), &branch),
            "the per-run branch should exist after the run (release keeps it)"
        );
        eng.prune(
            &PrunePolicy {
                keep_last: Some(0),
                ..PrunePolicy::default()
            },
            false,
        )
        .await
        .unwrap();
        assert!(
            !branch_exists(repo.path(), &branch),
            "prune must GC the dead per-run worktree branch"
        );
    }

    #[tokio::test]
    async fn an_approved_gate_emits_paired_start_finish_events() {
        // A resolved approval gate must emit a StepStarted to pair with its StepFinished, so an
        // audit consumer sees a complete lifecycle for the gate (it used to emit only Finished).
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(APPROVAL_WF);
        let s1 = eng.run(&wf, RunInput::manual()).await.unwrap();
        eng.submit_approval(
            s1.run_id,
            Decision::Approved,
            "alice".to_owned(),
            None,
            std::slice::from_ref(&wf),
        )
        .await
        .unwrap()
        .unwrap();
        let events = store.events(s1.run_id).await.unwrap();
        assert!(
            events.iter().any(
                |e| matches!(e, RunEvent::StepStarted { step, .. } if step.as_str() == "gate")
            ),
            "approval gate missing StepStarted: {events:?}"
        );
        assert!(
            events.iter().any(
                |e| matches!(e, RunEvent::StepFinished { step, .. } if step.as_str() == "gate")
            ),
            "approval gate missing StepFinished: {events:?}"
        );
    }

    /// The `on_event` hook fires the full lifecycle for a **non-durable** run — the run has no
    /// store and leaves no event log, yet a push-based embedder still sees every transition.
    #[tokio::test]
    async fn on_event_hook_fires_for_a_non_durable_run() {
        let repo = init_repo().await;
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let mut builder = EngineBuilder::new()
            .repo(repo.path()) // NB: no .store()
            .on_event(move |_id, ev| captured.lock().unwrap().push(ev.clone()));
        builder
            .registry_mut()
            .register_provider(Arc::new(EchoProvider::new("echo")));
        let eng = builder.build().unwrap();
        let wf = parse(
            "name: h\ndurable: false\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"true\"}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(summary.status, RunStatus::Succeeded);

        let evs = events.lock().unwrap();
        assert!(
            evs.iter().any(|e| matches!(e, RunEvent::RunStarted { .. })),
            "hook missed RunStarted: {evs:?}"
        );
        assert!(
            evs.iter()
                .any(|e| matches!(e, RunEvent::StepStarted { step, .. } if step.as_str() == "s")),
            "hook missed StepStarted: {evs:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(
                e,
                RunEvent::StepFinished {
                    status: StepStatus::Passed,
                    ..
                }
            )),
            "hook missed a passed StepFinished: {evs:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(
                e,
                RunEvent::RunFinished {
                    status: RunStatus::Succeeded,
                    ..
                }
            )),
            "hook missed RunFinished: {evs:?}"
        );
    }

    /// A panicking `on_event` callback must never abort the run — the panic is caught and logged.
    #[tokio::test]
    async fn a_panicking_on_event_hook_does_not_kill_the_run() {
        let repo = init_repo().await;
        let mut builder = EngineBuilder::new()
            .repo(repo.path())
            .on_event(|_id, _ev| panic!("boom from a bad callback"));
        builder
            .registry_mut()
            .register_provider(Arc::new(EchoProvider::new("echo")));
        let eng = builder.build().unwrap();
        let wf = parse(
            "name: hp\ndurable: false\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"true\"}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "a panicking hook must not abort the run"
        );
    }

    /// An approval decision and the subsequent resume emit the new `ApprovalDecided` (who/what/note)
    /// and `RunResumed` audit events.
    #[tokio::test]
    async fn approval_emits_decided_and_resumed_events() {
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(APPROVAL_WF);
        let s1 = eng.run(&wf, RunInput::manual()).await.unwrap();
        eng.submit_approval(
            s1.run_id,
            Decision::Approved,
            "alice".to_owned(),
            Some("lgtm".to_owned()),
            std::slice::from_ref(&wf),
        )
        .await
        .unwrap()
        .unwrap();
        let events = store.events(s1.run_id).await.unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::ApprovalDecided { decision: Decision::Approved, approver, note: Some(n), .. }
                    if approver == "alice" && n == "lgtm"
            )),
            "missing ApprovalDecided: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RunEvent::RunResumed { .. })),
            "missing RunResumed: {events:?}"
        );
    }

    /// The in-memory mirror makes a **non-durable** run visible to `recent()`/`summary()` even
    /// though the store never persisted it.
    #[tokio::test]
    async fn recent_and_summary_see_a_non_durable_run() {
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: nd\ndurable: false\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"true\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded);
        // The store never saw it (non-durable)…
        assert!(
            store.load_run(s.run_id).await.unwrap().is_none(),
            "the store must not hold a non-durable run"
        );
        assert!(store.recent(10).await.unwrap().is_empty());
        // …but the engine's mirror does.
        let summary = eng
            .summary(s.run_id)
            .await
            .unwrap()
            .expect("the mirror should hold the run");
        assert_eq!(summary.status, RunStatus::Succeeded);
        assert!(
            summary.finished_at.is_some(),
            "a terminal run has finished_at"
        );
        let recent = eng.recent(10).await.unwrap();
        assert!(
            recent.iter().any(|v| v.run_id == s.run_id.to_string()),
            "engine.recent() must list the non-durable run: {recent:?}"
        );
    }

    /// `summary()` reports `finished_at: None` for a still-running (mirrored) run — no fabricated
    /// end time, the bug the mirror's in-flight visibility would otherwise expose.
    #[tokio::test]
    async fn summary_finished_at_is_none_while_running() {
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: nf\ndurable: false\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"sleep 2\"}\n",
        );
        let eng2 = eng.clone();
        let wf2 = wf.clone();
        let handle = tokio::spawn(async move { eng2.run(&wf2, RunInput::manual()).await });

        // Grab the run id as soon as it appears (any status), then poll summary directly: while it
        // reports Running (the ~2s sleep gives a wide window), finished_at must be None.
        let mut run_id = None;
        for _ in 0..120 {
            if let Some(v) = eng.recent(10).await.unwrap().first() {
                run_id = Some(RunId::from_str(&v.run_id).unwrap());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        let run_id = run_id.expect("the non-durable run should appear in the mirror");
        let mut checked = false;
        for _ in 0..120 {
            let summary = eng.summary(run_id).await.unwrap().unwrap();
            if summary.status == RunStatus::Running {
                assert!(
                    summary.finished_at.is_none(),
                    "an in-flight run must not report finished_at"
                );
                checked = true;
            } else {
                break; // terminal — done observing the running window
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        assert!(checked, "should have observed the run while Running");
        handle.await.unwrap().unwrap();
    }

    /// A **durable** run is never mirrored (it's persisted) — guards the `if durable && store`
    /// predicate so a regression can't duplicate durable runs into `recent()`.
    #[tokio::test]
    async fn a_durable_run_with_a_store_is_not_mirrored() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mut reg = Registry::with_builtins();
        reg.register_provider(Arc::new(EchoProvider::new("echo")));
        let eng = LocalEngine::new(
            reg,
            Some(store.clone()),
            repo.path().to_path_buf(),
            None,
            None,
        );
        let wf = parse(
            "name: dm\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"true\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded);
        assert!(
            store.load_run(s.run_id).await.unwrap().is_some(),
            "in the store"
        );
        assert!(
            eng.mirror.lock().unwrap().is_empty(),
            "a durable, persisted run must NOT be mirrored"
        );
    }

    /// A **durable** run with NO store configured is still visible via the mirror — the
    /// persisted-vs-durable predicate fix (otherwise it would be invisible to `summary()`/`recent()`).
    #[tokio::test]
    async fn a_durable_run_without_a_store_is_mirrored() {
        let repo = init_repo().await;
        let mut reg = Registry::with_builtins();
        reg.register_provider(Arc::new(EchoProvider::new("echo")));
        let eng = LocalEngine::new(reg, None, repo.path().to_path_buf(), None, None);
        let wf = parse(
            "name: dns\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"true\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded);
        let summary = eng.summary(s.run_id).await.unwrap();
        assert!(
            summary.is_some(),
            "a durable, store-less run must be visible via the mirror"
        );
        assert!(
            eng.recent(10)
                .await
                .unwrap()
                .iter()
                .any(|v| v.run_id == s.run_id.to_string())
        );
    }

    /// The mirror is bounded: inserting past `MIRROR_CAP` keeps the count capped and evicts
    /// terminal entries first, never a still-in-flight one (until all slots are in-flight).
    #[tokio::test]
    async fn mirror_is_bounded_and_evicts_terminal_first() {
        let repo = init_repo().await;
        let eng = LocalEngine::new(
            Registry::with_builtins(),
            None,
            repo.path().to_path_buf(),
            None,
            None,
        );
        // One in-flight run, then flood with terminal runs past the cap.
        let live = mk_mirror_state(RunStatus::Running);
        eng.mirror_put(&live);
        for _ in 0..MIRROR_CAP + 50 {
            eng.mirror_put(&mk_mirror_state(RunStatus::Succeeded));
        }
        let mirror = eng.mirror.lock().unwrap();
        assert!(
            mirror.len() <= MIRROR_CAP,
            "mirror exceeded its cap: {}",
            mirror.len()
        );
        assert!(
            mirror.contains_key(&live.run_id),
            "the in-flight run must survive eviction of terminal runs"
        );
    }

    /// `light_snapshot` drops the heavy fields (step output, diff, payload), caps a large param,
    /// but keeps an approval gate's `message` so a paused run still shows it.
    #[test]
    fn light_snapshot_drops_heavy_keeps_gate_message_and_caps_params() {
        use crate::ids::StepId;
        let mut state = mk_mirror_state(RunStatus::AwaitingApproval);
        let mut outputs = IndexMap::new();
        outputs.insert("message".to_owned(), json!("ok to ship?"));
        outputs.insert("stdout".to_owned(), json!("X".repeat(2_000_000)));
        state.steps.insert(
            StepId::new("gate"),
            StepState {
                status: StepStatus::AwaitingApproval,
                attempts: 0,
                exit_code: None,
                outputs,
                usage: None,
                gates: IndexMap::new(),
                judge_score: None,
                side_effects: Vec::new(),
                error: None,
            },
        );
        state.artifacts.insert(DIFF.into(), "Y".repeat(2_000_000));
        state.input.trigger_payload = json!({ "huge": "Z".repeat(1_000_000) });
        state
            .input
            .params
            .insert("big".to_owned(), json!("Z".repeat(10_000)));
        state.input.params.insert("small".to_owned(), json!("ok"));

        let light = LocalEngine::light_snapshot(&state);
        assert!(light.artifacts.is_empty(), "DIFF dropped");
        assert_eq!(
            light.input.trigger_payload,
            serde_json::Value::Null,
            "payload dropped"
        );
        let gate = &light.steps[&StepId::new("gate")];
        assert!(gate.outputs.contains_key("message"), "gate message kept");
        assert!(!gate.outputs.contains_key("stdout"), "step stdout dropped");
        assert_eq!(light.input.params["small"], json!("ok"), "small param kept");
        assert!(
            light.input.params["big"]
                .as_str()
                .unwrap()
                .contains("omitted"),
            "large param capped: {:?}",
            light.input.params["big"]
        );
    }

    /// A clean (non-cancelled) run emits NO `RunCancelled`, and `RunStarted`/`RunFinished` bracket
    /// the event stream — the absence/ordering guarantees the cancel tests don't cover.
    #[tokio::test]
    async fn a_clean_run_brackets_events_and_emits_no_cancelled() {
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: clean\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"true\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded);
        let events = store.events(s.run_id).await.unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RunEvent::RunCancelled { .. })),
            "a clean run must not emit RunCancelled: {events:?}"
        );
        assert!(
            matches!(events.first(), Some(RunEvent::RunStarted { .. })),
            "first event must be RunStarted: {events:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(RunEvent::RunFinished {
                    status: RunStatus::Succeeded,
                    ..
                })
            ),
            "last event must be RunFinished(Succeeded): {events:?}"
        );
    }

    /// A USER cancel of a durable run ends with exactly `[.. RunCancelled{User}, RunFinished]` in
    /// that order — the ordering/exactly-once guarantee for the cancel path.
    #[tokio::test]
    async fn cancel_emits_run_cancelled_immediately_before_run_finished() {
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: co\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"sleep 30\"}\n",
        );
        let eng2 = eng.clone();
        let wf2 = wf.clone();
        let handle = tokio::spawn(async move { eng2.run(&wf2, RunInput::manual()).await });
        let mut run_id = None;
        for _ in 0..120 {
            if let Some(s) = store
                .load_incomplete()
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.status == RunStatus::Running)
            {
                run_id = Some(s.run_id);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let run_id = run_id.expect("running");
        for _ in 0..120 {
            if eng.cancel_run(run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        tokio::time::timeout(std::time::Duration::from_secs(20), handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let events = store.events(run_id).await.unwrap();
        let n_cancelled = events
            .iter()
            .filter(|e| matches!(e, RunEvent::RunCancelled { .. }))
            .count();
        assert_eq!(n_cancelled, 1, "exactly one RunCancelled: {events:?}");
        // RunCancelled is the second-to-last event; RunFinished is last.
        let len = events.len();
        assert!(
            matches!(
                events[len - 2],
                RunEvent::RunCancelled {
                    reason: crate::traits::CancelReason::User,
                    ..
                }
            ) && matches!(
                events[len - 1],
                RunEvent::RunFinished {
                    status: RunStatus::Cancelled,
                    ..
                }
            ),
            "tail must be [RunCancelled(User), RunFinished(Cancelled)]: {events:?}"
        );
    }

    /// A durable run hit by a graceful shutdown emits `RunSuspended{Shutdown}` (it's resumable),
    /// NOT `RunCancelled` — the distinction the review flagged as missing.
    #[tokio::test]
    async fn durable_shutdown_emits_suspended_not_cancelled() {
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: sd\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"sleep 30\"}\n",
        );
        let eng2 = eng.clone();
        let wf2 = wf.clone();
        let handle = tokio::spawn(async move { eng2.run(&wf2, RunInput::manual()).await });
        let mut fired = 0;
        for _ in 0..120 {
            fired = eng.cancel_all_active();
            if fired >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(fired >= 1);
        let suspended = tokio::time::timeout(std::time::Duration::from_secs(20), handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            suspended.status,
            RunStatus::Running,
            "suspended, not terminal"
        );
        let events = store.events(suspended.run_id).await.unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::RunSuspended {
                    reason: crate::traits::SuspendReason::Shutdown,
                    ..
                }
            )),
            "missing RunSuspended(Shutdown): {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RunEvent::RunCancelled { .. })),
            "a suspended durable run must not emit RunCancelled: {events:?}"
        );
    }

    /// Pausing at an approval gate emits `RunSuspended{Approval}`; a rejection records
    /// `ApprovalDecided{Rejected}`.
    #[tokio::test]
    async fn approval_gate_emits_suspended_and_rejection_decided() {
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(APPROVAL_WF);
        let s1 = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s1.status, RunStatus::AwaitingApproval);
        let events = store.events(s1.run_id).await.unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::RunSuspended {
                    reason: crate::traits::SuspendReason::Approval,
                    ..
                }
            )),
            "missing RunSuspended(Approval): {events:?}"
        );

        eng.submit_approval(
            s1.run_id,
            Decision::Rejected,
            "bob".to_owned(),
            Some("nope".to_owned()),
            std::slice::from_ref(&wf),
        )
        .await
        .unwrap()
        .unwrap();
        let events = store.events(s1.run_id).await.unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::ApprovalDecided { decision: Decision::Rejected, approver, .. }
                    if approver == "bob"
            )),
            "missing ApprovalDecided(Rejected, bob): {events:?}"
        );
    }

    #[tokio::test]
    async fn loop_emits_paired_start_finish_for_node_and_inner_steps() {
        // The loop node and each inner step must each emit a paired StepStarted + StepFinished —
        // previously the node had only Finished and inner steps had only Started.
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - id: fix\n    loop:\n      until: \"steps.check.status == 'passed'\"\n      max: 3\n      steps:\n        - {id: check, run: \"true\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let events = store.events(s.run_id).await.unwrap();
        for id in ["fix", "check"] {
            assert!(
                events.iter().any(
                    |e| matches!(e, RunEvent::StepStarted { step, .. } if step.as_str() == id)
                ),
                "missing StepStarted for {id:?}: {events:?}"
            );
            assert!(
                events.iter().any(
                    |e| matches!(e, RunEvent::StepFinished { step, .. } if step.as_str() == id)
                ),
                "missing StepFinished for {id:?}: {events:?}"
            );
        }
    }

    // Timing-sensitive: a slow Windows `git worktree add` can serialize the two scratch
    // acquisitions enough to close the overlap window (see `independent_scratch_steps_run_concurrently`).
    #[tokio::test]
    #[cfg(not(windows))]
    async fn an_exclusive_step_does_not_block_scratch_steps_behind_it() {
        // `order = [a (scratch, 0.5s), gate (exclusive, instant), c (scratch, 0.5s)]`. The exclusive
        // `gate` sits between two independent scratch steps. With head-of-line blocking it stops `c`
        // from starting beside `a` (serializing them); now `a` and `c` overlap and `gate` waits for
        // the workdir to clear. We prove overlap from the StepStarted/StepFinished *timestamps*
        // rather than total wall-clock, so it can't flake under parallel test load.
        let repo = init_repo().await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(
            "name: par\ndurable: true\nworkspace: { type: worktree }\nmax_parallel: 3\nsteps:\n  - { id: a, run: \"sleep 0.5\", scratch: true }\n  - { id: gate, run: \"true\" }\n  - { id: c, run: \"sleep 0.5\", scratch: true }\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let events = store.events(s.run_id).await.unwrap();
        let started = |id: &str| {
            events.iter().find_map(|e| match e {
                RunEvent::StepStarted { step, at, .. } if step.as_str() == id => Some(*at),
                _ => None,
            })
        };
        let finished = |id: &str| {
            events.iter().find_map(|e| match e {
                RunEvent::StepFinished { step, at, .. } if step.as_str() == id => Some(*at),
                _ => None,
            })
        };
        let (a_start, a_fin) = (started("a").unwrap(), finished("a").unwrap());
        let (c_start, c_fin) = (started("c").unwrap(), finished("c").unwrap());
        // Two intervals overlap iff each begins before the other ends.
        assert!(
            c_start < a_fin && a_start < c_fin,
            "scratch steps must overlap, not serialize behind the exclusive gate: \
             a {a_start:?}..{a_fin:?}, c {c_start:?}..{c_fin:?}"
        );
    }

    #[tokio::test]
    async fn approval_gate_reject_fails_with_feedback_and_skips_downstream() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(APPROVAL_WF);
        let s1 = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s1.status, RunStatus::AwaitingApproval);

        let s2 = eng
            .submit_approval(
                s1.run_id,
                Decision::Rejected,
                "bob".to_owned(),
                Some("needs tests".to_owned()),
                std::slice::from_ref(&wf),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s2.status, RunStatus::Failed);
        let gate = s2.steps.iter().find(|x| x.id.as_str() == "gate").unwrap();
        assert_eq!(gate.status, StepStatus::Failed);
        assert!(
            gate.error
                .as_deref()
                .unwrap_or("")
                .contains("rejected by bob"),
            "reason carries the rejector: {:?}",
            gate.error
        );
        assert_eq!(
            gate.outputs.get("feedback").and_then(|v| v.as_str()),
            Some("needs tests"),
            "the reviewer's note is surfaced as feedback"
        );
        assert_eq!(
            step_status(&s2, "ship"),
            Some(StepStatus::Skipped),
            "downstream skips after a rejected gate"
        );
    }

    #[tokio::test]
    async fn approval_gate_survives_a_restart_then_resumes() {
        let repo = init_repo().await;
        let db = tempfile::tempdir().unwrap();
        let path = db.path().join("state.db");
        let wf = parse(APPROVAL_WF);

        let run_id = {
            let eng = engine(repo.path(), Arc::new(SqliteStore::open(&path).unwrap()));
            let s1 = eng.run(&wf, RunInput::manual()).await.unwrap();
            assert_eq!(s1.status, RunStatus::AwaitingApproval);
            s1.run_id
        }; // engine + store dropped — simulates a daemon restart while paused.

        let eng = engine(repo.path(), Arc::new(SqliteStore::open(&path).unwrap()));
        // A paused run must NOT be auto-resumed by crash recovery...
        assert!(
            eng.resume_all(std::slice::from_ref(&wf))
                .await
                .unwrap()
                .is_empty(),
            "a run parked at an approval gate is not crash-resumed"
        );
        // ...only once a decision is recorded.
        let s2 = eng
            .submit_approval(
                run_id,
                Decision::Approved,
                "alice".to_owned(),
                None,
                std::slice::from_ref(&wf),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s2.status, RunStatus::Succeeded, "error: {:?}", s2.error);
    }

    #[tokio::test]
    async fn submit_approval_with_a_missing_workflow_errors_without_bricking_the_run() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(APPROVAL_WF);
        let s1 = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s1.status, RunStatus::AwaitingApproval);

        // Deciding without the run's workflow provided must error and NOT mutate the run...
        let err = eng
            .submit_approval(s1.run_id, Decision::Approved, "alice".to_owned(), None, &[])
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("workflow"),
            "should refuse a missing workflow, got: {err}"
        );
        // ...so a CORRECT approval still works (the run wasn't left stuck).
        let s2 = eng
            .submit_approval(
                s1.run_id,
                Decision::Approved,
                "alice".to_owned(),
                None,
                std::slice::from_ref(&wf),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s2.status, RunStatus::Succeeded, "error: {:?}", s2.error);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_concurrent_approvals_resume_the_run_exactly_once() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(APPROVAL_WF);
        let s1 = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s1.status, RunStatus::AwaitingApproval);
        let run_id = s1.run_id;

        // Two decisions land at once (e.g. a double-submitted HTTP approve). Without the per-run
        // claim, both would pass the status check and each resume the run — running `ship` twice.
        let (e1, e2, w1, w2) = (eng.clone(), eng.clone(), wf.clone(), wf.clone());
        let t1 = tokio::spawn(async move {
            e1.submit_approval(run_id, Decision::Approved, "a".to_owned(), None, &[w1])
                .await
        });
        let t2 = tokio::spawn(async move {
            e2.submit_approval(run_id, Decision::Approved, "b".to_owned(), None, &[w2])
                .await
        });
        let (r1, r2) = tokio::join!(t1, t2);
        let (r1, r2) = (r1.unwrap(), r2.unwrap());

        let ok = |r: &Result<Option<RunSummary>>| matches!(r, Ok(Some(_)));
        assert_eq!(
            usize::from(ok(&r1)) + usize::from(ok(&r2)),
            1,
            "exactly one approval should resume the run; got r1={r1:?} r2={r2:?}"
        );
        // And the run reached its terminal state exactly once.
        let final_summary = eng.summary(run_id).await.unwrap().unwrap();
        assert_eq!(final_summary.status, RunStatus::Succeeded);
    }

    #[tokio::test]
    async fn reject_and_rerun_fails_the_original_and_starts_a_fresh_run_with_feedback() {
        let repo = init_repo().await;
        let store: Arc<dyn crate::traits::Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());
        let wf = parse(APPROVAL_WF);

        // Start with a param so we can prove the rerun inherits the original input.
        let mut input = RunInput::manual();
        input.params.insert("task".to_owned(), json!("original"));
        let s1 = eng.run(&wf, input).await.unwrap();
        assert_eq!(s1.status, RunStatus::AwaitingApproval);

        let outcome = eng
            .reject_and_rerun(
                s1.run_id,
                "bob".to_owned(),
                "needs tests".to_owned(),
                std::slice::from_ref(&wf),
            )
            .await
            .unwrap()
            .unwrap();

        // The original run is now failed at the rejected gate.
        assert_eq!(outcome.rejected.run_id, s1.run_id);
        assert_eq!(outcome.rejected.status, RunStatus::Failed);
        // The rerun is a DISTINCT run that paused again at its own gate.
        assert_ne!(outcome.rerun.run_id, s1.run_id);
        assert_eq!(outcome.rerun.status, RunStatus::AwaitingApproval);
        // It carries the feedback as a param, alongside the original run's params.
        let rerun = store.load_run(outcome.rerun.run_id).await.unwrap().unwrap();
        assert_eq!(
            rerun.input.params.get("feedback").and_then(|v| v.as_str()),
            Some("needs tests"),
            "the reject note is injected as the feedback param"
        );
        assert_eq!(
            rerun.input.params.get("task").and_then(|v| v.as_str()),
            Some("original"),
            "the rerun inherits the original run's params"
        );
    }

    #[tokio::test]
    async fn reject_and_rerun_refuses_a_misdeclared_feedback_param_without_failing_the_run() {
        // `reject --rerun` injects the note as a STRING. A workflow that declares `feedback` as a
        // different type would make the rerun fail param validation — so we must refuse BEFORE the
        // destructive reject, leaving the run still resumable.
        const BAD_FEEDBACK_WF: &str = "name: badfb\ndurable: true\nworkspace: { type: worktree }\nparams:\n  feedback: { type: number }\nsteps:\n  - {id: plan, run: \"echo planned\"}\n  - id: gate\n    approval: {}\n    depends_on: [plan]\n";
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(BAD_FEEDBACK_WF);
        let s1 = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s1.status, RunStatus::AwaitingApproval);

        let err = eng
            .reject_and_rerun(
                s1.run_id,
                "bob".to_owned(),
                "redo".to_owned(),
                std::slice::from_ref(&wf),
            )
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("feedback"),
            "should explain the misdeclared feedback param, got: {err}"
        );
        // The reject must NOT have fired — the run is still awaiting and can be decided normally.
        let still = eng.summary(s1.run_id).await.unwrap().unwrap();
        assert_eq!(still.status, RunStatus::AwaitingApproval);
    }

    #[tokio::test]
    async fn prune_removes_the_run_and_reclaims_its_snapshot_ref() {
        let repo = init_repo().await;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let eng = engine(repo.path(), store.clone());

        // A terminal run persisted in the store, with a leftover snapshot ref in the shared repo
        // (the historical flag-flip leak this also sweeps).
        let mut state = RunState {
            run_id: RunId::new(),
            workflow: crate::ids::WorkflowId::new("pw"),
            schema_major: 1,
            status: RunStatus::Succeeded,
            error: None,
            steps: IndexMap::new(),
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: None,
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        state.updated_at = Utc::now();
        let run_id = state.run_id;
        store.checkpoint(&state).await.unwrap();
        let snapshot_ref = format!("refs/odin/run/{run_id}");
        assert!(
            git_in(repo.path(), &["update-ref", &snapshot_ref, "HEAD"]),
            "seed the leftover ref"
        );

        let report = eng
            .prune(
                &PrunePolicy {
                    keep_last: Some(0),
                    ..PrunePolicy::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(report.runs_pruned, 1);
        assert!(report.run_ids.contains(&run_id));
        assert!(
            store.load_run(run_id).await.unwrap().is_none(),
            "run removed from store"
        );
        assert!(
            !git_in(
                repo.path(),
                &["show-ref", "--verify", "--quiet", &snapshot_ref]
            ),
            "the snapshot ref was reclaimed"
        );
    }

    /// Runs `git -C <repo> <args>` for a test, returning whether it succeeded.
    fn git_in(repo: &Path, args: &[&str]) -> bool {
        let mut full = vec!["-C", repo.to_str().unwrap()];
        full.extend_from_slice(args);
        std::process::Command::new("git")
            .args(&full)
            .status()
            .unwrap()
            .success()
    }

    /// A provider that always replies with a fixed string (a stand-in judge/agent).
    struct FixedProvider {
        id: &'static str,
        reply: String,
    }

    #[async_trait::async_trait]
    impl crate::traits::Provider for FixedProvider {
        fn id(&self) -> crate::ids::ProviderRef {
            crate::ids::ProviderRef::new(self.id)
        }
        async fn invoke(
            &self,
            _ctx: crate::traits::InvocationCtx,
        ) -> std::result::Result<crate::traits::InvocationOutcome, crate::error::ProviderError>
        {
            Ok(crate::traits::InvocationOutcome::success(
                self.reply.clone(),
            ))
        }
    }

    fn engine_with(repo: &Path, store: Arc<dyn Store>, scorer_reply: &str) -> Arc<dyn Engine> {
        let mut builder = EngineBuilder::new().repo(repo).store(store);
        builder
            .registry_mut()
            .register_provider(Arc::new(EchoProvider::new("echo")))
            .register_provider(Arc::new(FixedProvider {
                id: "scorer",
                reply: scorer_reply.to_owned(),
            }));
        builder.build().unwrap()
    }

    #[test]
    fn parse_score_reads_json() {
        assert_eq!(super::ctx::parse_score(r#"{"score": 0.8}"#), Some(0.8));
        assert_eq!(
            super::ctx::parse_score("noise before {\"score\": 1.5} after"),
            Some(1.0)
        );
        assert_eq!(super::ctx::parse_score("no json here"), None);
        // Verdict wrapped in brace-bearing prose (common for real LLM judges).
        assert_eq!(
            super::ctx::parse_score("Looking at {correctness}. Final: {\"score\": 0.85}"),
            Some(0.85)
        );
        // Score as a string, or under a nested key.
        assert_eq!(super::ctx::parse_score(r#"{"score": "0.9"}"#), Some(0.9));
        assert_eq!(
            super::ctx::parse_score(r#"{"result": {"score": 0.7}}"#),
            Some(0.7)
        );
    }

    #[test]
    fn effective_timeout_applies_a_default_only_to_subprocess_kinds() {
        use super::ctx::{DEFAULT_STEP_TIMEOUT, effective_timeout};
        use std::time::Duration;

        // No step/defaults timeout: subprocess kinds (run/provider/action) get the built-in
        // default; an approval gate gets none (it may wait for a human for hours).
        let wf = parse(
            "name: t\nsteps:\n  - {id: r, run: x}\n  - {id: p, provider: claude, prompt: hi}\n  - {id: c, action: git.commit, with: {message: m}}\n  - id: g\n    approval: { message: ok }\n    depends_on: [r]\n",
        );
        let by_id = |id: &str| wf.steps.iter().find(|s| s.id.as_str() == id).unwrap();
        assert_eq!(
            effective_timeout(by_id("r"), &wf.defaults),
            Some(DEFAULT_STEP_TIMEOUT)
        );
        assert_eq!(
            effective_timeout(by_id("p"), &wf.defaults),
            Some(DEFAULT_STEP_TIMEOUT)
        );
        assert_eq!(
            effective_timeout(by_id("c"), &wf.defaults),
            Some(DEFAULT_STEP_TIMEOUT)
        );
        assert_eq!(
            effective_timeout(by_id("g"), &wf.defaults),
            None,
            "an approval gate must never get an implicit timeout"
        );

        // Explicit step timeout wins; otherwise the workflow `defaults.timeout` applies.
        let wf2 = parse(
            "name: t\ndefaults: { timeout: \"5s\" }\nsteps:\n  - {id: a, run: x, timeout: \"2s\"}\n  - {id: b, run: y}\n",
        );
        let s = |id: &str| wf2.steps.iter().find(|st| st.id.as_str() == id).unwrap();
        assert_eq!(
            effective_timeout(s("a"), &wf2.defaults),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            effective_timeout(s("b"), &wf2.defaults),
            Some(Duration::from_secs(5))
        );
    }

    #[tokio::test]
    async fn judge_passes_a_step_above_threshold() {
        let repo = init_repo().await;
        let eng = engine_with(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
            r#"{"score": 0.9}"#,
        );
        let wf = parse(
            "name: j\nworkspace: { type: worktree }\nsteps:\n  - id: a\n    provider: echo\n    prompt: hi\n    judge: { provider: scorer, criteria: ok, threshold: 0.7 }\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        let a = &summary.steps[0];
        assert_eq!(a.status, StepStatus::Passed);
        assert!((a.judge_score.unwrap() - 0.9).abs() < 0.001);

        // The judge score (and gates) must survive a durable reload, not just live.
        let reloaded = eng.summary(summary.run_id).await.unwrap().unwrap();
        let ra = reloaded
            .steps
            .iter()
            .find(|s| s.id.as_str() == "a")
            .unwrap();
        assert!(
            (ra.judge_score.unwrap() - 0.9).abs() < 0.001,
            "judge score must be persisted and reloadable"
        );
    }

    #[tokio::test]
    async fn judge_fails_a_step_below_threshold() {
        let repo = init_repo().await;
        let eng = engine_with(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
            r#"{"score": 0.2}"#,
        );
        let wf = parse(
            "name: j\nworkspace: { type: worktree }\nsteps:\n  - id: a\n    provider: echo\n    prompt: hi\n    judge: { provider: scorer, criteria: ok, threshold: 0.7 }\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(summary.status, RunStatus::Failed);
        let a = &summary.steps[0];
        assert_eq!(a.status, StepStatus::Failed);
        assert!((a.judge_score.unwrap() - 0.2).abs() < 0.001);
        assert!(
            summary
                .error
                .as_deref()
                .unwrap_or("")
                .contains("judge score")
                || a.status == StepStatus::Failed
        );
    }

    #[tokio::test]
    async fn retry_recovers_a_flaky_step() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // Model real flakiness with state OUTSIDE the workdir (an external resource that
        // becomes available on retry): the marker lives in a temp dir, NOT the workdir, so the
        // per-retry workdir restore doesn't wipe it. Fails the first attempt, passes the second.
        let ext = tempfile::tempdir().unwrap();
        let marker = ext.path().join("marker");
        // Forward slashes: the path is interpolated into a DOUBLE-QUOTED YAML scalar, where a
        // backslash is an escape (`\U…` from a Windows temp path is an invalid escape and fails to
        // parse) — and `sh` also treats `\` as an escape. Windows accepts `/` in paths. No-op on Unix.
        let marker = marker.to_string_lossy().replace('\\', "/");
        let wf = parse(&format!(
            "name: r\nworkspace: {{ type: worktree }}\nsteps:\n  - id: flaky\n    run: \"test -f {marker} || (touch {marker}; exit 1)\"\n    retry: {{ max: 1 }}\n",
        ));
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        let flaky = &summary.steps[0];
        assert_eq!(flaky.status, StepStatus::Passed);
        assert_eq!(flaky.attempts, 2, "should have retried once");
    }

    #[tokio::test]
    async fn retry_rewinds_the_workdir_between_attempts() {
        // Regression: a workdir mutation by a failed attempt must NOT persist into the retry.
        // An in-workdir marker created on attempt 1 is wiped before attempt 2, so the step
        // never "recovers" off its own partial edits — it exhausts its retries and fails.
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: r\nworkspace: { type: worktree }\nsteps:\n  - id: flaky\n    run: \"test -f .marker || (touch .marker; exit 1)\"\n    retry: { max: 1 }\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(summary.status, RunStatus::Failed);
        let flaky = &summary.steps[0];
        assert_eq!(flaky.status, StepStatus::Failed);
        assert_eq!(flaky.attempts, 2, "both attempts ran against a clean tree");
    }

    #[tokio::test]
    async fn non_durable_retry_does_not_leak_a_snapshot_ref() {
        // A retrying step snapshots the tree even in a non-durable run; the snapshot ref must
        // still be reclaimed at run end (cleanup is gated on durable elsewhere, so this guards
        // the unconditional run-path delete).
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: r\ndurable: false\nworkspace: { type: worktree }\nsteps:\n  - {id: flaky, run: \"echo x >> f.txt && false\", retry: { max: 1 }}\n",
        );
        let _ = eng.run(&wf, RunInput::manual()).await.unwrap();
        // No refs/odin/* may remain pinning dangling snapshot commits.
        let refs = std::process::Command::new("git")
            .args([
                "-C",
                repo.path().to_str().unwrap(),
                "for-each-ref",
                "refs/odin/",
            ])
            .output()
            .unwrap();
        assert!(
            refs.stdout.is_empty(),
            "leaked snapshot ref(s): {}",
            String::from_utf8_lossy(&refs.stdout)
        );
    }

    #[tokio::test]
    async fn durable_retry_run_cleans_up_both_the_durable_and_retry_refs() {
        // A durable run whose step also carries a retry takes BOTH a durable per-run snapshot
        // (anchored by refs/odin/run/<id>) and a transient pre-step retry snapshot (anchored by
        // a separate refs/odin/retry/<id>-<step> ref). After the run, NEITHER may linger — the
        // retry ref is dropped when the step settles, the durable ref at run end.
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: r\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: write, run: \"echo hi > f.txt\", retry: { max: 1 }}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(summary.status, RunStatus::Succeeded);
        let refs = std::process::Command::new("git")
            .args([
                "-C",
                repo.path().to_str().unwrap(),
                "for-each-ref",
                "refs/odin/",
            ])
            .output()
            .unwrap();
        assert!(
            refs.stdout.is_empty(),
            "leaked ref(s) after a durable+retry run: {}",
            String::from_utf8_lossy(&refs.stdout)
        );
    }

    #[tokio::test]
    async fn retry_exhausts_then_fails() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: r\nworkspace: { type: worktree }\nsteps:\n  - {id: boom, run: \"exit 1\", retry: { max: 1 }}\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(summary.status, RunStatus::Failed);
        assert_eq!(summary.steps[0].attempts, 2, "1 + max attempts");
    }

    #[tokio::test]
    async fn a_bare_step_inherits_defaults_retry() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // `flaky` declares no `retry:` of its own, so it must inherit `defaults.retry` (max 2) and
        // recover on the second attempt. Without the merge it would get 0 retries and the run fails.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\ndefaults:\n  retry: { max: 2 }\nsteps:\n  - {id: flaky, run: \"if [ {{ retry.attempt }} -eq 1 ]; then exit 1; fi; echo ok\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let flaky = s.steps.iter().find(|x| x.id.as_str() == "flaky").unwrap();
        assert_eq!(
            flaky.attempts, 2,
            "inherited defaults.retry should give a 2nd attempt"
        );
    }

    #[tokio::test]
    async fn a_step_retry_overrides_defaults_retry() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // The step sets its OWN `retry: { max: 1 }` (2 attempts), so it must NOT inherit the more
        // generous default (max 5). It needs attempt 3 to pass, so capped at 2 it fails.
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\ndefaults:\n  retry: { max: 5 }\nsteps:\n  - {id: flaky, retry: { max: 1 }, run: \"if [ {{ retry.attempt }} -lt 3 ]; then exit 1; fi; echo ok\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            s.status,
            RunStatus::Failed,
            "the step's own max:1 must override the default"
        );
        let flaky = s.steps.iter().find(|x| x.id.as_str() == "flaky").unwrap();
        assert_eq!(
            flaky.attempts, 2,
            "step's own max:1 = 2 attempts, not the default's 6"
        );
    }

    #[tokio::test]
    async fn a_bare_step_inherits_defaults_retry_feedback() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        // The default carries `feedback: concise`, which a bare step must inherit — so attempt 2
        // sees the prior failure under `retry.feedback`, not an empty string. (Guards against the
        // effective policy supplying `max` but `feedback` still reading the step's own off mode.)
        let wf = parse(
            "name: w\ndurable: true\nworkspace: { type: worktree }\ndefaults:\n  retry: { max: 1, feedback: concise }\nsteps:\n  - {id: flaky, run: \"if [ {{ retry.attempt }} -eq 1 ]; then exit 1; fi; echo 'fb=[{{ retry.feedback }}]'\"}\n",
        );
        let s = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(s.status, RunStatus::Succeeded, "error: {:?}", s.error);
        let flaky = s.steps.iter().find(|x| x.id.as_str() == "flaky").unwrap();
        let stdout = flaky
            .outputs
            .get("stdout")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            stdout.contains("exited with code 1"),
            "inherited feedback should carry the prior failure; stdout: {stdout:?}"
        );
    }

    /// Three `scratch` steps each write the same filename in their OWN isolated worktree;
    /// each one's `outputs.diff` must contain only its own content (proving isolation), and
    /// the run's shared DIFF stays empty (scratch edits never touch the shared tree).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn scratch_steps_are_isolated_and_emit_their_own_diff() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: fanout\ndurable: true\nworkspace: { type: worktree }\nmax_parallel: 3\nsteps:\n  - { id: a, run: \"echo aaa > cand.txt\", scratch: true }\n  - { id: b, run: \"echo bbb > cand.txt\", scratch: true }\n  - { id: c, run: \"echo ccc > cand.txt\", scratch: true }\n  - { id: collect, run: \"true\", depends_on: [a, b, c] }\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        assert_eq!(summary.steps.len(), 4);

        for (id, want, others) in [
            ("a", "aaa", ["bbb", "ccc"]),
            ("b", "bbb", ["aaa", "ccc"]),
            ("c", "ccc", ["aaa", "bbb"]),
        ] {
            let step = summary.steps.iter().find(|s| s.id.as_str() == id).unwrap();
            let diff = step.outputs["diff"].as_str().unwrap_or_default();
            assert!(
                diff.contains(want),
                "{id}'s diff should contain {want}: {diff}"
            );
            for other in others {
                assert!(
                    !diff.contains(other),
                    "{id} saw {other} — scratch steps are not isolated"
                );
            }
        }

        // The shared tree was never touched by the scratch steps (`collect` is a no-op).
        assert!(
            summary
                .diff
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "shared DIFF should be empty, got: {:?}",
            summary.diff
        );
    }

    /// A downstream step can consume a scratch step's diff via `steps.<id>.outputs.diff`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scratch_diff_flows_to_a_downstream_step() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: chain\ndurable: true\nworkspace: { type: worktree }\nmax_parallel: 2\nsteps:\n  - { id: cand, run: \"echo zzz > out.txt\", scratch: true }\n  - id: use\n    provider: echo\n    prompt: \"candidate:\\n{{ steps.cand.outputs.diff }}\"\n    depends_on: [cand]\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        let used = summary
            .steps
            .iter()
            .find(|s| s.id.as_str() == "use")
            .unwrap();
        // The echo provider echoes its rendered prompt, which embedded the candidate diff.
        assert!(
            used.outputs["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("zzz"),
            "downstream step should see the scratch diff: {:?}",
            used.outputs["stdout"]
        );
    }

    /// Independent `scratch` steps run concurrently: three 0.5s sleeps under `max_parallel:
    /// 3` finish well under the ~1.5s a sequential walk takes. Generous threshold so a slow
    /// CI runner (worktree + process overhead) doesn't flake.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn independent_scratch_steps_run_concurrently() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: par\ndurable: true\nworkspace: { type: worktree }\nmax_parallel: 3\nsteps:\n  - { id: a, run: \"sleep 0.5\", scratch: true }\n  - { id: b, run: \"sleep 0.5\", scratch: true }\n  - { id: c, run: \"sleep 0.5\", scratch: true }\n",
        );
        let start = std::time::Instant::now();
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        // The wall-clock concurrency check is Unix-only: on Windows `git worktree add` (serialized
        // per scratch step under the worktree lock) dominates the wall-clock and masks the overlap
        // of the sleeps. Windows still exercises the concurrent scratch path (and asserts it
        // succeeds above); Unix verifies the timing.
        #[cfg(not(windows))]
        assert!(
            elapsed < std::time::Duration::from_millis(1300),
            "three 0.5s scratch steps took {elapsed:?}; expected concurrency (~0.5s, not ~1.5s)"
        );
        #[cfg(windows)]
        let _ = elapsed;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_runs_on_one_worktree_repo_do_not_race() {
        // Regression (CI-caught): N concurrent runs each `git worktree add`/`remove` on the
        // SAME repo. git's `.git/worktrees/` metadata is global and not safe for concurrent
        // mutation — an unserialized add racing a remove fails with "failed to read
        // .../commondir", failing a run. With acquire/release serialized, all N must succeed.
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf =
            parse("name: c\nworkspace: { type: worktree }\nsteps:\n  - {id: s, run: \"true\"}\n");
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let eng = Arc::clone(&eng);
            let wf = wf.clone();
            tasks.push(tokio::spawn(async move {
                eng.run(&wf, RunInput::manual()).await
            }));
        }
        for t in tasks {
            let summary = t.await.unwrap().unwrap();
            assert_eq!(
                summary.status,
                RunStatus::Succeeded,
                "a concurrent run failed (worktree metadata race): {:?}",
                summary.error
            );
        }
    }

    /// A scratch step that edits files then FAILS still surfaces its diff (a failed
    /// candidate's partial work is worth inspecting), unlike a skipped one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_scratch_step_still_captures_its_diff() {
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: failcap\ndurable: true\nworkspace: { type: worktree }\nmax_parallel: 2\nsteps:\n  - { id: bad, run: \"echo partial > work.txt; exit 1\", scratch: true }\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(summary.status, RunStatus::Failed);
        let bad = &summary.steps[0];
        assert_eq!(bad.status, StepStatus::Failed);
        assert!(
            bad.outputs
                .get("diff")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .contains("partial"),
            "a failed scratch step should still expose its diff: {:?}",
            bad.outputs.get("diff")
        );
    }

    #[tokio::test]
    async fn capture_diff_returns_none_for_an_unresolvable_base() {
        // A failed `git diff <base>` (bad/ reaped base) must yield None — NOT Some("") — so a
        // carried-forward DIFF isn't clobbered with empty. A valid base with no changes is a
        // real empty diff and stays Some("").
        let repo = init_repo().await;
        let eng = LocalEngine::new(
            Registry::with_builtins(),
            None,
            repo.path().to_path_buf(),
            None,
            None,
        );
        let cancel = CancelToken::new();
        let bogus = "0".repeat(40);
        assert!(
            eng.capture_diff(repo.path(), Some(&bogus), &cancel)
                .await
                .is_none(),
            "an unresolvable base must return None"
        );
        let head = eng.git_head(repo.path(), &cancel).await.unwrap();
        assert_eq!(
            eng.capture_diff(repo.path(), Some(&head), &cancel)
                .await
                .as_deref(),
            Some(""),
            "a valid base with no changes is an empty diff, not None"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scratch_diff_includes_staged_changes() {
        // A scratch step that `git add`s its edit must still expose them in outputs.diff
        // (diff vs HEAD captures staged + unstaged), not silently drop the staged part.
        let repo = init_repo().await;
        let eng = engine(
            repo.path(),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        );
        let wf = parse(
            "name: staged\ndurable: true\nworkspace: { type: worktree }\nmax_parallel: 2\nsteps:\n  - { id: cand, run: \"echo s > staged.txt && git add staged.txt\", scratch: true }\n",
        );
        let summary = eng.run(&wf, RunInput::manual()).await.unwrap();
        assert_eq!(
            summary.status,
            RunStatus::Succeeded,
            "error: {:?}",
            summary.error
        );
        let cand = summary
            .steps
            .iter()
            .find(|s| s.id.as_str() == "cand")
            .unwrap();
        assert!(
            cand.outputs["diff"]
                .as_str()
                .unwrap_or_default()
                .contains("staged.txt"),
            "scratch diff dropped the staged change: {:?}",
            cand.outputs.get("diff")
        );
    }
}
