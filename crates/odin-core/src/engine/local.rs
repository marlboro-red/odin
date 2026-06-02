//! The built-in linear executor.
//!
//! [`LocalEngine`] walks a workflow's steps in topological order. For each step it builds
//! the template context from the run state so far, evaluates `when`, renders the prompt /
//! command / args, dispatches to the pinned provider, a shell `run:` hook, or a
//! registered action, runs the gates, auto-captures the git `DIFF`, and checkpoints the
//! [`RunState`] — so a crashed run can be resumed from the last completed step.
//!
//! v1 is linear (one shared workspace, single attempt, no judge); these are additive
//! refinements on top of this structure.
//!
//! ## Resume semantics
//!
//! Each step is checkpointed `Running` before dispatch and to its terminal status after,
//! so a step interrupted mid-flight by a crash is distinguishable from one not yet
//! started. On resume, only `Passed`/`Skipped` steps are skipped; a `Running` (or absent)
//! step is **re-executed from scratch**. Because all steps share one workdir, that
//! re-execution is **not guaranteed idempotent** in v1 — a step whose side effects
//! (file edits, a created branch) partially applied before the crash may double-apply.
//! Per-step commit snapshots are the planned fix.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use indexmap::IndexMap;
use serde_json::{Value, json};

use super::Engine;
use crate::api::{RunInput, RunStatus, RunSummary, SideEffect, StepResult, StepStatus};
use crate::context::render::{build_context, eval_when, render_template};
use crate::error::{Error, Result};
use crate::ids::{RunId, StepId};
use crate::ir::{Step, StepKind, Workflow, WorkspaceConfig};
use crate::provider::process::{ProcessOptions, run_process};
use crate::registry::Registry;
use crate::traits::{
    AcquireCtx, ActionCtx, CancelToken, InvocationCtx, RunEvent, RunState, StepState, Store,
    Workspace,
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
        }
    }
}

