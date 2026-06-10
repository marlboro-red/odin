//! The public `Engine` trait implementation and run lifecycle.
//!
//! `impl Engine for LocalEngine` — the embedder-facing surface (`run`, `resume_all`, `summary`,
//! `submit_approval`, `reject_and_rerun`, `prune`) — plus the `resume_state` and `fail_run`
//! lifecycle helpers it relies on. Carved out of `local.rs`; a child module of `local`, so it
//! drives the engine's own scheduler/provisioning/event helpers directly. (The `Engine` trait
//! itself lives in the parent `engine` module — imported crate-rooted, not via `super`.)

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;
use indexmap::IndexMap;
use serde_json::Value;
use tracing::Instrument as _;

use super::ctx::{collect_side_effects, step_result, total_usage};
use super::{DIFF, ExecResult, LocalEngine};
use crate::api::{
    ApprovalDecision, Decision, RerunOutcome, RunInput, RunStatus, RunSummary, StepStatus,
};
use crate::engine::Engine;
use crate::error::{Error, Result};
use crate::ids::RunId;
use crate::ir::Workflow;
use crate::traits::{AcquireCtx, CancelToken, PrunePolicy, PruneReport, RunEvent, RunState};
use crate::usage::Usage;

/// How many crashed runs `resume_all` recovers at once. Bounds concurrent recovery so a restart
/// with many incomplete runs doesn't spawn an unbounded fan-out, while still overlapping the long
/// (agentic) resumes instead of serializing them.
const RESUME_CONCURRENCY: usize = 8;

