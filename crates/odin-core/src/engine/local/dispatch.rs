//! Step execution and retry for the executor.
//!
//! The per-step pipeline carved out of `local.rs`: param resolution, prompt rendering, the
//! `dispatch` arm that runs each step kind (provider / run / action / case), the gate + judge
//! verification (`exec_step`/`run_judge`), the retry/backoff loop (`run_with_retry`), the shared
//! `shell` hook, and the scratch-aware `run_one` entry point the scheduler calls. A second `impl
//! LocalEngine` block (a child module of `local`, so it calls the engine's own private helpers —
//! `emit`, the `gitio`/`provision` primitives — directly).

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use indexmap::IndexMap;
use serde_json::Value;

use super::ctx::{
    STDOUT_MAX, attempt_context, backoff_delay, clip_middle, effective_retry, failure_detail,
    join_streams, parse_score, skipped_outcome, with_stderr_tail,
};
use super::{DIFF, LocalEngine, StepOutcome};
use crate::api::{RunInput, StepStatus};
use crate::context::render::{eval_when, render_template};
use crate::error::{Error, Result};
use crate::ids::{RunId, StepId};
use crate::ir::{JudgeSpec, RetrySpec, Step, StepKind, Workflow};
use crate::provider::process::{ProcessOptions, ProcessOutput, StreamSink, run_process};
use crate::traits::{ActionCtx, CancelToken, InvocationCtx, RunEvent};

