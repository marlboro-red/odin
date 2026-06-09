//! The `loop:` body driver for the executor.
//!
//! `run_loop` — the sequential mini-driver carved out of `local.rs`. Unlike the concurrent
//! top-level scheduler, a loop body runs its inner steps in dependency order, evaluates the
//! `until` guard after each full iteration, and repeats — **accumulating** workdir edits across
//! iterations (rather than rewinding like single-step retry) — folding the inner steps' side
//! effects and usage into one aggregate outcome. A second `impl LocalEngine` block (a child
//! module of `local`, so it drives the engine's own `dispatch`/`gitio`/event helpers directly).

use std::collections::HashMap;
use std::path::Path;

use chrono::Utc;
use indexmap::IndexMap;
use serde_json::Value;

use super::ctx::{FEEDBACK_MAX, build_ctx_with, clip_tail, effective_timeout};
use super::{LocalEngine, StepOutcome};
use crate::api::{SideEffect, StepStatus};
use crate::context::render::eval_when;
use crate::error::Result;
use crate::ids::{RunId, StepId};
use crate::ir::{Step, Workflow};
use crate::traits::{CancelToken, LoopProgress, RunState, StepState};
use crate::usage::Usage;

impl LocalEngine {
    /// Drives a `loop:` body: runs the inner steps sequentially in dependency order, evaluates the
    /// `until` guard after each full iteration, and repeats — **accumulating** workdir edits across
    /// iterations (unlike single-step retry, which rewinds) — until `until` holds or `max`
    /// iterations elapse.
    ///
    /// Returns one aggregate outcome: Passed with `outputs.iterations` (count) and
    /// `outputs.converged = true` on success; Failed when the cap is hit without `until` holding,
    /// or when `until` errors. Inner-step states are **transient** — they shape each iteration's
    /// template context but never enter `state.steps` (which would collide and confuse resume); the
    /// loop appears to the rest of the run as a single step. Inner side effects and usage are folded
    /// up into the aggregate outcome.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) async fn run_loop(
        &self,
        run_id: RunId,
        step: &Step,
        l: &crate::ir::LoopStep,
        state: &mut RunState,
        workdir: &Path,
        params: &IndexMap<String, Value>,
        base_commit: Option<&str>,
        base_diff: Option<&str>,
        workflow: &Workflow,
        cancel: &CancelToken,
    ) -> Result<StepOutcome> {
        let durable = workflow.durable;
        let loop_ref = format!("refs/odin/loop/{run_id}/{}", step.id.as_str());
        let max = u32::from(l.max);
        let inner_order = crate::validate::graph::topo_order(&l.steps)
            .unwrap_or_else(|_| l.steps.iter().map(|s| s.id.clone()).collect());
        let inner_by_id: HashMap<&str, &Step> =
            l.steps.iter().map(|s| (s.id.as_str(), s)).collect();

        let mut diff = base_diff.map(str::to_owned);
        let mut feedback = String::new();
        let mut side_effects: Vec<SideEffect> = Vec::new();
        let mut usage = Usage::default();

        // Resume: if a clean per-iteration snapshot survives, restore the workdir to it and
        // re-enter at the next iteration (skipping the ones that already completed). Only while
        // HEAD is still at base — once an inner step committed, rewinding would corrupt the run
        // branch, so a committed loop restarts from iteration 1 (the documented degradation).
        let mut start: u32 = 0;
        if let Some(p) = state.loop_state.get(&step.id) {
            if let Some(snap) = p.iteration_snapshot.clone() {
                let at_base = base_commit.is_some()
                    && self.git_head(workdir, cancel).await.as_deref() == base_commit;
                // Advance `start` ONLY if the workdir was actually restored — a reaped snapshot
                // (read-tree failed) leaves the tree without those iterations' work, so we must
                // restart from iteration 1 rather than skip to a later one.
                if at_base && self.restore_workdir(workdir, &snap, cancel).await {
                    start = p.last_completed_iteration;
                    feedback = p.feedback.clone().unwrap_or_default();
                }
            }
        }

        let mut iteration: u32 = start;
        let mut outcome = loop {
            iteration += 1;
            // Per-iteration inner states (transient): each inner step sees the outer run plus the
            // earlier inner steps of THIS iteration.
            let mut inner_states: IndexMap<StepId, StepState> = IndexMap::new();
            let mut last_failure: Option<String> = None;

            for inner_id in &inner_order {
                let inner = inner_by_id[inner_id.as_str()];
                let mut steps_view = state.steps.clone();
                for (k, v) in &inner_states {
                    steps_view.insert(k.clone(), v.clone());
                }
                let ctx = build_ctx_with(
                    params,
                    &state.input.trigger_payload,
                    &steps_view,
                    diff.as_deref(),
                    state,
                    workflow,
                    iteration,
                    &feedback,
                );
                let deps_passed = inner.depends_on.iter().all(|d| {
                    matches!(
                        inner_states.get(d).map(|s| s.status),
                        Some(StepStatus::Passed)
                    )
                });
                let timeout = effective_timeout(inner, &workflow.defaults);
                let (_, o) = self
                    .run_one(
                        run_id,
                        durable,
                        inner,
                        ctx,
                        workdir,
                        timeout,
                        deps_passed,
                        workflow.defaults.retry.as_ref(),
                        cancel,
                    )
                    .await;
                // Pair the StepStarted that `run_one` emitted (for an inner step that executed)
                // with a StepFinished — plus any gate/judge results — so a loop body's inner steps
                // produce the same audit trail as top-level steps.
                self.emit_step_events(run_id, durable, inner_id, &o).await;
                if let Some(u) = &o.usage {
                    usage.add(*u);
                }
                side_effects.extend(o.side_effects.iter().cloned());
                // A passing non-scratch inner step accumulates into the shared DIFF.
                if matches!(o.status, StepStatus::Passed) && !inner.scratch {
                    diff = self.capture_diff(workdir, base_commit, cancel).await;
                }
                if matches!(o.status, StepStatus::Failed) {
                    last_failure = o.failure_detail.clone().or_else(|| o.error.clone());
                }
                inner_states.insert(inner_id.clone(), o.to_state());
            }

            // Evaluate `until` after the whole body — every inner step has run, so all are visible.
            let mut steps_view = state.steps.clone();
            for (k, v) in &inner_states {
                steps_view.insert(k.clone(), v.clone());
            }
            let until_ctx = build_ctx_with(
                params,
                &state.input.trigger_payload,
                &steps_view,
                diff.as_deref(),
                state,
                workflow,
                iteration,
                &feedback,
            );
            // `iterations` (count so far) and `converged` are reported on every terminal path, so
            // a hit-the-cap failure is as introspectable as a success.
            let outputs = |converged: bool| {
                let mut o = IndexMap::new();
                o.insert("iterations".to_owned(), Value::from(iteration));
                o.insert("converged".to_owned(), Value::Bool(converged));
                o
            };
            match eval_when(&l.until, &until_ctx) {
                Ok(true) => break StepOutcome::passing(0, outputs(true), None),
                Ok(false) => {
                    if iteration >= max {
                        let mut o = StepOutcome::failed(format!(
                            "loop did not satisfy `until` within {max} iteration(s)"
                        ));
                        o.outputs = outputs(false);
                        break o;
                    }
                    feedback = clip_tail(&last_failure.unwrap_or_default(), FEEDBACK_MAX);
                    // Durable per-iteration checkpoint: snapshot the accumulated workdir (only
                    // while HEAD is still at base — an inner commit disengages snapshots) and
                    // record progress, so a crash before the next iteration resumes here.
                    let snapshot = if durable {
                        let at_base = base_commit.is_some()
                            && self.git_head(workdir, cancel).await.as_deref() == base_commit;
                        match base_commit.filter(|_| at_base) {
                            Some(base) => {
                                self.snapshot_to_ref(workdir, base, run_id, &loop_ref, cancel)
                                    .await
                            }
                            None => None,
                        }
                    } else {
                        None
                    };
                    state.loop_state.insert(
                        step.id.clone(),
                        LoopProgress {
                            last_completed_iteration: iteration,
                            iteration_snapshot: snapshot,
                            feedback: Some(feedback.clone()),
                        },
                    );
                    state.updated_at = Utc::now();
                    self.checkpoint(durable, state).await?;
                }
                Err(e) => {
                    let mut o = StepOutcome::failed(format!("loop `until` evaluation failed: {e}"));
                    o.outputs = outputs(false);
                    break o;
                }
            }
        };
        // The loop settled: clear its progress and drop the snapshot ref so its dangling commit is
        // collectable. The caller's post-step block persists the cleared `loop_state`.
        state.loop_state.shift_remove(&step.id);
        if durable {
            self.delete_ref(workdir, &loop_ref, cancel).await;
        }
        outcome.usage = Some(usage);
        outcome.side_effects = side_effects;
        Ok(outcome)
    }
}