#[async_trait]
impl Engine for LocalEngine {
    #[allow(clippy::too_many_lines)]
    async fn run(&self, workflow: &Workflow, input: RunInput) -> Result<RunSummary> {
        let report = crate::validate::validate(workflow, &self.registry.known_names());
        if report.has_errors() {
            return Err(Error::Validation(report));
        }
        // An approval gate parks the run until a human decision (`submit_approval`), which can only
        // resume the run from a durable store. With no store the run WOULD suspend, but be
        // permanently unresumable (and vanish on restart) — and that only surfaced later, at
        // `submit_approval`. Fail fast at run start instead.
        if self.store.is_none()
            && workflow
                .steps
                .iter()
                .any(|s| matches!(s.kind, crate::ir::StepKind::Approval(_)))
        {
            return Err(Error::Input(format!(
                "workflow {:?} has an approval gate, which requires a durable store to resume from, \
                 but the engine has none — build it with `.store(...)`",
                workflow.name.as_str()
            )));
        }
        let params = Self::resolve_params(workflow, &input)?;

        let run_id = RunId::new();
        // Claim the run id for this whole execution. The id is fresh, so the claim always
        // succeeds; holding it makes a concurrent `resume_all` sweep refuse to also execute this
        // run (its early `Running` checkpoint would otherwise make it look crash-recoverable),
        // so the run's side effects can't be double-applied. Released on return (any path).
        let _claim = self.claim_run(run_id);
        let started_at = Utc::now();
        let workspace = self.make_workspace(&workflow.workspace);
        let handle = self
            .acquire_workspace(
                &workspace,
                AcquireCtx {
                    run_id,
                    config: workflow.workspace.clone(),
                },
            )
            .await?;
        let workdir = handle.path.clone();

        let mut state = RunState {
            run_id,
            workflow: workflow.name.clone(),
            schema_major: workflow.schema_version.major,
            status: RunStatus::Running,
            error: None,
            steps: IndexMap::new(),
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input,
            workspace: Some(handle.clone()),
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: started_at,
            updated_at: started_at,
        };
        if let Err(e) = self.checkpoint(workflow.durable, &state).await {
            self.release_workspace(&workspace, handle).await;
            return Err(e);
        }
        self.emit(
            run_id,
            workflow.durable,
            RunEvent::RunStarted { at: started_at },
        )
        .await;

        let run_span = tracing::info_span!(
            "run",
            run_id = %run_id,
            workflow = %workflow.name,
            durable = workflow.durable,
        );
        tracing::info!(parent: &run_span, "run started");
        let cancel = CancelToken::new();
        // Register the token so `cancel_run`/`cancel_all_active` can fire it; the guard removes it
        // when this method returns (completed, failed, or suspended at a gate).
        let _cancel_guard = self.register_cancel(run_id, cancel.clone());
        // Also watch the store for a cross-process `odin cancel` request (durable runs only).
        let _cancel_watcher = self.spawn_cancel_watcher(run_id, workflow.durable, cancel.clone());
        let exec = self
            .execute(workflow, &mut state, &workdir, &params, &cancel)
            .instrument(run_span.clone())
            .await;
        // Paused at an approval gate? Keep the workspace and snapshot ref (the run resumes on a
        // decision) and return AwaitingApproval — do NOT reclaim resources or mark terminal.
        let exec = match exec {
            Ok(r) if r.suspended.is_some() => {
                let gate = r.suspended.clone();
                state.status = RunStatus::AwaitingApproval;
                state.updated_at = Utc::now();
                self.checkpoint(workflow.durable, &state).await?;
                self.emit_suspended(
                    run_id,
                    workflow.durable,
                    crate::traits::SuspendReason::Approval,
                )
                .await;
                tracing::info!(parent: &run_span, run_id = %run_id, gate = ?gate, "run paused for approval");
                return Ok(Self::suspended_summary(
                    run_id,
                    workflow,
                    &r,
                    started_at,
                    &state.steps,
                ));
            }
            other => other,
        };
        // Graceful-shutdown suspend: a durable run interrupted by `cancel_all_active` (daemon
        // shutdown fires each token via `shutdown`) is left RESUMABLE — keep its workspace and
        // snapshot ref and checkpoint it non-terminal (`Running`) so `resume_all` completes it on
        // the next start. (The executor already rewound the killed step to `Running`.) A USER
        // cancel, or a non-durable run, falls through to the terminal path below and ends
        // `Cancelled`. Only when `execute` returned Ok — an Err during shutdown is a real failure.
        if workflow.durable && cancel.is_shutdown() {
            if let Ok(r) = &exec {
                state.status = RunStatus::Running;
                state.error = None;
                state.updated_at = Utc::now();
                self.checkpoint(workflow.durable, &state).await?;
                self.emit_suspended(
                    run_id,
                    workflow.durable,
                    crate::traits::SuspendReason::Shutdown,
                )
                .await;
                tracing::info!(parent: &run_span, run_id = %run_id, "run suspended for shutdown; will resume on next start");
                return Ok(Self::interrupted_summary(
                    run_id,
                    workflow,
                    r,
                    started_at,
                    &state.steps,
                ));
            }
        }
        // The run is terminal now — drop the snapshot ref so its commits can be collected.
        // Fully unconditional: durable runs snapshot per step, AND a retrying step snapshots
        // even in a NON-durable run (for per-retry workdir restore), so either kind can leave
        // the ref behind. `delete_snapshot_ref` is a no-op when no ref was created.
        self.delete_snapshot_ref(&workdir, run_id, &cancel).await;
        self.release_workspace(&workspace, handle).await;

        let (exec, error) = match exec {
            Ok(r) => (r, None),
            Err(e) => (
                ExecResult {
                    steps: Vec::new(),
                    // Reconstruct from persisted state so an execute error after some
                    // side-effecting steps completed doesn't drop them from the summary.
                    side_effects: collect_side_effects(&state.steps),
                    usage: Usage::default(),
                    diff: None,
                    suspended: None,
                },
                Some(e.to_string()),
            ),
        };
        // A step failure leaves `error` None here; `summarize` derives the run-level error
        // from the first failed step's recorded reason (single source of truth).
        let (mut status, mut summary) = Self::summarize(run_id, workflow, exec, error, started_at);
        // A fired cancel token ends the run as `Cancelled` regardless of how the cut-short steps
        // settled — whether the running step was killed (Failed) or cancel landed before the next
        // step even launched (the loop guard skips it, so nothing failed). The token is only
        // reachable while the run is registered/in-flight (the guard removes it on finish), so an
        // `is_cancelled()` here always means the run was interrupted, never a clean completion.
        // Read ONCE so the recorded status, the `RunCancelled` event, and `RunFinished` can't
        // disagree if a cancel lands between two reads.
        let cancelled = cancel.is_cancelled();
        if cancelled {
            status = RunStatus::Cancelled;
            summary.status = RunStatus::Cancelled;
            summary.error = Some("run cancelled".to_owned());
        }
        state.status = status;
        state.error = summary.error.clone();
        state.updated_at = Utc::now();
        self.checkpoint(workflow.durable, &state).await?;
        if cancelled {
            self.emit_cancelled(run_id, workflow.durable, &cancel).await;
        }
        self.emit(
            run_id,
            workflow.durable,
            RunEvent::RunFinished {
                status,
                at: Utc::now(),
            },
        )
        .await;
        let elapsed_ms = (Utc::now() - started_at).num_milliseconds();
        // Level matches status: failed → ERROR (with reason), cancelled → WARN, else INFO. Emitted
        // within the run span so it nests under the run's trace.
        run_span.in_scope(|| Self::log_run_outcome(&summary, elapsed_ms));

        Ok(summary)
    }