impl LocalEngine {
    /// Resolves declared params (input value or default), erroring on a missing required
    /// one, and passes through any extra provided params.
    pub(crate) fn resolve_params(
        workflow: &Workflow,
        input: &RunInput,
    ) -> Result<IndexMap<String, Value>> {
        let mut params = IndexMap::new();
        for (name, spec) in &workflow.params {
            let value = input
                .params
                .get(name.as_str())
                .cloned()
                .or_else(|| spec.default.clone());
            match value {
                Some(v) => {
                    if !spec.ty.matches(&v) {
                        return Err(Error::Input(format!(
                            "param {:?} expects type {} but got {v}",
                            name.as_str(),
                            spec.ty.name()
                        )));
                    }
                    params.insert(name.as_str().to_owned(), v);
                }
                None if spec.required => {
                    return Err(Error::Input(format!(
                        "missing required param {:?}",
                        name.as_str()
                    )));
                }
                None => {}
            }
        }
        for (k, v) in &input.params {
            params.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Ok(params)
    }

    /// Renders the effective prompt for a provider step (inline or from a file).
    fn render_prompt(
        &self,
        provider: &crate::ir::ProviderStep,
        ctx: &minijinja::Value,
    ) -> std::result::Result<String, String> {
        let template = if let Some(p) = &provider.prompt {
            p.clone()
        } else if let Some(file) = &provider.prompt_file {
            // Contain prompt_file under the repo root: reject absolute paths and `..`
            // escapes (the path is author-controlled YAML).
            let resolved = self
                .repo_root
                .join(file)
                .canonicalize()
                .map_err(|e| format!("reading prompt_file {file:?}: {e}"))?;
            let root = self
                .repo_root
                .canonicalize()
                .unwrap_or_else(|_| self.repo_root.clone());
            if !resolved.starts_with(&root) {
                return Err(format!("prompt_file {file:?} escapes the repository root"));
            }
            std::fs::read_to_string(&resolved)
                .map_err(|e| format!("reading prompt_file {file:?}: {e}"))?
        } else {
            return Err("provider step has no prompt".to_owned());
        };
        render_template(&template, ctx, "prompt").map_err(|e| e.to_string())
    }

    /// Dispatches a step to its provider / shell `run:` / action body, before gates.
    #[allow(clippy::too_many_lines)]
    async fn dispatch(
        &self,
        step: &Step,
        ctx: &minijinja::Value,
        workdir: &Path,
        timeout: Option<Duration>,
        cancel: &CancelToken,
    ) -> StepOutcome {
        match &step.kind {
            StepKind::Provider(p) => {
                let prompt = match self.render_prompt(p, ctx) {
                    Ok(s) => s,
                    Err(e) => return StepOutcome::failed(e),
                };
                tracing::debug!(
                    step = %step.id,
                    provider = %p.provider.as_str(),
                    prompt_bytes = prompt.len(),
                    "invoking provider"
                );
                let Some(provider) = self.registry.provider(p.provider.as_str()).cloned() else {
                    return StepOutcome::failed(format!(
                        "provider {:?} is not registered",
                        p.provider.as_str()
                    ));
                };
                let inputs = step
                    .artifacts
                    .requires
                    .iter()
                    .filter(|a| a.as_str() != DIFF)
                    .map(|a| (a.clone(), workdir.join(a.as_str())))
                    .collect();
                let ictx = InvocationCtx {
                    step_id: step.id.clone(),
                    workdir: workdir.to_path_buf(),
                    prompt: Some(prompt),
                    inputs,
                    timeout,
                    cancel: cancel.clone(),
                    stream: self.step_stream(step.id.as_str()),
                };
                match provider.invoke(ictx).await {
                    Ok(o) => {
                        let mut outputs = o.outputs;
                        // Cap the persisted/exposed stdout so a runaway agent can't bloat the
                        // run-state blob (re-serialized at every later checkpoint). 1 MiB is far
                        // beyond any real answer, so this only bites pathological output.
                        outputs
                            .entry("stdout".to_owned())
                            .or_insert(Value::String(clip_middle(&o.stdout, STDOUT_MAX)));
                        let mut outcome = StepOutcome::passing(o.exit_code, outputs, o.usage);
                        outcome.stderr = o.stderr;
                        outcome
                    }
                    Err(e) => StepOutcome::failed(format!("provider error: {e}")),
                }
            }
            StepKind::Run(r) => {
                let cmd = match render_template(&r.run, ctx, "run") {
                    Ok(s) => s,
                    Err(e) => return StepOutcome::failed(e.to_string()),
                };
                let stream = self.step_stream(step.id.as_str());
                match self
                    .shell(&cmd, workdir, timeout, cancel, stream.as_ref())
                    .await
                {
                    Ok(out) => {
                        // A timeout/cancel kill exits the child with code -1; record WHY instead of
                        // the misleading "exited with code -1" `exec_step` would otherwise synthesize
                        // (a timed-out `run:` step was previously indistinguishable from a crash).
                        let reason = killed_reason(&out, timeout);
                        let mut outputs = IndexMap::new();
                        outputs.insert(
                            "stdout".to_owned(),
                            Value::String(clip_middle(&out.stdout, STDOUT_MAX)),
                        );
                        let mut outcome = StepOutcome::passing(out.exit_code, outputs, None);
                        outcome.stderr = out.stderr;
                        if let Some(reason) = reason {
                            outcome.status = StepStatus::Failed;
                            outcome.failure_detail = Some(failure_detail(&reason, &outcome.stderr));
                            outcome.error = Some(with_stderr_tail(&reason, &outcome.stderr));
                        }
                        outcome
                    }
                    Err(e) => StepOutcome::failed(format!("run error: {e}")),
                }
            }
            StepKind::Action(a) => {
                let Some(action) = self.registry.action(&a.action).cloned() else {
                    return StepOutcome::failed(format!("action {:?} is not registered", a.action));
                };
                let mut args = IndexMap::new();
                for (k, v) in &a.with {
                    let rendered = match v.as_str() {
                        Some(s) => match render_template(s, ctx, "with") {
                            Ok(r) => Value::String(r),
                            Err(e) => return StepOutcome::failed(e.to_string()),
                        },
                        None => v.clone(),
                    };
                    args.insert(k.clone(), rendered);
                }
                let actx = ActionCtx {
                    step_id: step.id.clone(),
                    workdir: workdir.to_path_buf(),
                    args,
                    // Plumb the run's cancel + the step timeout so a hung action (e.g. an
                    // interactive auth prompt) is killed instead of wedging the whole run.
                    cancel: cancel.clone(),
                    timeout,
                };
                match action.run(actx).await {
                    Ok(o) => {
                        let mut outcome = StepOutcome::passing(o.exit_code, o.outputs, None);
                        outcome.side_effects = o.side_effects;
                        // Carry the action's stderr so a non-zero exit keeps its real error in the
                        // failure reason / `retry.feedback` (the exit-code check in `exec_step`).
                        outcome.stderr = o.stderr;
                        outcome
                    }
                    Err(e) => StepOutcome::failed(format!("action error: {e}")),
                }
            }
            // Approval gates are resolved synchronously by the scheduler (`execute`), which has
            // the run's recorded decisions; they never reach `dispatch`.
            StepKind::Approval(_) => {
                StepOutcome::failed("internal: an approval gate was dispatched".to_owned())
            }
            // A `case:` selector: evaluate the branch guards in order; the first true (else the
            // `else` label) wins. Records `outputs.selected = <label>` and always passes —
            // branching is a decision, never a failure. The author's branch-body steps are gated
            // on this `selected` value, and a join downstream depends on the selector.
            StepKind::Case(c) => {
                let mut selected: Option<String> = None;
                for b in &c.branches {
                    let matched = match &b.when {
                        Some(expr) => match eval_when(expr, ctx) {
                            Ok(m) => m,
                            Err(e) => {
                                return StepOutcome::failed(format!(
                                    "case guard for branch {:?}: {e}",
                                    b.label
                                ));
                            }
                        },
                        None => true,
                    };
                    if matched {
                        selected = Some(b.label.clone());
                        break;
                    }
                }
                let selected = selected.or_else(|| c.else_.clone()).unwrap_or_default();
                let mut outputs = IndexMap::new();
                outputs.insert("selected".to_owned(), Value::String(selected));
                StepOutcome::passing(0, outputs, None)
            }
            // A `loop:` step is driven by the scheduler (`execute`), which owns `&mut state` for
            // per-iteration checkpointing — like `approval:`, it never reaches `dispatch`. (The
            // iterating mini-driver lands in a follow-up; until then a loop step fails loudly here
            // rather than silently no-op'ing.)
            StepKind::Loop(_) => {
                StepOutcome::failed("internal: a loop step was dispatched".to_owned())
            }
        }
    }

    /// Executes one step: dispatch by kind, check the exit code, then run gates.
    async fn exec_step(
        &self,
        step: &Step,
        ctx: &minijinja::Value,
        workdir: &Path,
        timeout: Option<Duration>,
        cancel: &CancelToken,
    ) -> StepOutcome {
        let mut outcome = self.dispatch(step, ctx, workdir, timeout, cancel).await;

        if outcome.status == StepStatus::Failed {
            return outcome;
        }
        if outcome.exit_code.unwrap_or(0) != 0 {
            let code = outcome.exit_code.unwrap_or(-1);
            let headline = format!("exited with code {code}");
            outcome.failure_detail = Some(failure_detail(&headline, &outcome.stderr));
            outcome.status = StepStatus::Failed;
            outcome.error = Some(with_stderr_tail(&headline, &outcome.stderr));
            return outcome;
        }

        // Gates: every named command must exit 0. Capture BOTH streams — most verifiers put the
        // actionable detail on stdout (test runners: which assertion failed) while compilers use
        // stderr — so `retry.feedback` needs both to be useful.
        let gate_stream = self.step_stream(step.id.as_str());
        for (name, command) in &step.gates {
            let cmd = match render_template(command, ctx, "gate") {
                Ok(s) => s,
                Err(e) => {
                    outcome.status = StepStatus::Failed;
                    outcome.error = Some(e.to_string());
                    return outcome;
                }
            };
            let (passed, gate_output, killed) = match self
                .shell(&cmd, workdir, timeout, cancel, gate_stream.as_ref())
                .await
            {
                Ok(out) => (
                    out.exit_code == 0,
                    join_streams(&out.stdout, &out.stderr),
                    killed_reason(&out, timeout),
                ),
                Err(e) => (false, e.to_string(), None),
            };
            outcome.gates.insert(name.as_str().to_owned(), passed);
            if !passed {
                // Name a timeout/cancel explicitly rather than just "gate failed".
                let headline = match killed {
                    Some(reason) => format!("gate {:?} {reason}", name.as_str()),
                    None => format!("gate {:?} failed", name.as_str()),
                };
                outcome.failure_detail = Some(failure_detail(&headline, &gate_output));
                outcome.status = StepStatus::Failed;
                outcome.error = Some(with_stderr_tail(&headline, &gate_output));
            }
        }

        // LLM-as-judge: if the step otherwise passed, score its output against criteria.
        if outcome.status == StepStatus::Passed {
            if let Some(judge) = &step.judge {
                let output = outcome
                    .outputs
                    .get("stdout")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                match self
                    .run_judge(step, judge, &output, workdir, timeout, cancel)
                    .await
                {
                    Ok(score) => {
                        outcome.judge_score = Some(score);
                        if score < judge.threshold {
                            outcome.status = StepStatus::Failed;
                            outcome.error = Some(format!(
                                "judge score {score:.2} below threshold {:.2}",
                                judge.threshold
                            ));
                        }
                    }
                    Err(e) => {
                        outcome.status = StepStatus::Failed;
                        outcome.error = Some(format!("judge error: {e}"));
                    }
                }
            }
        }
        outcome
    }

    /// Scores a step's output via the judge provider, returning a `0.0..=1.0` score.
    async fn run_judge(
        &self,
        step: &Step,
        judge: &JudgeSpec,
        output: &str,
        workdir: &Path,
        timeout: Option<Duration>,
        cancel: &CancelToken,
    ) -> std::result::Result<f32, String> {
        let provider = self
            .registry
            .provider(judge.provider.as_str())
            .cloned()
            .ok_or_else(|| {
                format!(
                    "judge provider {:?} is not registered",
                    judge.provider.as_str()
                )
            })?;
        let prompt = format!(
            "You are scoring whether an output satisfies the given criteria. Respond with ONLY a \
             JSON object of the form {{\"score\": <number between 0.0 and 1.0>}}.\n\n\
             Criteria:\n{}\n\nOutput to score:\n{output}",
            judge.criteria
        );
        let ictx = InvocationCtx {
            step_id: step.id.clone(),
            workdir: workdir.to_path_buf(),
            prompt: Some(prompt),
            inputs: IndexMap::new(),
            timeout,
            cancel: cancel.clone(),
            stream: self.step_stream(&format!("{} judge", step.id.as_str())),
        };
        let out = provider.invoke(ictx).await.map_err(|e| e.to_string())?;
        parse_score(&out.stdout).ok_or_else(|| {
            format!(
                "could not parse a score from judge output: {}",
                out.stdout.trim()
            )
        })
    }

    /// Decides a step's outcome: skip if a dependency failed or `when` is false, else run
    /// it (with retry).
    #[allow(clippy::too_many_arguments)]
    async fn decide_outcome(
        &self,
        run_id: RunId,
        durable: bool,
        step: &Step,
        ctx: &minijinja::Value,
        workdir: &Path,
        timeout: Option<Duration>,
        deps_passed: bool,
        default_retry: Option<&RetrySpec>,
        cancel: &CancelToken,
    ) -> StepOutcome {
        if !deps_passed {
            return StepOutcome::failed("an upstream dependency did not pass").skipped();
        }
        match step.when.as_deref() {
            Some(expr) => match eval_when(expr, ctx) {
                Ok(true) => {
                    self.run_with_retry(
                        run_id,
                        durable,
                        step,
                        ctx,
                        workdir,
                        timeout,
                        default_retry,
                        cancel,
                    )
                    .await
                }
                Ok(false) => skipped_outcome(),
                Err(e) => StepOutcome::failed(format!("when: {e}")),
            },
            None => {
                self.run_with_retry(
                    run_id,
                    durable,
                    step,
                    ctx,
                    workdir,
                    timeout,
                    default_retry,
                    cancel,
                )
                .await
            }
        }
    }

    /// Runs a step, retrying on failure per its **effective** retry policy with backoff between
    /// attempts. The effective policy is the step's own `retry:` if it set one, else the workflow
    /// `defaults.retry` (`default_retry`) — so a bare step inherits the default. Emits a
    /// `StepStarted` event per attempt.
    #[allow(clippy::too_many_arguments)]
    async fn run_with_retry(
        &self,
        run_id: RunId,
        durable: bool,
        step: &Step,
        ctx: &minijinja::Value,
        workdir: &Path,
        timeout: Option<Duration>,
        default_retry: Option<&RetrySpec>,
        cancel: &CancelToken,
    ) -> StepOutcome {
        let retry = effective_retry(&step.retry, default_retry);
        // u32 so a (valid) `retry.max` of 255 can't overflow the attempt counter.
        let max_attempts = 1 + u32::from(retry.max);
        // If the step can retry, snapshot the tree as it is BEFORE the first attempt, so each
        // retry runs against that clean state rather than on top of the failed attempt's
        // partial edits (a failing `run:` that mutated the workdir would otherwise double-
        // apply). Best-effort and git-only: a non-git workdir yields `None` and we just retry
        // in place, as before. Applies to the shared workdir and to scratch worktrees alike.
        //
        // Anchor this transient snapshot under a step-scoped ref *separate* from the durable
        // per-run ref — otherwise it would move `refs/odin/run/<id>` off the commit the
        // persisted `RunState.snapshot` still names, leaving resume's target un-anchored and
        // gc-collectable. Nested under the run id (`refs/odin/retry/<id>/<step>`) so it is both
        // step-scoped (concurrent scratch retry steps don't collide) and sweepable as a group
        // by run-end cleanup if a crash/cancel mid-loop skips the per-step delete below.
        let retry_ref = format!("refs/odin/retry/{run_id}/{}", step.id.as_str());
        let pre_step_snapshot = if max_attempts > 1 {
            match self.git_head(workdir, cancel).await {
                Some(head) => {
                    self.snapshot_to_ref(workdir, &head, run_id, &retry_ref, cancel)
                        .await
                }
                None => None,
            }
        } else {
            None
        };
        let mut attempt: u32 = 0;
        // The prior attempt's failure reason, fed forward to the next attempt as `retry.feedback`.
        let mut prior_error: Option<String> = None;
        let outcome = loop {
            attempt += 1;
            // Before a *retry*, rewind the workdir to the pre-step snapshot.
            if attempt > 1 {
                if let Some(snapshot) = &pre_step_snapshot {
                    self.restore_workdir(workdir, snapshot, cancel).await;
                }
            }
            self.emit(
                run_id,
                durable,
                RunEvent::StepStarted {
                    step: step.id.clone(),
                    attempt: u8::try_from(attempt).unwrap_or(u8::MAX),
                    at: Utc::now(),
                },
            )
            .await;
            // Per-attempt context: always exposes `retry.attempt`; on a retry with `feedback`
            // enabled, `retry.feedback` carries the prior failure so the prompt can address it.
            let attempt_ctx = attempt_context(ctx, retry.feedback, attempt, prior_error.as_deref());
            let mut outcome = self
                .exec_step(step, &attempt_ctx, workdir, timeout, cancel)
                .await;
            if outcome.status != StepStatus::Failed || attempt >= max_attempts {
                outcome.attempts = u8::try_from(attempt).unwrap_or(u8::MAX);
                break outcome;
            }
            // Stop retrying the instant a cancel/shutdown fires: the next attempt's subprocess
            // would be killed immediately anyway, so burning the backoff sleep and spawning doomed
            // attempts only delays the run settling. Without this a cancelled step can churn
            // through every remaining retry's backoff before it stops.
            if cancel.is_cancelled() {
                outcome.attempts = u8::try_from(attempt).unwrap_or(u8::MAX);
                break outcome;
            }
            // Feed the *un-wrapped* diagnostic forward (so `concise` sees real content, not a
            // synthetic headline); fall back to the summary `error` for failure kinds without a
            // dedicated detail (judge/provider/action errors are already single-line).
            prior_error = outcome
                .failure_detail
                .clone()
                .or_else(|| outcome.error.clone());
            tokio::time::sleep(backoff_delay(retry.backoff, attempt)).await;
        };
        // The retry rewind point is dead once the step settles; drop its ref so the snapshot
        // commit becomes collectable (and doesn't outlive the run as a dangling anchor).
        if pre_step_snapshot.is_some() {
            self.delete_ref(workdir, &retry_ref, cancel).await;
        }
        outcome
    }

    /// Runs a shell command in `workdir`, returning `(exit_code, stdout, stderr)`. With a
    /// `stream` sink (the `--stream` view) the command's output is teed to the terminal live.
    async fn shell(
        &self,
        command: &str,
        workdir: &Path,
        timeout: Option<Duration>,
        cancel: &CancelToken,
        stream: Option<&StreamSink>,
    ) -> Result<crate::provider::process::ProcessOutput> {
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            timeout,
            env: Vec::new(),
            stdin: None,
            stream: stream.cloned(),
        };
        let args = vec!["-c".to_owned(), command.to_owned()];
        Ok(run_process(crate::provider::posix_shell()?, &args, &opts, cancel).await?)
    }
    /// Runs one step, provisioning an isolated scratch worktree first if `step.scratch`. A
    /// scratch step's file edits stay in its throwaway worktree; its diff is surfaced as
    /// `outputs.diff` and the worktree is removed. Returns `(id, outcome)` for the driver.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(name = "step", skip_all, fields(step = %step.id, scratch = step.scratch))]
    pub(crate) async fn run_one(
        &self,
        run_id: RunId,
        durable: bool,
        step: &Step,
        ctx: minijinja::Value,
        base_workdir: &Path,
        timeout: Option<Duration>,
        deps_passed: bool,
        default_retry: Option<&RetrySpec>,
        cancel: &CancelToken,
    ) -> (StepId, StepOutcome) {
        let id = step.id.clone();
        // A non-scratch step, or one whose deps did not pass (it will be skipped without
        // touching any workdir), runs against the shared workdir — no worktree needed.
        if !step.scratch || !deps_passed {
            let outcome = self
                .decide_outcome(
                    run_id,
                    durable,
                    step,
                    &ctx,
                    base_workdir,
                    timeout,
                    deps_passed,
                    default_retry,
                    cancel,
                )
                .await;
            return (id, outcome);
        }
        match self
            .acquire_scratch(run_id, base_workdir, &id, cancel)
            .await
        {
            Ok(scratch) => {
                let mut outcome = self
                    .decide_outcome(
                        run_id,
                        durable,
                        step,
                        &ctx,
                        &scratch,
                        timeout,
                        deps_passed,
                        default_retry,
                        cancel,
                    )
                    .await;
                // Capture the candidate's diff for any step that actually executed — Passed
                // *or* Failed (a failed candidate's partial work is still worth inspecting by
                // a downstream judge). A `Skipped` step (e.g. `when: false`) ran nothing.
                if !matches!(outcome.status, StepStatus::Skipped) {
                    // The scratch worktree is detached at the shared tree's HEAD, so diffing
                    // vs `HEAD` captures the candidate's full changes — staged *and* unstaged,
                    // consistent with the base-relative shared DIFF.
                    if let Some(d) = self.capture_diff(&scratch, Some("HEAD"), cancel).await {
                        outcome.outputs.insert("diff".to_owned(), Value::String(d));
                    }
                }
                self.release_scratch(base_workdir, &scratch, cancel).await;
                (id, outcome)
            }
            Err(e) => (id, StepOutcome::failed(format!("scratch workspace: {e}"))),
        }
    }
}

/// Why a subprocess was killed, for the step failure headline — so a timed-out or cancelled
/// `run:`/gate step reads "timed out after 30s" / "cancelled" instead of the bare, misleading
/// "exited with code -1" the synthetic exit code would otherwise produce. `None` for a normal
/// (non-zero) exit, which the caller reports as usual.
fn killed_reason(out: &ProcessOutput, timeout: Option<Duration>) -> Option<String> {
    if out.timed_out {
        Some(match timeout {
            Some(d) => format!("timed out after {d:?}"),
            None => "timed out".to_owned(),
        })
    } else if out.cancelled {
        Some("cancelled".to_owned())
    } else {
        None
    }
}
