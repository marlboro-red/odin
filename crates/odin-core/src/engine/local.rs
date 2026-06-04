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
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use indexmap::IndexMap;
use serde_json::{Value, json};
use tracing::Instrument as _;

use super::Engine;
use crate::api::{
    ApprovalDecision, Decision, RerunOutcome, RunInput, RunStatus, RunSummary, SideEffect,
    StepResult, StepStatus,
};
use crate::context::render::{build_context, eval_when, render_template};
use crate::error::{Error, Result};
use crate::ids::{RunId, StepId};
use crate::ir::{Backoff, FeedbackMode, JudgeSpec, Step, StepKind, Workflow, WorkspaceConfig};
use crate::provider::process::{ProcessOptions, run_process};
use crate::registry::Registry;
use crate::traits::{
    AcquireCtx, ActionCtx, CancelToken, InvocationCtx, LoopProgress, PrunePolicy, PruneReport,
    RunEvent, RunState, StepState, Store, Workspace, WorkspaceHandle,
};
use crate::usage::Usage;
use crate::workspace::{SlotPoolWorkspace, WorktreeWorkspace};

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
}

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

/// What executing a single step produced (richer than the persisted `StepState`).
struct StepOutcome {
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
    ) -> Self {
        Self {
            registry,
            store,
            repo_root,
            worktree_lock: tokio::sync::Mutex::new(()),
            workspaces: std::sync::Mutex::new(HashMap::new()),
            running: std::sync::Mutex::new(HashSet::new()),
        }
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

    fn make_workspace(&self, cfg: &WorkspaceConfig) -> Arc<dyn Workspace> {
        // The workspace `type` the workflow declares, as a registry key.
        let kind = match cfg {
            WorkspaceConfig::Worktree(_) => "worktree",
            WorkspaceConfig::SlotPool(_) => "slot_pool",
        };
        // A custom workspace registered under this kind overrides the built-in (the same
        // last-writer-wins override `register_provider` gives providers). This is the live
        // path for `Registry::register_workspace`. NB: the workflow IR's `workspace.type` is
        // still a closed set (worktree / slot_pool), so an embedder can *replace* a built-in
        // kind but cannot yet introduce a brand-new `type:` string from YAML.
        if let Some(workspace) = self.registry.workspace(kind) {
            return Arc::clone(workspace);
        }
        // Cache one built-in instance per (kind, config) for the engine's lifetime, so all
        // runs and resumes share it. `slot_pool`'s concurrency cap and lease bookkeeping are
        // in-memory and per-instance — a fresh instance per run would defeat them entirely.
        let key = format!("{kind}|{}", serde_json::to_string(cfg).unwrap_or_default());
        if let Some(workspace) = self.workspaces.lock().unwrap().get(&key) {
            return Arc::clone(workspace);
        }
        let workspace: Arc<dyn Workspace> = match cfg {
            WorkspaceConfig::Worktree(_) => {
                Arc::new(WorktreeWorkspace::new(self.repo_root.clone()))
            }
            WorkspaceConfig::SlotPool(c) => Arc::new(SlotPoolWorkspace::new(
                self.repo_root.clone(),
                self.repo_root.join(".odin").join("slots"),
                c.pool as usize,
                c.reset,
            )),
        };
        self.workspaces
            .lock()
            .unwrap()
            .insert(key, Arc::clone(&workspace));
        workspace
    }

    /// Acquires a workspace, serializing `worktree` acquisition under the same lock as scratch
    /// worktrees. `git worktree add`/`remove` mutate the repo's shared `.git/worktrees/`
    /// metadata, which is NOT safe for concurrent runs to touch at once — a concurrent add and
    /// remove corrupt it (`fatal: failed to read .../commondir`), failing a run. `slot_pool`
    /// (and custom kinds) acquire without the lock; they don't touch worktree metadata.
    async fn acquire_workspace(
        &self,
        workspace: &Arc<dyn Workspace>,
        ctx: AcquireCtx,
    ) -> std::result::Result<WorkspaceHandle, crate::error::WorkspaceError> {
        if workspace.kind() == "worktree" {
            let _guard = self.worktree_lock.lock().await;
            workspace.acquire(ctx).await
        } else {
            workspace.acquire(ctx).await
        }
    }

    /// Releases a workspace (best effort), serialized for `worktree` kinds — see
    /// [`acquire_workspace`](Self::acquire_workspace).
    async fn release_workspace(&self, workspace: &Arc<dyn Workspace>, handle: WorkspaceHandle) {
        if workspace.kind() == "worktree" {
            let _guard = self.worktree_lock.lock().await;
            let _ = workspace.release(handle).await;
        } else {
            let _ = workspace.release(handle).await;
        }
    }

    async fn checkpoint(&self, durable: bool, state: &RunState) -> Result<()> {
        if durable {
            if let Some(store) = &self.store {
                store.checkpoint(state).await?;
            }
        }
        Ok(())
    }

    async fn emit(&self, run_id: RunId, event: RunEvent) {
        if let Some(store) = &self.store {
            let _ = store.append_event(run_id, &event).await;
        }
    }

    /// Appends the gate/judge/finished audit events for a completed step.
    async fn emit_step_events(&self, run_id: RunId, id: &StepId, outcome: &StepOutcome) {
        for (gate, passed) in &outcome.gates {
            self.emit(
                run_id,
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
    /// is unresumable without persistence; so this checkpoint always writes.)
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

    /// Resolves declared params (input value or default), erroring on a missing required
    /// one, and passes through any extra provided params.
    fn resolve_params(workflow: &Workflow, input: &RunInput) -> Result<IndexMap<String, Value>> {
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
                };
                match provider.invoke(ictx).await {
                    Ok(o) => {
                        let mut outputs = o.outputs;
                        outputs
                            .entry("stdout".to_owned())
                            .or_insert(Value::String(o.stdout));
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
                match self.shell(&cmd, workdir, timeout, cancel).await {
                    Ok((code, stdout, stderr)) => {
                        let mut outputs = IndexMap::new();
                        outputs.insert("stdout".to_owned(), Value::String(stdout));
                        let mut outcome = StepOutcome::passing(code, outputs, None);
                        outcome.stderr = stderr;
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
                };
                match action.run(actx).await {
                    Ok(o) => StepOutcome {
                        side_effects: o.side_effects,
                        ..StepOutcome::passing(o.exit_code, o.outputs, None)
                    },
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
        for (name, command) in &step.gates {
            let cmd = match render_template(command, ctx, "gate") {
                Ok(s) => s,
                Err(e) => {
                    outcome.status = StepStatus::Failed;
                    outcome.error = Some(e.to_string());
                    return outcome;
                }
            };
            let (passed, gate_output) = match self.shell(&cmd, workdir, timeout, cancel).await {
                Ok((code, stdout, stderr)) => (code == 0, join_streams(&stdout, &stderr)),
                Err(e) => (false, e.to_string()),
            };
            outcome.gates.insert(name.as_str().to_owned(), passed);
            if !passed {
                let headline = format!("gate {:?} failed", name.as_str());
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
        step: &Step,
        ctx: &minijinja::Value,
        workdir: &Path,
        timeout: Option<Duration>,
        deps_passed: bool,
        cancel: &CancelToken,
    ) -> StepOutcome {
        if !deps_passed {
            return StepOutcome::failed("an upstream dependency did not pass").skipped();
        }
        match step.when.as_deref() {
            Some(expr) => match eval_when(expr, ctx) {
                Ok(true) => {
                    self.run_with_retry(run_id, step, ctx, workdir, timeout, cancel)
                        .await
                }
                Ok(false) => skipped_outcome(),
                Err(e) => StepOutcome::failed(format!("when: {e}")),
            },
            None => {
                self.run_with_retry(run_id, step, ctx, workdir, timeout, cancel)
                    .await
            }
        }
    }

    /// Runs a step, retrying on failure per its retry policy with backoff between attempts.
    /// Emits a `StepStarted` event per attempt.
    async fn run_with_retry(
        &self,
        run_id: RunId,
        step: &Step,
        ctx: &minijinja::Value,
        workdir: &Path,
        timeout: Option<Duration>,
        cancel: &CancelToken,
    ) -> StepOutcome {
        // u32 so a (valid) `retry.max` of 255 can't overflow the attempt counter.
        let max_attempts = 1 + u32::from(step.retry.max);
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
                RunEvent::StepStarted {
                    step: step.id.clone(),
                    attempt: u8::try_from(attempt).unwrap_or(u8::MAX),
                    at: Utc::now(),
                },
            )
            .await;
            // Per-attempt context: always exposes `retry.attempt`; on a retry with `feedback`
            // enabled, `retry.feedback` carries the prior failure so the prompt can address it.
            let attempt_ctx =
                attempt_context(ctx, step.retry.feedback, attempt, prior_error.as_deref());
            let mut outcome = self
                .exec_step(step, &attempt_ctx, workdir, timeout, cancel)
                .await;
            if outcome.status != StepStatus::Failed || attempt >= max_attempts {
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
            tokio::time::sleep(backoff_delay(step.retry.backoff, attempt)).await;
        };
        // The retry rewind point is dead once the step settles; drop its ref so the snapshot
        // commit becomes collectable (and doesn't outlive the run as a dangling anchor).
        if pre_step_snapshot.is_some() {
            self.delete_ref(workdir, &retry_ref, cancel).await;
        }
        outcome
    }

    /// Runs a shell command in `workdir`, returning `(exit_code, stdout)`.
    async fn shell(
        &self,
        command: &str,
        workdir: &Path,
        timeout: Option<Duration>,
        cancel: &CancelToken,
    ) -> Result<(i32, String, String)> {
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            timeout,
            env: Vec::new(),
            stdin: None,
        };
        let args = vec!["-c".to_owned(), command.to_owned()];
        let out = run_process("sh", &args, &opts, cancel).await?;
        Ok((out.exit_code, out.stdout, out.stderr))
    }

    /// Captures the working-tree diff in `workdir`, including agent-created files.
    ///
    /// Coding agents most often *create* files, which a plain `git diff` (tracked
    /// changes only) would miss — so we first mark everything intent-to-add
    /// (`git add -N .`), then diff. Best-effort: a non-git workdir yields `None`.
    async fn capture_diff(
        &self,
        workdir: &Path,
        base: Option<&str>,
        cancel: &CancelToken,
    ) -> Option<String> {
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let intent_to_add = ["add", "-N", "."].map(str::to_owned);
        let _ = run_process("git", &intent_to_add, &opts, cancel).await;
        // Diff against the run's base commit when known (snapshots may have advanced the
        // index without moving it), else the working tree's HEAD. `--` disambiguates the
        // revision from any path.
        let args = match base {
            Some(base) => vec!["diff".to_owned(), base.to_owned(), "--".to_owned()],
            None => vec!["diff".to_owned()],
        };
        // Gate on a clean exit: an unresolvable base (e.g. a reaped snapshot) makes
        // `git diff <base>` exit non-zero with EMPTY stdout, which must NOT be mistaken for a
        // real empty diff and clobber the carried-forward DIFF — return `None` instead.
        run_process("git", &args, &opts, cancel)
            .await
            .ok()
            .filter(|o| o.exit_code == 0)
            .map(|o| o.stdout)
    }

    /// The workspace's current `HEAD` commit, or `None` if it cannot be read (non-git dir).
    async fn git_head(&self, workdir: &Path, cancel: &CancelToken) -> Option<String> {
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let out = run_process(
            "git",
            &["rev-parse".to_owned(), "HEAD".to_owned()],
            &opts,
            cancel,
        )
        .await
        .ok()?;
        let head = out.stdout.trim();
        (out.exit_code == 0 && !head.is_empty()).then(|| head.to_owned())
    }

    /// Snapshots the current workspace tree as an off-branch commit parented on `base`,
    /// kept alive by the per-run ref `refs/odin/run/<run_id>` so [`restore_workdir`] can
    /// reach it on resume — this is the **durable** snapshot the persisted `RunState.snapshot`
    /// points at. Returns the commit SHA, or `None` on any git failure.
    async fn snapshot_workdir(
        &self,
        workdir: &Path,
        base: &str,
        run_id: RunId,
        cancel: &CancelToken,
    ) -> Option<String> {
        self.snapshot_to_ref(
            workdir,
            base,
            run_id,
            &format!("refs/odin/run/{run_id}"),
            cancel,
        )
        .await
    }

    /// Snapshots the workspace tree as an off-branch commit parented on `base`, anchored by
    /// `snapshot_ref` so [`restore_workdir`] can reach it. Returns the commit SHA, or `None` on
    /// any git failure (snapshots are best-effort — resume idempotency degrades gracefully
    /// without one). Does not touch the branch, HEAD, or the working index (it stages into a
    /// throwaway index file). The caller chooses the ref so a transient snapshot (e.g. the
    /// per-step retry rewind point) can use a *separate* ref and not disturb the durable
    /// per-run ref that resume relies on.
    async fn snapshot_to_ref(
        &self,
        workdir: &Path,
        base: &str,
        run_id: RunId,
        snapshot_ref: &str,
        cancel: &CancelToken,
    ) -> Option<String> {
        let index_path =
            std::env::temp_dir().join(format!("odin-index-{run_id}-{}", uuid::Uuid::new_v4()));
        let index_str = index_path.to_str()?.to_owned();
        // Stage into a throwaway index, and pin an explicit identity so `commit-tree`
        // succeeds even in a repo with no configured user (or `user.useConfigOnly`).
        let staged = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            env: vec![
                ("GIT_INDEX_FILE".to_owned(), index_str),
                ("GIT_AUTHOR_NAME".to_owned(), "odin".to_owned()),
                ("GIT_AUTHOR_EMAIL".to_owned(), "odin@localhost".to_owned()),
                ("GIT_COMMITTER_NAME".to_owned(), "odin".to_owned()),
                (
                    "GIT_COMMITTER_EMAIL".to_owned(),
                    "odin@localhost".to_owned(),
                ),
            ],
            ..ProcessOptions::default()
        };
        let run_git = |args: Vec<String>| {
            let opts = staged.clone();
            async move { run_process("git", &args, &opts, cancel).await.ok() }
        };
        // Seed a throwaway index from HEAD, stage every change (tracked + untracked), write
        // the tree, and commit it parented on the run's base — no branch is moved. Compute
        // the SHA, then remove the temp index on EVERY path (a `?` here returns from the
        // block, not the function, so the cleanup below always runs).
        let sha = async {
            let read = run_git(vec!["read-tree".to_owned(), "HEAD".to_owned()]).await?;
            if read.exit_code != 0 {
                return None;
            }
            let _ = run_git(vec!["add".to_owned(), "-A".to_owned()]).await;
            let tree_out = run_git(vec!["write-tree".to_owned()]).await?;
            let tree = tree_out.stdout.trim().to_owned();
            if tree_out.exit_code != 0 || tree.is_empty() {
                return None;
            }
            let commit = run_git(vec![
                "commit-tree".to_owned(),
                tree,
                "-p".to_owned(),
                base.to_owned(),
                "-m".to_owned(),
                "odin snapshot".to_owned(),
            ])
            .await?;
            let sha = commit.stdout.trim().to_owned();
            (commit.exit_code == 0 && !sha.is_empty()).then_some(sha)
        }
        .await;
        let _ = std::fs::remove_file(&index_path);
        let sha = sha?;
        // Anchor the dangling commit so it survives until the run completes.
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let _ = run_process(
            "git",
            &[
                "update-ref".to_owned(),
                snapshot_ref.to_owned(),
                sha.clone(),
            ],
            &opts,
            cancel,
        )
        .await;
        Some(sha)
    }

    /// Resets the workspace (index + worktree) to `target`, then drops leftover untracked
    /// files — so a step interrupted mid-edit re-runs from a clean, known state. HEAD is not
    /// moved; callers only restore while HEAD is still at the run's base (no commits), so the
    /// worktree and HEAD stay consistent.
    ///
    /// `git clean -fd` discards every *non-ignored* untracked file in the workspace created
    /// since `target`. That is the intended blast radius — the run's worktree is a throwaway
    /// per-run checkout, not the user's repo — but ignored files (`.gitignore`d build caches,
    /// local `.env`, etc.) are deliberately left untouched and so are NOT rewound; a step's
    /// side effects on ignored paths are outside snapshot/restore.
    ///
    /// If `read-tree` fails (e.g. the snapshot commit was reaped), the restore is abandoned
    /// WITHOUT running `clean`, leaving the workspace untouched rather than half-reset.
    ///
    /// Returns `true` iff the worktree was actually reset to `target`. A caller that advances
    /// resume state on the assumption the restore happened (the loop re-entering at a later
    /// iteration) MUST gate on this — otherwise a reaped snapshot would skip iterations whose work
    /// is no longer present, re-entering against a tree that lacks it.
    async fn restore_workdir(&self, workdir: &Path, target: &str, cancel: &CancelToken) -> bool {
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let read = ["read-tree", "-u", "--reset", target].map(str::to_owned);
        let reset_ok = run_process("git", &read, &opts, cancel)
            .await
            .is_ok_and(|o| o.exit_code == 0);
        if reset_ok {
            let _ = run_process(
                "git",
                &["clean".to_owned(), "-fd".to_owned()],
                &opts,
                cancel,
            )
            .await;
        }
        reset_ok
    }

    /// Drops the run's snapshot refs so their dangling commits become collectable: the durable
    /// per-run ref, plus any per-step retry rewind refs (`refs/odin/retry/<id>/*`) and per-loop
    /// iteration refs (`refs/odin/loop/<id>/*`) a crash or cancel left behind (the happy path drops
    /// each one when its step settles). Best effort.
    async fn delete_snapshot_ref(&self, workdir: &Path, run_id: RunId, cancel: &CancelToken) {
        self.delete_ref(workdir, &format!("refs/odin/run/{run_id}"), cancel)
            .await;
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        // List the run's transient refs (a trailing `/` matches the whole hierarchy) and drop each.
        for prefix in [
            format!("refs/odin/retry/{run_id}/"),
            format!("refs/odin/loop/{run_id}/"),
        ] {
            let list = [
                "for-each-ref".to_owned(),
                "--format=%(refname)".to_owned(),
                prefix,
            ];
            if let Ok(out) = run_process("git", &list, &opts, cancel).await {
                for refname in out.stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    self.delete_ref(workdir, refname, cancel).await;
                }
            }
        }
    }

    /// Deletes a git ref (best effort), letting any commit it anchored become collectable.
    async fn delete_ref(&self, workdir: &Path, ref_name: &str, cancel: &CancelToken) {
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let args = [
            "update-ref".to_owned(),
            "-d".to_owned(),
            ref_name.to_owned(),
        ];
        let _ = run_process("git", &args, &opts, cancel).await;
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
        let mut settled: HashSet<StepId> = HashSet::new();
        for (id, st) in &state.steps {
            if matches!(st.status, StepStatus::Passed | StepStatus::Skipped) {
                settled.insert(id.clone());
                results.insert(id.clone(), step_result(id, st));
                // Carry forward side effects already recorded by finished steps; they won't
                // re-run, so without this a resumed run's summary would drop every PR/commit/
                // push from before the crash.
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
            // Fill the ready-set up to the concurrency limit, honoring the exclusivity rule.
            while in_flight.len() < max_parallel && !exclusive_running && suspended_gate.is_none() {
                let Some(step) = order
                    .iter()
                    .filter_map(|id| by_id.get(id.as_str()).copied())
                    .find(|s| {
                        !started.contains(&s.id) && s.depends_on.iter().all(|d| settled.contains(d))
                    })
                else {
                    break;
                };
                let exclusive = !step.scratch;
                if exclusive && !in_flight.is_empty() {
                    break; // an exclusive step must wait until the workdir is idle
                }

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

                let timeout = step
                    .timeout
                    .or(workflow.defaults.timeout)
                    .map(crate::ir::HumanDuration::as_duration);
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
                    step,
                    ctx,
                    workdir,
                    timeout,
                    deps_passed,
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
            self.emit_step_events(run_id, &id, &outcome).await;
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

    /// Runs one step, provisioning an isolated scratch worktree first if `step.scratch`. A
    /// scratch step's file edits stay in its throwaway worktree; its diff is surfaced as
    /// `outputs.diff` and the worktree is removed. Returns `(id, outcome)` for the driver.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(name = "step", skip_all, fields(step = %step.id, scratch = step.scratch))]
    async fn run_one(
        &self,
        run_id: RunId,
        step: &Step,
        ctx: minijinja::Value,
        base_workdir: &Path,
        timeout: Option<Duration>,
        deps_passed: bool,
        cancel: &CancelToken,
    ) -> (StepId, StepOutcome) {
        let id = step.id.clone();
        // A non-scratch step, or one whose deps did not pass (it will be skipped without
        // touching any workdir), runs against the shared workdir — no worktree needed.
        if !step.scratch || !deps_passed {
            let outcome = self
                .decide_outcome(
                    run_id,
                    step,
                    &ctx,
                    base_workdir,
                    timeout,
                    deps_passed,
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
                    .decide_outcome(run_id, step, &ctx, &scratch, timeout, deps_passed, cancel)
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
    async fn run_loop(
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
                let timeout = inner
                    .timeout
                    .or(workflow.defaults.timeout)
                    .map(crate::ir::HumanDuration::as_duration);
                let (_, o) = self
                    .run_one(run_id, inner, ctx, workdir, timeout, deps_passed, cancel)
                    .await;
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

    /// Adds a detached git worktree at the run's base commit (`HEAD` of `base_workdir`) as a
    /// throwaway scratch dir, outside the run workdir so it never pollutes the shared DIFF.
    /// Named by `run_id` so [`cleanup_scratch`](Self::cleanup_scratch) can reclaim leftovers.
    async fn acquire_scratch(
        &self,
        run_id: RunId,
        base_workdir: &Path,
        step_id: &StepId,
        cancel: &CancelToken,
    ) -> std::result::Result<PathBuf, String> {
        let scratch = std::env::temp_dir().join(format!(
            "odin-scratch-{run_id}-{}-{}",
            step_id.as_str(),
            uuid::Uuid::new_v4()
        ));
        let scratch_str = scratch
            .to_str()
            .ok_or_else(|| "scratch path is not valid UTF-8".to_owned())?;
        let opts = ProcessOptions {
            workdir: Some(base_workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let args = ["worktree", "add", "--detach", scratch_str, "HEAD"].map(str::to_owned);
        // Serialized: concurrent scratch steps must not race on git's worktree metadata.
        let out = {
            let _guard = self.worktree_lock.lock().await;
            run_process("git", &args, &opts, cancel).await
        }
        .map_err(|e| e.to_string())?;
        if out.exit_code == 0 {
            Ok(scratch)
        } else {
            Err(format!("git worktree add failed: {}", out.stderr.trim()))
        }
    }

    /// Removes a scratch worktree (best effort — failure only leaks a temp dir).
    async fn release_scratch(&self, base_workdir: &Path, scratch: &Path, cancel: &CancelToken) {
        let Some(scratch_str) = scratch.to_str() else {
            return;
        };
        let opts = ProcessOptions {
            workdir: Some(base_workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let args = ["worktree", "remove", "--force", scratch_str].map(str::to_owned);
        let _guard = self.worktree_lock.lock().await;
        let _ = run_process("git", &args, &opts, cancel).await;
    }

    /// Reclaims scratch worktrees left over from a previous attempt of this run (a crash or
    /// kill mid-scratch-step leaks the temp dir). Called once at the start of a run that has
    /// scratch steps, so resumes don't accumulate orphaned worktrees.
    async fn cleanup_scratch(&self, run_id: RunId, base_workdir: &Path, cancel: &CancelToken) {
        let opts = ProcessOptions {
            workdir: Some(base_workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let prefix = format!("odin-scratch-{run_id}-");
        // Serialize against acquire/release_scratch: `git worktree remove` and especially
        // `git worktree prune` (a *global* operation on `.git/worktrees/`) race with a
        // concurrent run's `git worktree add`, corrupting git's worktree metadata. Hold the
        // same lock those paths take for the whole cleanup (it is rare — once per resuming
        // run that has scratch steps). The filesystem reads/removes are cheap to keep inside.
        let _guard = self.worktree_lock.lock().await;
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().starts_with(&prefix) {
                    continue;
                }
                if let Some(p) = entry.path().to_str() {
                    let args = ["worktree", "remove", "--force", p].map(str::to_owned);
                    let _ = run_process("git", &args, &opts, cancel).await;
                }
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        // Drop any now-dangling worktree registrations from the repo metadata.
        let _ = run_process(
            "git",
            &["worktree".to_owned(), "prune".to_owned()],
            &opts,
            cancel,
        )
        .await;
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
        Ok(RunSummary {
            run_id: state.run_id,
            workflow: state.workflow.clone(),
            status: RunStatus::Failed,
            steps: state
                .steps
                .iter()
                .map(|(id, st)| step_result(id, st))
                .collect(),
            usage: total_usage(&state.steps),
            side_effects: collect_side_effects(&state.steps),
            diff: state
                .artifacts
                .get(&crate::ids::ArtifactName::new(DIFF))
                .cloned(),
            error: Some(error.to_owned()),
            started_at: state.created_at,
            finished_at: Some(Utc::now()),
        })
    }
}

#[async_trait]
impl Engine for LocalEngine {
    #[allow(clippy::too_many_lines)]
    async fn run(&self, workflow: &Workflow, input: RunInput) -> Result<RunSummary> {
        let report = crate::validate::validate(workflow, &self.registry.known_names());
        if report.has_errors() {
            return Err(Error::Validation(report));
        }
        let params = Self::resolve_params(workflow, &input)?;

        let run_id = RunId::new();
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
        self.emit(run_id, RunEvent::RunStarted { at: started_at })
            .await;

        let run_span = tracing::info_span!(
            "run",
            run_id = %run_id,
            workflow = %workflow.name,
            durable = workflow.durable,
        );
        tracing::info!(parent: &run_span, "run started");
        let cancel = CancelToken::new();
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
        let (status, summary) = Self::summarize(run_id, workflow, exec, error, started_at);
        state.status = status;
        state.error = summary.error.clone();
        state.updated_at = Utc::now();
        self.checkpoint(workflow.durable, &state).await?;
        self.emit(
            run_id,
            RunEvent::RunFinished {
                status,
                at: Utc::now(),
            },
        )
        .await;
        let elapsed_ms = (Utc::now() - started_at).num_milliseconds();
        tracing::info!(
            parent: &run_span,
            run_id = %run_id,
            status = ?status,
            steps = summary.steps.len(),
            cost_micros = summary.usage.cost_micros,
            elapsed_ms,
            "run finished"
        );

        Ok(summary)
    }

    async fn resume_all(&self, workflows: &[Workflow]) -> Result<Vec<RunSummary>> {
        let Some(store) = self.store.clone() else {
            return Ok(Vec::new());
        };
        let by_name: HashMap<&str, &Workflow> =
            workflows.iter().map(|w| (w.name.as_str(), w)).collect();

        let mut summaries = Vec::new();
        let sweep_cancel = CancelToken::new();
        for state in store.load_incomplete().await? {
            let Some(workflow) = by_name.get(state.workflow.as_str()).copied() else {
                // The run targets a workflow we no longer serve — best-effort reclaim its
                // snapshot ref from the shared main repo (a no-op if none was created), so
                // dangling commits aren't pinned forever even if the worktree is gone.
                self.delete_snapshot_ref(&self.repo_root, state.run_id, &sweep_cancel)
                    .await;
                continue;
            };
            // Skip a run already being resumed (e.g. by a concurrent approval decision); never
            // execute the same run twice. The claim is held for this run's whole resume.
            let Some(_claim) = self.claim_run(state.run_id) else {
                continue;
            };
            if let Some(summary) = self.resume_state(workflow, state).await? {
                summaries.push(summary);
            }
        }
        Ok(summaries)
    }

    async fn summary(&self, run_id: RunId) -> Result<Option<RunSummary>> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let Some(state) = store.load_run(run_id).await? else {
            return Ok(None);
        };
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
        Ok(Some(RunSummary {
            run_id: state.run_id,
            workflow: state.workflow,
            status: state.status,
            steps,
            usage,
            side_effects: collect_side_effects(&state.steps),
            diff,
            error: state.error,
            started_at: state.created_at,
            finished_at: Some(state.updated_at),
        }))
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
        state.approvals.insert(
            gate,
            ApprovalDecision {
                decision,
                approver,
                at: Utc::now(),
                note,
            },
        );
        // Flip to Running, then resume THIS run (not the all-runs sweep): resuming only the
        // decided run keeps the claim meaningful and avoids disturbing other in-flight runs.
        state.status = RunStatus::Running;
        state.updated_at = Utc::now();
        store.checkpoint(&state).await?;
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
        // Reclaim each pruned run's leftover snapshot refs from the shared repo (best-effort,
        // never fails the prune). Terminal runs normally drop these on completion; this sweeps
        // any historical leftover so deleting the DB row doesn't orphan a dangling-commit ref.
        if !dry_run && !report.run_ids.is_empty() {
            let cancel = CancelToken::new();
            for run_id in &report.run_ids {
                self.delete_snapshot_ref(&self.repo_root, *run_id, &cancel)
                    .await;
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
    /// Resumes a single already-loaded run to its next stopping point (terminal, or paused
    /// again at a gate). The caller MUST hold the run's [`claim_run`] guard for the duration —
    /// two concurrent resumes of one run would double-run its side effects. Returns `Ok(None)`
    /// if the run can't be resumed because its workspace handle is absent.
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
                    let run_span = tracing::info_span!(
                        "run",
                        run_id = %state.run_id,
                        workflow = %workflow.name,
                        durable = workflow.durable,
                        resumed = true,
                    );
                    tracing::info!(parent: &run_span, "resuming run");
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
                                    let (status, summary) = Self::summarize(
                                        state.run_id,
                                        workflow,
                                        result,
                                        None,
                                        started_at,
                                    );
                                    state.status = status;
                                    state.error = summary.error.clone();
                                    state.updated_at = Utc::now();
                                    self.checkpoint(workflow.durable, &state).await?;
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

fn skipped_outcome() -> StepOutcome {
    StepOutcome {
        status: StepStatus::Skipped,
        exit_code: None,
        outputs: IndexMap::new(),
        usage: None,
        gates: IndexMap::new(),
        side_effects: Vec::new(),
        error: None,
        failure_detail: None,
        stderr: String::new(),
        attempts: 1,
        judge_score: None,
    }
}

/// Sums the persisted per-step usage across a run's steps.
fn total_usage(steps: &IndexMap<StepId, StepState>) -> Usage {
    let mut usage = Usage::default();
    for step in steps.values() {
        if let Some(u) = step.usage {
            usage.add(u);
        }
    }
    usage
}

/// All side effects recorded across a run's steps, in step (execution) order. Used to
/// reconstruct a `RunSummary` from persisted state on every read path (resume, post-hoc
/// `summary(run_id)`, and the failure paths) so none silently drops a PR/commit/push.
fn collect_side_effects(steps: &IndexMap<StepId, StepState>) -> Vec<SideEffect> {
    steps
        .values()
        .flat_map(|st| st.side_effects.iter().cloned())
        .collect()
}

fn step_result(id: &StepId, state: &StepState) -> StepResult {
    StepResult {
        id: id.clone(),
        status: state.status,
        attempts: state.attempts,
        exit_code: state.exit_code,
        outputs: state.outputs.clone(),
        gates: state.gates.clone(),
        judge_score: state.judge_score,
        usage: state.usage,
        error: state.error.clone(),
    }
}

/// Appends a truncated tail of `stderr` to a failure `message`, so a failed step records the
/// actual cause (compiler errors, an auth failure, a stack trace) rather than just an exit
/// code. Keeps the *end* of stderr (where the real error usually is), on a char boundary.
fn with_stderr_tail(message: &str, stderr: &str) -> String {
    const MAX: usize = 2000;
    let stderr = stderr.trim();
    if stderr.is_empty() {
        return message.to_owned();
    }
    if stderr.len() <= MAX {
        return format!("{message}\nstderr:\n{stderr}");
    }
    let mut start = stderr.len() - MAX;
    while start < stderr.len() && !stderr.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "{message}\nstderr (last {MAX} bytes):\n…{}",
        &stderr[start..]
    )
}

/// Joins a command's `stdout` and `stderr` into one diagnostic blob (stderr first, since it
/// usually carries the error summary), trimming each and dropping an empty stream. Used for gate
/// failures, where the actionable detail may land on either stream depending on the tool.
fn join_streams(stdout: &str, stderr: &str) -> String {
    match (stderr.trim(), stdout.trim()) {
        ("", out) => out.to_owned(),
        (err, "") => err.to_owned(),
        (err, out) => format!("{err}\n{out}"),
    }
}

/// Upper bound on the bytes of failure context fed into `retry.feedback`. Each retry re-renders a
/// (paid) provider prompt, so the feedback is capped regardless of which failure path produced it.
const FEEDBACK_MAX: usize = 2000;

/// The last `max` bytes of `s` on a char boundary, prefixed with `…` when truncated.
fn clip_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

/// The raw diagnostic `retry.feedback` surfaces: the un-wrapped `detail` (tail-capped to the most
/// recent bytes, with no synthetic headline so its *first* line is real content), or the `headline`
/// alone when there is no detail. Distinct from [`with_stderr_tail`], which keeps the headline for a
/// human/log summary.
fn failure_detail(headline: &str, detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        return headline.to_owned();
    }
    clip_tail(detail, FEEDBACK_MAX)
}

/// Base inter-attempt retry delay.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

/// The delay before re-attempting after `completed_attempt` failed.
fn backoff_delay(backoff: Backoff, completed_attempt: u32) -> Duration {
    match backoff {
        Backoff::Fixed => RETRY_BASE_DELAY,
        Backoff::Exponential => {
            RETRY_BASE_DELAY * 2u32.pow(completed_attempt.saturating_sub(1).min(6))
        }
    }
}

/// Extracts a `0.0..=1.0` score from judge output: a JSON object with a `score` field,
/// even when the model wraps it in prose or other braces.
fn parse_score(text: &str) -> Option<f32> {
    // Fast path: the whole output is the JSON object.
    if let Some(score) = score_from_json(text.trim()) {
        return Some(score);
    }
    // Otherwise scan each `{` for the balanced object it opens and try that.
    for (start, _) in text.match_indices('{') {
        let mut depth = 0usize;
        for (offset, ch) in text[start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Some(slice) = text.get(start..=start + offset) {
                            if let Some(score) = score_from_json(slice) {
                                return Some(score);
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Parses one JSON document and pulls a `score` out of it (anywhere in the tree).
fn score_from_json(s: &str) -> Option<f32> {
    let value: Value = serde_json::from_str(s).ok()?;
    extract_score(&value)
}

/// Finds a `score` value (number or numeric string) anywhere in a JSON value.
#[allow(clippy::cast_possible_truncation)]
fn extract_score(value: &Value) -> Option<f32> {
    if let Some(score) = value.get("score") {
        let n = score
            .as_f64()
            .or_else(|| score.as_str().and_then(|s| s.trim().parse::<f64>().ok()));
        if let Some(n) = n {
            return Some((n as f32).clamp(0.0, 1.0));
        }
    }
    value.as_object()?.values().find_map(extract_score)
}

/// Builds the minijinja context from the run state assembled so far, with the default `loop` root
/// (`loop.counter` = 1, empty `loop.feedback`) — the pre-iteration state seen by a top-level step.
fn build_ctx(
    params: &IndexMap<String, Value>,
    trigger_payload: &Value,
    steps: &IndexMap<StepId, StepState>,
    diff: Option<&str>,
    state: &RunState,
    workflow: &Workflow,
) -> minijinja::Value {
    build_ctx_with(params, trigger_payload, steps, diff, state, workflow, 1, "")
}

/// Builds the context with explicit `loop.counter` / `loop.feedback` — used inside a `loop:` body
/// so an inner step (and the `until` guard) sees the current iteration. (`loop` is a keyword, so
/// the root is baked into the JSON here rather than overlaid via `context!` like `retry`.)
#[allow(clippy::too_many_arguments)]
fn build_ctx_with(
    params: &IndexMap<String, Value>,
    trigger_payload: &Value,
    steps: &IndexMap<StepId, StepState>,
    diff: Option<&str>,
    state: &RunState,
    workflow: &Workflow,
    loop_counter: u32,
    loop_feedback: &str,
) -> minijinja::Value {
    let mut steps_obj = serde_json::Map::new();
    for (id, st) in steps {
        steps_obj.insert(
            id.as_str().to_owned(),
            json!({
                "outputs": st.outputs,
                "exit_code": st.exit_code,
                "status": st.status,
            }),
        );
    }
    let mut artifacts = serde_json::Map::new();
    if let Some(d) = diff {
        artifacts.insert(DIFF.to_owned(), Value::String(d.to_owned()));
    }
    let root = json!({
        "params": params,
        "trigger": trigger_payload,
        "steps": steps_obj,
        "artifacts": artifacts,
        "run": { "id": state.run_id.to_string(), "workflow": workflow.name.as_str() },
        // A default `retry` root so `retry.*` resolves in EVERY template position — including a
        // step-level `when:` guard, evaluated once before any attempt. `attempt_context` overrides
        // this per attempt for the step body; here it reflects the pre-attempt state.
        "retry": { "attempt": 1, "feedback": "" },
        // `loop.*` resolves everywhere too (default counter 1, empty feedback); a `loop:` body
        // rebuilds the context with the live iteration values.
        "loop": { "counter": loop_counter, "feedback": loop_feedback },
    });
    build_context(&root)
}

/// Builds a step's per-attempt template context: the base context with its `retry` root *replaced*
/// by one carrying the 1-based `attempt` and — when `feedback` is enabled and a prior attempt
/// failed — that failure's un-wrapped diagnostic as `feedback` (empty otherwise). The explicit
/// `retry` overrides the default seeded by `build_ctx` (verified: spread keys lose to explicit).
/// `retry.attempt` is always present so a prompt can branch on `{% if retry.attempt > 1 %}`.
fn attempt_context(
    base: &minijinja::Value,
    feedback: FeedbackMode,
    attempt: u32,
    prior_error: Option<&str>,
) -> minijinja::Value {
    let fb = match feedback {
        FeedbackMode::Off => String::new(),
        // First *non-blank* line of the un-wrapped failure — a brief signal. (For multi-line tool
        // output the headline may not be the whole story; `verbose` is the mode for that.)
        FeedbackMode::Concise => prior_error
            .and_then(|e| e.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or_default()
            .to_owned(),
        FeedbackMode::Verbose => prior_error.unwrap_or_default().to_owned(),
    };
    // Bound the feedback regardless of mode/source: gate/exit detail is already ≤ FEEDBACK_MAX, but
    // a provider/action error string (the fallback source) or a pathological single line is not.
    let fb = clip_tail(&fb, FEEDBACK_MAX);
    minijinja::context! {
        retry => minijinja::context! { attempt => attempt, feedback => fb },
        ..base.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineBuilder};
    use crate::mock::EchoProvider;
    use crate::storage::SqliteStore;
    use crate::workspace::testutil::init_repo;

    fn parse(yaml: &str) -> Workflow {
        Workflow::from_yaml_str(yaml).unwrap()
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
            input: RunInput::manual().param("logfile", logfile.to_str().unwrap()),
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
        assert_eq!(super::parse_score(r#"{"score": 0.8}"#), Some(0.8));
        assert_eq!(
            super::parse_score("noise before {\"score\": 1.5} after"),
            Some(1.0)
        );
        assert_eq!(super::parse_score("no json here"), None);
        // Verdict wrapped in brace-bearing prose (common for real LLM judges).
        assert_eq!(
            super::parse_score("Looking at {correctness}. Final: {\"score\": 0.85}"),
            Some(0.85)
        );
        // Score as a string, or under a nested key.
        assert_eq!(super::parse_score(r#"{"score": "0.9"}"#), Some(0.9));
        assert_eq!(
            super::parse_score(r#"{"result": {"score": 0.7}}"#),
            Some(0.7)
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
        let wf = parse(&format!(
            "name: r\nworkspace: {{ type: worktree }}\nsteps:\n  - id: flaky\n    run: \"test -f {m} || (touch {m}; exit 1)\"\n    retry: {{ max: 1 }}\n",
            m = marker.display()
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
        assert!(
            elapsed < std::time::Duration::from_millis(1300),
            "three 0.5s scratch steps took {elapsed:?}; expected concurrency (~0.5s, not ~1.5s)"
        );
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
        let eng = LocalEngine::new(Registry::with_builtins(), None, repo.path().to_path_buf());
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