    fn cancel_run(&self, run_id: RunId) -> bool {
        match self.active_cancels.lock().unwrap().get(&run_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    fn cancel_all_active(&self) -> usize {
        let map = self.active_cancels.lock().unwrap();
        for token in map.values() {
            // Graceful shutdown, not a user cancel: a durable run is left resumable (checkpointed
            // non-terminal) and picked up by `resume_all` next start, rather than dying `Cancelled`.
            token.shutdown();
        }
        map.len()
    }

    async fn resume_all(&self, workflows: &[Workflow]) -> Result<Vec<RunSummary>> {
        use futures_util::stream::StreamExt as _;

        let Some(store) = self.store.clone() else {
            return Ok(Vec::new());
        };
        let by_name: HashMap<&str, &Workflow> =
            workflows.iter().map(|w| (w.name.as_str(), w)).collect();

        // First pass (sequential, cheap): claim each resumable run and pair it with its workflow.
        // Claiming first means a run is recovered at most once even though the resumes below run
        // concurrently; an unserved-workflow run gets its leftover snapshot refs reclaimed here.
        let sweep_cancel = CancelToken::new();
        let mut claimed = Vec::new();
        for state in store.load_incomplete().await? {
            let Some(workflow) = by_name.get(state.workflow.as_str()).copied() else {
                // The run targets a workflow we no longer serve — best-effort reclaim its
                // snapshot refs. A `slot_pool` run's refs live in the slot's *own* `.git`, not the
                // main repo, so clean against the run's recorded workspace path when it still
                // exists; fall back to `repo_root` (a `worktree` run's refs live in the shared
                // `.git`, reachable from there, and a vanished slot took its refs with it).
                let workdir = state
                    .workspace
                    .as_ref()
                    .map(|h| h.path.clone())
                    .filter(|p| p.exists())
                    .unwrap_or_else(|| self.repo_root.clone());
                self.delete_snapshot_ref(&workdir, state.run_id, &sweep_cancel)
                    .await;
                continue;
            };
            // Skip a run already being resumed (e.g. by a concurrent approval decision); never
            // execute the same run twice. The claim is held for this run's whole resume.
            let Some(claim) = self.claim_run(state.run_id) else {
                continue;
            };
            claimed.push((claim, workflow, state));
        }

        // Second pass: resume the claimed runs CONCURRENTLY (bounded). Recovering one at a time
        // blocked the daemon's trigger serving behind the *entire* recovery — hours, with several
        // long durable runs in flight. Each run has its own claim, workspace, and cancel token, so
        // this is the same concurrency the engine already supports for live dispatch. (Build the
        // futures in a plain loop rather than `iter.map(closure)` to sidestep a closure-lifetime
        // HRTB error on the captured `&self`.)
        let mut futs = Vec::with_capacity(claimed.len());
        for (claim, workflow, state) in claimed {
            futs.push(async move {
                let _claim = claim; // held across the whole resume
                self.resume_state(workflow, state).await
            });
        }
        let results: Vec<Result<Option<RunSummary>>> = futures_util::stream::iter(futs)
            .buffer_unordered(RESUME_CONCURRENCY)
            .collect()
            .await;

        let mut summaries = Vec::new();
        for result in results {
            if let Some(summary) = result? {
                summaries.push(summary);
            }
        }
        Ok(summaries)
    }

    async fn summary(&self, run_id: RunId) -> Result<Option<RunSummary>> {
        // Persisted runs are in the store; unpersisted (non-durable, or durable-without-store) in
        // the mirror. Try both; if a run is in both (a `durable` flag flipped mid-run), prefer the
        // one updated more recently so a stale persisted row can't shadow the live mirror entry.
        let from_store = match &self.store {
            Some(store) => store.load_run(run_id).await?,
            None => None,
        };
        let from_mirror = self.mirror.lock().unwrap().get(&run_id).cloned();
        let state = match (from_store, from_mirror) {
            (Some(a), Some(b)) => Some(if a.updated_at >= b.updated_at { a } else { b }),
            (s, m) => s.or(m),
        };
        Ok(state.map(Self::summary_from_state))
    }

    async fn recent(&self, limit: usize) -> Result<Vec<crate::view::RunView>> {
        // Project both sources to the light `RunView` rather than cloning full states — in
        // particular the mirror is projected *under its lock* (RunView is small), so a dashboard
        // poll never clones up to 256 full run states while blocking every step boundary.
        let mut views: Vec<crate::view::RunView> = match &self.store {
            Some(store) => store
                .recent(limit)
                .await?
                .iter()
                .map(crate::view::RunView::project)
                .collect(),
            None => Vec::new(),
        };
        views.extend({
            let mirror = self.mirror.lock().unwrap();
            mirror
                .values()
                .map(crate::view::RunView::project)
                .collect::<Vec<_>>()
        });
        // Newest first (RFC3339 `updated_at` sorts chronologically), then dedup by run_id keeping
        // the newest — the persisted/mirror sources normally don't overlap, but a durable flag
        // flipped mid-run can land a run in both.
        views.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.run_id.cmp(&a.run_id))
        });
        let mut seen = std::collections::HashSet::new();
        views.retain(|v| seen.insert(v.run_id.clone()));
        views.truncate(limit);
        Ok(views)
    }

    async fn submit_approval(
        &self,
        run_id: RunId,
        decision: Decision,
        approver: String,
        note: Option<String>,
        workflows: &[Workflow],
    ) -> Result<Option<RunSummary>> {
        let Some(store) = self.store.clone() else {
            return Err(Error::Input(
                "approvals require a durable store; none is configured".to_owned(),
            ));
        };
        // Claim the run for the whole decision-then-resume so two concurrent decisions (e.g. a
        // double-submitted HTTP approve) can't both pass the status check below and each resume
        // it — which would run the downstream side effects twice. The loser is refused here; a
        // decision arriving after the winner finished is refused by the status check.
        let Some(_claim) = self.claim_run(run_id) else {
            return Err(Error::Input(format!(
                "run {run_id} already has a decision being applied"
            )));
        };
        let Some(mut state) = store.load_run(run_id).await? else {
            return Ok(None);
        };
        // The run's own workflow must be among those provided, or it couldn't be resumed. Check
        // BEFORE mutating any state: recording the decision + flipping to Running first would
        // leave the run stuck (un-resumable, no longer awaiting).
        let Some(workflow) = workflows.iter().find(|w| w.name == state.workflow) else {
            return Err(Error::Input(format!(
                "run {run_id} targets workflow {:?}, which was not provided — pass its file via \
                 --workflow",
                state.workflow.as_str()
            )));
        };
        if state.status != RunStatus::AwaitingApproval {
            return Err(Error::Input(format!(
                "run {run_id} is not awaiting approval (status is {:?})",
                state.status
            )));
        }
        // Cross-process fence: atomically flip `awaiting_approval` -> `running` in the store. Only
        // the winner of a race between two processes sharing this store (e.g. the CLI `odin
        // approve` and the daemon's HTTP `/approve`) gets `true` and proceeds, so the resumed run's
        // downstream side effects run exactly once. The in-process `claim_run` above guards
        // same-process races; this guards separate processes. (Checked AFTER the workflow/status
        // validation above so a missing-`--workflow` error never leaves the column flipped.)
        if !store.claim_awaiting(run_id).await? {
            return Err(Error::Input(format!(
                "run {run_id} is already having a decision applied by another process"
            )));
        }
        // The gate parked at `AwaitingApproval` is the one this decision answers.
        let Some(gate) = state
            .steps
            .iter()
            .find(|(_, st)| st.status == StepStatus::AwaitingApproval)
            .map(|(id, _)| id.clone())
        else {
            return Err(Error::Input(format!(
                "run {run_id} has no pending approval gate"
            )));
        };
        tracing::info!(run_id = %run_id, gate = %gate, decision = ?decision, %approver, "approval recorded");
        // Capture the decision for the audit/progress event before the fields are moved into the
        // recorded `ApprovalDecision`. One timestamp for both so they can't disagree.
        let decided_at = Utc::now();
        let decided = RunEvent::ApprovalDecided {
            step: gate.clone(),
            decision,
            approver: approver.clone(),
            note: note.clone(),
            at: decided_at,
        };
        state.approvals.insert(
            gate,
            ApprovalDecision {
                decision,
                approver,
                at: decided_at,
                note,
            },
        );
        // Flip to Running, then resume THIS run (not the all-runs sweep): resuming only the
        // decided run keeps the claim meaningful and avoids disturbing other in-flight runs.
        state.status = RunStatus::Running;
        state.updated_at = Utc::now();
        store.checkpoint(&state).await?;
        // Emit with `durable = true`: approvals always run with a store (checked above) and the
        // checkpoint just persisted, so the audit event must not be dropped on a flag mismatch.
        self.emit(run_id, true, decided).await;
        match self.resume_state(workflow, state).await? {
            Some(summary) => Ok(Some(summary)),
            // A paused run always kept its workspace, so this is unreachable in practice; treat
            // a missing workspace as a resume failure rather than a silent success.
            None => Err(Error::Input(format!(
                "run {run_id} has no workspace handle; cannot resume"
            ))),
        }
    }

    async fn reject_and_rerun(
        &self,
        run_id: RunId,
        approver: String,
        note: String,
        workflows: &[Workflow],
    ) -> Result<Option<RerunOutcome>> {
        let Some(store) = self.store.clone() else {
            return Err(Error::Input(
                "approvals require a durable store; none is configured".to_owned(),
            ));
        };
        // 1. Load the run FIRST — to find its workflow and pre-check that the feedback we'll
        //    inject is acceptable — BEFORE the destructive reject. If a misdeclared `feedback`
        //    param would make the rerun fail param validation, we must not fail the gate first
        //    (that would leave a dead run with no possible rerun). The input is also captured
        //    here; the reject doesn't mutate it.
        let Some(original) = store.load_run(run_id).await? else {
            return Ok(None); // unknown run
        };
        let Some(workflow) = workflows.iter().find(|w| w.name == original.workflow) else {
            return Err(Error::Input(format!(
                "run {run_id} targets workflow {:?}, which was not provided — pass its file via \
                 --workflow",
                original.workflow.as_str()
            )));
        };
        let feedback = Value::String(note.clone());
        if let Some((_, spec)) = workflow
            .params
            .iter()
            .find(|(n, _)| n.as_str() == "feedback")
        {
            if !spec.ty.matches(&feedback) {
                return Err(Error::Input(format!(
                    "workflow {:?} declares a `feedback` param of type {}, but `--rerun` injects \
                     the note as a string; declare `feedback` as a string",
                    workflow.name.as_str(),
                    spec.ty.name()
                )));
            }
        }
        // 2. Reject the gate (fails the run, recording `note` as the gate's feedback).
        let Some(rejected) = self
            .submit_approval(run_id, Decision::Rejected, approver, Some(note), workflows)
            .await?
        else {
            return Ok(None);
        };
        // 3. Start a fresh run carrying the original params/trigger plus the feedback. Clear the
        //    idempotency key (a rerun is a distinct run, not a retry of the original key).
        let mut input = original.input;
        input.params.insert("feedback".to_owned(), feedback);
        input.idempotency_key = None;
        tracing::info!(rejected = %run_id, "rerunning with feedback");
        let rerun = self.run(workflow, input).await?;
        Ok(Some(RerunOutcome { rejected, rerun }))
    }

    async fn prune(&self, policy: &PrunePolicy, dry_run: bool) -> Result<PruneReport> {
        let Some(store) = &self.store else {
            return Ok(PruneReport::default());
        };
        let report = store.prune(policy, dry_run).await?;
        // Reclaim each pruned run's leftover git refs from the shared repo (best-effort, never
        // fails the prune). Two kinds: (1) snapshot refs — terminal runs normally drop these on
        // completion, this sweeps any historical leftover; (2) the per-run `worktree` branch
        // `refs/heads/odin/run/<id>`, which `WorktreeWorkspace::release` deliberately keeps (it may
        // hold committed work to push/PR). Once the run record is pruned the branch is dead, so GC
        // it here — otherwise these branches accumulate without bound for every run ever served.
        if !dry_run && !report.run_ids.is_empty() {
            let cancel = CancelToken::new();
            for run_id in &report.run_ids {
                self.delete_snapshot_ref(&self.repo_root, *run_id, &cancel)
                    .await;
                self.delete_ref(
                    &self.repo_root,
                    &format!("refs/heads/odin/run/{run_id}"),
                    &cancel,
                )
                .await;
                // Drop the run's spooled step logs too (best-effort), so they don't outlive the
                // run record that referenced them.
                if let Some(logs) = &self.logs_dir {
                    let dir = logs.join(run_id.to_string());
                    if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(run_id = %run_id, error = %e, "failed to remove spooled step logs on prune");
                        }
                    }
                }
            }
        }
        tracing::info!(
            runs_pruned = report.runs_pruned,
            events_pruned = report.events_pruned,
            dry_run = report.dry_run,
            "prune complete"
        );
        Ok(report)
    }
}