/// The summary-shaped result of an execution pass.
struct ExecResult {
    steps: Vec<StepResult>,
    side_effects: Vec<SideEffect>,
    usage: Usage,
    diff: Option<String>,
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
        }
    }

    fn make_workspace(&self, cfg: &WorkspaceConfig) -> Arc<dyn Workspace> {
        match cfg {
            WorkspaceConfig::Worktree(_) => {
                Arc::new(WorktreeWorkspace::new(self.repo_root.clone()))
            }
            WorkspaceConfig::SlotPool(c) => Arc::new(SlotPoolWorkspace::new(
                self.repo_root.clone(),
                self.repo_root.join(".odin").join("slots"),
                c.pool as usize,
                c.reset,
            )),
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

    /// Records a step `Running` and checkpoints it — the durability boundary written
    /// before dispatch, so a step interrupted mid-flight is distinguishable from one not
    /// yet started.
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
            },
        );
        state.updated_at = Utc::now();
        self.checkpoint(workflow.durable, state).await?;
        self.emit(
            state.run_id,
            RunEvent::StepStarted {
                step: id.clone(),
                attempt: 1,
                at: Utc::now(),
            },
        )
        .await;
        Ok(())
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
                        StepOutcome::passing(o.exit_code, outputs, o.usage)
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
                    Ok((code, stdout)) => {
                        let mut outputs = IndexMap::new();
                        outputs.insert("stdout".to_owned(), Value::String(stdout));
                        StepOutcome::passing(code, outputs, None)
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
            outcome.status = StepStatus::Failed;
            outcome.error = Some(format!(
                "exited with code {}",
                outcome.exit_code.unwrap_or(-1)
            ));
            return outcome;
        }

        // Gates: every named command must exit 0.
        for (name, command) in &step.gates {
            let cmd = match render_template(command, ctx, "gate") {
                Ok(s) => s,
                Err(e) => {
                    outcome.status = StepStatus::Failed;
                    outcome.error = Some(e.to_string());
                    return outcome;
                }
            };
            let passed = matches!(self.shell(&cmd, workdir, timeout, cancel).await, Ok((0, _)));
            outcome.gates.insert(name.as_str().to_owned(), passed);
            if !passed {
                outcome.status = StepStatus::Failed;
                outcome.error = Some(format!("gate {:?} failed", name.as_str()));
            }
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
    ) -> Result<(i32, String)> {
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            timeout,
            env: Vec::new(),
            stdin: None,
        };
        let args = vec!["-c".to_owned(), command.to_owned()];
        let out = run_process("sh", &args, &opts, cancel).await?;
        Ok((out.exit_code, out.stdout))
    }

    /// Captures the working-tree diff in `workdir`, including agent-created files.
    ///
    /// Coding agents most often *create* files, which a plain `git diff` (tracked
    /// changes only) would miss — so we first mark everything intent-to-add
    /// (`git add -N .`), then diff. Best-effort: a non-git workdir yields `None`.
    async fn capture_diff(&self, workdir: &Path, cancel: &CancelToken) -> Option<String> {
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let intent_to_add = ["add", "-N", "."].map(str::to_owned);
        let _ = run_process("git", &intent_to_add, &opts, cancel).await;
        run_process("git", &["diff".to_owned()], &opts, cancel)
            .await
            .ok()
            .map(|o| o.stdout)
    }

    /// Walks the DAG, executing or skipping each step and checkpointing as it goes.
    async fn execute(
        &self,
        workflow: &Workflow,
        state: &mut RunState,
        workdir: &Path,
        params: &IndexMap<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ExecResult> {
        let order = crate::validate::graph::topo_order(workflow)
            .unwrap_or_else(|_| workflow.steps.iter().map(|s| s.id.clone()).collect());
        let by_id: HashMap<&str, &Step> =
            workflow.steps.iter().map(|s| (s.id.as_str(), s)).collect();

        let mut summary = Vec::new();
        let mut side_effects = Vec::new();
        let mut usage = Usage::default();
        // Seed from the persisted DIFF so a resumed run carries it forward (already-passed
        // steps cannot re-capture it); otherwise a downstream `{{ artifacts.DIFF }}` would
        // be undefined after a crash.
        let mut diff: Option<String> = state
            .artifacts
            .get(&crate::ids::ArtifactName::new(DIFF))
            .cloned();

        for id in &order {
            let Some(step) = by_id.get(id.as_str()).copied() else {
                continue;
            };

            // Already done (resume): keep its state, surface it in the summary.
            if let Some(existing) = state.steps.get(id) {
                if matches!(existing.status, StepStatus::Passed | StepStatus::Skipped) {
                    summary.push(step_result(id, existing, IndexMap::new()));
                    continue;
                }
            }

            let timeout = step
                .timeout
                .or(workflow.defaults.timeout)
                .map(crate::ir::HumanDuration::as_duration);

            let deps_passed = step.depends_on.iter().all(|d| {
                matches!(
                    state.steps.get(d).map(|s| s.status),
                    Some(StepStatus::Passed)
                )
            });
            let ctx = build_ctx(
                params,
                &state.input.trigger_payload,
                &state.steps,
                diff.as_deref(),
                state,
                workflow,
            );

            // Durability boundary before acting (see `mark_running`).
            self.mark_running(workflow, state, id).await?;

            let outcome = if deps_passed {
                match step.when.as_deref() {
                    Some(expr) => match eval_when(expr, &ctx) {
                        Ok(true) => self.exec_step(step, &ctx, workdir, timeout, cancel).await,
                        Ok(false) => skipped_outcome(),
                        Err(e) => StepOutcome::failed(format!("when: {e}")),
                    },
                    None => self.exec_step(step, &ctx, workdir, timeout, cancel).await,
                }
            } else {
                StepOutcome::failed("an upstream dependency did not pass").skipped()
            };

            if let Some(u) = &outcome.usage {
                usage.add(*u);
            }
            side_effects.extend(outcome.side_effects.iter().cloned());

            // Persist step state, then refresh DIFF for subsequent steps.
            let step_state = StepState {
                status: outcome.status,
                attempts: 1,
                exit_code: outcome.exit_code,
                outputs: outcome.outputs.clone(),
                usage: outcome.usage,
            };
            state.steps.insert(id.clone(), step_state.clone());
            if matches!(outcome.status, StepStatus::Passed) {
                diff = self.capture_diff(workdir, cancel).await;
                if let Some(d) = &diff {
                    state.artifacts.insert(DIFF.into(), d.clone());
                }
            }
            state.updated_at = Utc::now();
            self.checkpoint(workflow.durable, state).await?;
            self.emit(
                state.run_id,
                RunEvent::StepFinished {
                    step: id.clone(),
                    status: outcome.status,
                    exit_code: outcome.exit_code,
                    at: Utc::now(),
                },
            )
            .await;

            summary.push(step_result(id, &step_state, outcome.gates));
        }

        Ok(ExecResult {
            steps: summary,
            side_effects,
            usage,
            diff,
        })
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
                .map(|(id, st)| step_result(id, st, IndexMap::new()))
                .collect(),
            usage: Usage::default(),
            side_effects: Vec::new(),
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
    async fn run(&self, workflow: &Workflow, input: RunInput) -> Result<RunSummary> {
        let report = crate::validate::validate(workflow, &self.registry.known_names());
        if report.has_errors() {
            return Err(Error::Validation(report));
        }
        let params = Self::resolve_params(workflow, &input)?;

        let run_id = RunId::new();
        let started_at = Utc::now();
        let workspace = self.make_workspace(&workflow.workspace);
        let handle = workspace
            .acquire(AcquireCtx {
                run_id,
                config: workflow.workspace.clone(),
            })
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
            input,
            workspace: Some(handle.clone()),
            created_at: started_at,
            updated_at: started_at,
        };
        if let Err(e) = self.checkpoint(workflow.durable, &state).await {
            let _ = workspace.release(handle).await;
            return Err(e);
        }
        self.emit(run_id, RunEvent::RunStarted { at: started_at })
            .await;

        let cancel = CancelToken::new();
        let exec = self
            .execute(workflow, &mut state, &workdir, &params, &cancel)
            .await;
        let _ = workspace.release(handle).await;

        let (exec, error) = match exec {
            Ok(r) => (r, None),
            Err(e) => (
                ExecResult {
                    steps: Vec::new(),
                    side_effects: Vec::new(),
                    usage: Usage::default(),
                    diff: None,
                },
                Some(e.to_string()),
            ),
        };
        let error = error.or_else(|| {
            exec.steps
                .iter()
                .find(|s| matches!(s.status, StepStatus::Failed))
                .map(|s| format!("step {:?} failed", s.id.as_str()))
        });

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

        Ok(summary)
    }

    async fn resume_all(&self, workflows: &[Workflow]) -> Result<Vec<RunSummary>> {
        let Some(store) = self.store.clone() else {
            return Ok(Vec::new());
        };
        let by_name: HashMap<&str, &Workflow> =
            workflows.iter().map(|w| (w.name.as_str(), w)).collect();

        let mut summaries = Vec::new();
        for mut state in store.load_incomplete().await? {
            let Some(workflow) = by_name.get(state.workflow.as_str()).copied() else {
                continue;
            };
            let Some(handle) = state.workspace.clone() else {
                continue;
            };
            let started_at = state.created_at;

            // Crash recovery is per-run, never all-or-nothing: one run's failure must not
            // abort the others or leave it stuck Running forever.
            let summary = if handle.path.exists() {
                match Self::resolve_params(workflow, &state.input) {
                    Ok(params) => {
                        let cancel = CancelToken::new();
                        let exec = self
                            .execute(workflow, &mut state, &handle.path.clone(), &params, &cancel)
                            .await;
                        let _ = self
                            .make_workspace(&workflow.workspace)
                            .release(handle)
                            .await;
                        match exec {
                            Ok(result) => {
                                let error = result
                                    .steps
                                    .iter()
                                    .find(|s| matches!(s.status, StepStatus::Failed))
                                    .map(|s| format!("step {:?} failed", s.id.as_str()));
                                let (status, summary) = Self::summarize(
                                    state.run_id,
                                    workflow,
                                    result,
                                    error,
                                    started_at,
                                );
                                state.status = status;
                                state.error = summary.error.clone();
                                state.updated_at = Utc::now();
                                self.checkpoint(workflow.durable, &state).await?;
                                summary
                            }
                            Err(e) => self.fail_run(workflow, &mut state, &e.to_string()).await?,
                        }
                    }
                    Err(e) => self.fail_run(workflow, &mut state, &e.to_string()).await?,
                }
            } else {
                // The workspace is gone (host moved, manual cleanup); cannot resume.
                self.fail_run(workflow, &mut state, "workspace is gone; cannot resume")
                    .await?
            };
            summaries.push(summary);
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
            .map(|(id, st)| step_result(id, st, IndexMap::new()))
            .collect();
        let diff = state
            .artifacts
            .get(&crate::ids::ArtifactName::new(DIFF))
            .cloned();
        Ok(Some(RunSummary {
            run_id: state.run_id,
            workflow: state.workflow,
            status: state.status,
            steps,
            usage: Usage::default(),
            side_effects: Vec::new(),
            diff,
            error: state.error,
            started_at: state.created_at,
            finished_at: Some(state.updated_at),
        }))
    }
}

impl StepOutcome {
    /// Converts an outcome into a skipped one, preserving the explanatory error as info.
    fn skipped(mut self) -> Self {
        self.status = StepStatus::Skipped;
        self
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
    }
}

fn step_result(id: &StepId, state: &StepState, gates: IndexMap<String, bool>) -> StepResult {
    StepResult {
        id: id.clone(),
        status: state.status,
        attempts: state.attempts,
        exit_code: state.exit_code,
        outputs: state.outputs.clone(),
        gates,
        judge_score: None,
        usage: state.usage,
    }
}

/// Builds the minijinja context from the run state assembled so far.
fn build_ctx(
    params: &IndexMap<String, Value>,
    trigger_payload: &Value,
    steps: &IndexMap<StepId, StepState>,
    diff: Option<&str>,
    state: &RunState,
    workflow: &Workflow,
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
    });
    build_context(&root)
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
            input: RunInput::manual().param("who", "resumed"),
            workspace: Some(handle),
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
            input: RunInput::manual().param("who", "resumed"),
            workspace: Some(handle),
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
            input: RunInput::manual().param("who", "x"),
            workspace: Some(crate::traits::WorkspaceHandle {
                run_id,
                path: repo.path().join(".odin/worktrees/does-not-exist"),
                branch: None,
                token: "x".to_owned(),
            }),
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
}