impl LocalEngine {
    /// Builds a [`RunSummary`] from a loaded/mirrored [`RunState`]. `finished_at` is `Some` only
    /// for a terminal run — an in-flight or paused run (now visible via the mirror) reports `None`
    /// rather than a fabricated end time.
    fn summary_from_state(state: RunState) -> RunSummary {
        let steps = state
            .steps
            .iter()
            .map(|(id, st)| step_result(id, st))
            .collect();
        let diff = state
            .artifacts
            .get(&crate::ids::ArtifactName::new(DIFF))
            .cloned();
        let usage = total_usage(&state.steps);
        let finished_at = state.status.is_terminal().then_some(state.updated_at);
        RunSummary {
            run_id: state.run_id,
            workflow: state.workflow,
            status: state.status,
            steps,
            usage,
            side_effects: collect_side_effects(&state.steps),
            diff,
            error: state.error,
            started_at: state.created_at,
            finished_at,
        }
    }

    /// Logs a run's terminal outcome at a level matching its status: a failed run at ERROR (with
    /// its terminal reason), a cancelled run at WARN, everything else at INFO. Called from every
    /// terminal path — the foreground `run()`, a resume, and `fail_run` — so a failed run is
    /// ALWAYS loud at the default level, including one that fails during crash recovery.
    fn log_run_outcome(summary: &RunSummary, elapsed_ms: i64) {
        match summary.status {
            RunStatus::Failed => tracing::error!(
                run_id = %summary.run_id,
                steps = summary.steps.len(),
                cost_micros = summary.usage.cost_micros,
                elapsed_ms,
                error = summary.error.as_deref().unwrap_or("(none)"),
                "run failed"
            ),
            RunStatus::Cancelled => tracing::warn!(
                run_id = %summary.run_id,
                steps = summary.steps.len(),
                cost_micros = summary.usage.cost_micros,
                elapsed_ms,
                "run cancelled"
            ),
            status => tracing::info!(
                run_id = %summary.run_id,
                status = ?status,
                steps = summary.steps.len(),
                cost_micros = summary.usage.cost_micros,
                elapsed_ms,
                "run finished"
            ),
        }
    }

    /// Marks a run terminally Failed, checkpoints it, and returns its summary. Used by
    /// `resume_all` so one un-resumable run does not abort recovery of the others.
    async fn fail_run(
        &self,
        workflow: &Workflow,
        state: &mut RunState,
        error: &str,
    ) -> Result<RunSummary> {
        state.status = RunStatus::Failed;
        state.error = Some(error.to_owned());
        state.updated_at = Utc::now();
        self.checkpoint(workflow.durable, state).await?;
        // Emit the terminal audit event for a run failed off the main `run()` path (a resume
        // that couldn't recover, a bad-params rerun) — otherwise it has a `RunStarted` with no
        // matching `RunFinished`.
        self.emit(
            state.run_id,
            workflow.durable,
            RunEvent::RunFinished {
                status: RunStatus::Failed,
                at: Utc::now(),
            },
        )
        .await;
        let summary = Self::summary_from_state(state.clone());
        let elapsed_ms = (Utc::now() - state.created_at).num_milliseconds();
        Self::log_run_outcome(&summary, elapsed_ms);
        Ok(summary)
    }

    /// Resumes a single already-loaded run to its next stopping point (terminal, or paused
    /// again at a gate). The caller MUST hold the run's [`claim_run`] guard for the duration —
    /// two concurrent resumes of one run would double-run its side effects. Returns `Ok(None)`
    /// if the run can't be resumed because its workspace handle is absent.
    #[allow(clippy::too_many_lines)]
    async fn resume_state(
        &self,
        workflow: &Workflow,
        mut state: RunState,
    ) -> Result<Option<RunSummary>> {
        let sweep_cancel = CancelToken::new();
        let Some(handle) = state.workspace.clone() else {
            return Ok(None);
        };
        let started_at = state.created_at;

        // Crash recovery is per-run, never all-or-nothing: one run's failure must not
        // abort the others or leave it stuck Running forever.
        let summary = if handle.path.exists() {
            match Self::resolve_params(workflow, &state.input) {
                Ok(params) => {
                    let cancel = CancelToken::new();
                    let _cancel_guard = self.register_cancel(state.run_id, cancel.clone());
                    let _cancel_watcher =
                        self.spawn_cancel_watcher(state.run_id, workflow.durable, cancel.clone());
                    let run_span = tracing::info_span!(
                        "run",
                        run_id = %state.run_id,
                        workflow = %workflow.name,
                        durable = workflow.durable,
                        resumed = true,
                    );
                    tracing::info!(parent: &run_span, "resuming run");
                    self.emit(
                        state.run_id,
                        workflow.durable,
                        RunEvent::RunResumed { at: Utc::now() },
                    )
                    .await;
                    let exec = self
                        .execute(workflow, &mut state, &handle.path.clone(), &params, &cancel)
                        .instrument(run_span)
                        .await;
                    match exec {
                        // Paused again at an approval gate — keep the workspace + ref; the
                        // run stays AwaitingApproval until the next decision.
                        Ok(r) if r.suspended.is_some() => {
                            let gate = r.suspended.clone();
                            state.status = RunStatus::AwaitingApproval;
                            state.updated_at = Utc::now();
                            self.checkpoint(workflow.durable, &state).await?;
                            self.emit_suspended(
                                state.run_id,
                                workflow.durable,
                                crate::traits::SuspendReason::Approval,
                            )
                            .await;
                            tracing::info!(run_id = %state.run_id, gate = ?gate, "resumed run paused for approval");
                            Self::suspended_summary(
                                state.run_id,
                                workflow,
                                &r,
                                started_at,
                                &state.steps,
                            )
                        }
                        terminal => {
                            // Graceful-shutdown during resume: same as run() — keep the durable
                            // run resumable (workspace + refs kept, checkpointed non-terminal) so
                            // the NEXT start completes it, rather than ending it `Cancelled`.
                            if workflow.durable && cancel.is_shutdown() {
                                if let Ok(r) = &terminal {
                                    state.status = RunStatus::Running;
                                    state.error = None;
                                    state.updated_at = Utc::now();
                                    self.checkpoint(workflow.durable, &state).await?;
                                    self.emit_suspended(
                                        state.run_id,
                                        workflow.durable,
                                        crate::traits::SuspendReason::Shutdown,
                                    )
                                    .await;
                                    tracing::info!(run_id = %state.run_id, "resumed run suspended for shutdown; will resume again");
                                    return Ok(Some(Self::interrupted_summary(
                                        state.run_id,
                                        workflow,
                                        r,
                                        started_at,
                                        &state.steps,
                                    )));
                                }
                            }
                            // Reclaim the run's snapshot refs unconditionally (matches run()
                            // and the unserved-workflow path): a retrying step snapshots even
                            // in a non-durable run, and the durable flag may have flipped, so
                            // either can leave a ref. `delete_snapshot_ref` no-ops if none.
                            self.delete_snapshot_ref(&handle.path, state.run_id, &cancel)
                                .await;
                            let workspace = self.make_workspace(&workflow.workspace);
                            self.release_workspace(&workspace, handle).await;
                            match terminal {
                                Ok(result) => {
                                    // `summarize` derives the run-level error from the first
                                    // failed step's recorded reason (single source of truth).
                                    let (mut status, mut summary) = Self::summarize(
                                        state.run_id,
                                        workflow,
                                        result,
                                        None,
                                        started_at,
                                    );
                                    // Read once (see run()'s terminal path) so status/event agree.
                                    let cancelled = cancel.is_cancelled();
                                    if cancelled {
                                        status = RunStatus::Cancelled;
                                        summary.status = RunStatus::Cancelled;
                                        summary.error = Some("run cancelled".to_owned());
                                    }
                                    state.status = status;
                                    state.error = summary.error.clone();
                                    state.updated_at = Utc::now();
                                    self.checkpoint(workflow.durable, &state).await?;
                                    if cancelled {
                                        self.emit_cancelled(
                                            state.run_id,
                                            workflow.durable,
                                            &cancel,
                                        )
                                        .await;
                                    }
                                    // A run completing via the resume path (crash recovery or an
                                    // approval decision) must still emit its terminal audit event
                                    // — only `run()` did before, so resumed/approved runs left a
                                    // started-but-never-finished trail.
                                    self.emit(
                                        state.run_id,
                                        workflow.durable,
                                        RunEvent::RunFinished {
                                            status,
                                            at: Utc::now(),
                                        },
                                    )
                                    .await;
                                    // Log the resumed run's terminal outcome too (failed → ERROR),
                                    // so a run that completes via crash-recovery/approval isn't
                                    // silent at the default level. (`fail_run` logs its own paths.)
                                    let elapsed_ms =
                                        (Utc::now() - state.created_at).num_milliseconds();
                                    Self::log_run_outcome(&summary, elapsed_ms);
                                    summary
                                }
                                Err(e) => {
                                    self.fail_run(workflow, &mut state, &e.to_string()).await?
                                }
                            }
                        }
                    }
                }
                Err(e) => self.fail_run(workflow, &mut state, &e.to_string()).await?,
            }
        } else {
            // The workspace is gone (host moved, manual cleanup); cannot resume. The
            // snapshot refs live in the shared main repo, so reclaim them there (best
            // effort, no-op when none) even though the worktree dir is gone — unconditional
            // for the same reason as the path above.
            self.delete_snapshot_ref(&self.repo_root, state.run_id, &sweep_cancel)
                .await;
            self.fail_run(workflow, &mut state, "workspace is gone; cannot resume")
                .await?
        };
        Ok(Some(summary))
    }
}
