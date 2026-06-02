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

use super::Engine;
use crate::api::{RunInput, RunStatus, RunSummary, SideEffect, StepResult, StepStatus};
use crate::context::render::{build_context, eval_when, render_template};
use crate::error::{Error, Result};
use crate::ids::{RunId, StepId};
use crate::ir::{Backoff, JudgeSpec, Step, StepKind, Workflow, WorkspaceConfig};
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
    /// Serializes `git worktree add/remove/prune`: git's worktree metadata is not safe for
    /// concurrent mutation, and scratch steps provision worktrees in parallel.
    worktree_lock: tokio::sync::Mutex<()>,
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
            attempts: 1,
            judge_score: None,
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
            worktree_lock: tokio::sync::Mutex::new(()),
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
            },
        );
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
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            self.emit(
                run_id,
                RunEvent::StepStarted {
                    step: step.id.clone(),
                    attempt: u8::try_from(attempt).unwrap_or(u8::MAX),
                    at: Utc::now(),
                },
            )
            .await;
            let mut outcome = self.exec_step(step, ctx, workdir, timeout, cancel).await;
            if outcome.status != StepStatus::Failed || attempt >= max_attempts {
                outcome.attempts = u8::try_from(attempt).unwrap_or(u8::MAX);
                return outcome;
            }
            tokio::time::sleep(backoff_delay(step.retry.backoff, attempt)).await;
        }
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
    /// reach it on resume. Returns the commit SHA, or `None` on any git failure (snapshots
    /// are best-effort — resume idempotency degrades gracefully without one). Does not touch
    /// the branch, HEAD, or the working index (it stages into a throwaway index file).
    async fn snapshot_workdir(
        &self,
        workdir: &Path,
        base: &str,
        run_id: RunId,
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
                format!("refs/odin/run/{run_id}"),
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
    async fn restore_workdir(&self, workdir: &Path, target: &str, cancel: &CancelToken) {
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
    }

    /// Drops the per-run snapshot ref so its dangling commits become collectable. Best effort.
    async fn delete_snapshot_ref(&self, workdir: &Path, run_id: RunId, cancel: &CancelToken) {
        let opts = ProcessOptions {
            workdir: Some(workdir.to_path_buf()),
            ..ProcessOptions::default()
        };
        let args = [
            "update-ref".to_owned(),
            "-d".to_owned(),
            format!("refs/odin/run/{run_id}"),
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
        let order = crate::validate::graph::topo_order(workflow)
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
            }
        }
        let mut started: HashSet<StepId> = settled.clone();
        let mut in_flight = FuturesUnordered::new();
        // A non-`scratch` step mutates the shared workdir, so it runs *exclusively* — never
        // alongside another step. `scratch` steps run in isolated worktrees, so any number
        // may run concurrently (bounded by `max_parallel`).
        let mut exclusive_running = false;

        loop {
            // Fill the ready-set up to the concurrency limit, honoring the exclusivity rule.
            while in_flight.len() < max_parallel && !exclusive_running {
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
                in_flight.push(self.run_one(
                    run_id,
                    step,
                    ctx,
                    workdir,
                    timeout,
                    deps_passed,
                    cancel,
                ));
                if exclusive {
                    break; // nothing else starts beside it
                }
            }

            let Some((id, outcome)) = in_flight.next().await else {
                break; // nothing running and nothing ready — done
            };
            exclusive_running = false; // an exclusive step ran alone, so this clears it

            if let Some(u) = &outcome.usage {
                usage.add(*u);
            }
            side_effects.extend(outcome.side_effects.iter().cloned());
            let step_state = StepState {
                status: outcome.status,
                attempts: outcome.attempts,
                exit_code: outcome.exit_code,
                outputs: outcome.outputs.clone(),
                usage: outcome.usage,
                gates: outcome.gates.clone(),
                judge_score: outcome.judge_score,
            };
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
        })
    }

    /// Runs one step, provisioning an isolated scratch worktree first if `step.scratch`. A
    /// scratch step's file edits stay in its throwaway worktree; its diff is surfaced as
    /// `outputs.diff` and the worktree is removed. Returns `(id, outcome)` for the driver.
    #[allow(clippy::too_many_arguments)]
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
                .map(|(id, st)| step_result(id, st))
                .collect(),
            usage: total_usage(&state.steps),
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
            base_commit: None,
            snapshot: None,
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
        // The run is terminal now — drop the snapshot ref so its commits can be collected.
        // Unconditional for durable runs: a run that snapshotted then disengaged (committed)
        // has `snapshot == None` yet still left the ref behind, so gating on it would leak.
        if workflow.durable {
            self.delete_snapshot_ref(&workdir, run_id, &cancel).await;
        }
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
        let sweep_cancel = CancelToken::new();
        for mut state in store.load_incomplete().await? {
            let Some(workflow) = by_name.get(state.workflow.as_str()).copied() else {
                // The run targets a workflow we no longer serve — best-effort reclaim its
                // snapshot ref from the shared main repo (a no-op if none was created), so
                // dangling commits aren't pinned forever even if the worktree is gone.
                self.delete_snapshot_ref(&self.repo_root, state.run_id, &sweep_cancel)
                    .await;
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
                        if workflow.durable {
                            self.delete_snapshot_ref(&handle.path, state.run_id, &cancel)
                                .await;
                        }
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
                // The workspace is gone (host moved, manual cleanup); cannot resume. The
                // snapshot ref lives in the shared main repo, so reclaim it there (best
                // effort) even though the worktree dir is gone.
                if workflow.durable {
                    self.delete_snapshot_ref(&self.repo_root, state.run_id, &sweep_cancel)
                        .await;
                }
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
    }
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
            base_commit: None,
            snapshot: None,
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
            input: RunInput::manual(),
            workspace: Some(handle),
            base_commit: Some(base),
            snapshot: None,
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
            input: RunInput::manual(),
            workspace: Some(handle),
            base_commit: Some(base),
            snapshot: None,
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
            input: RunInput::manual(),
            workspace: Some(handle),
            base_commit: Some(base.clone()),
            // STALE: points at the pre-commit base, but HEAD has advanced past it.
            snapshot: Some(base),
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
            base_commit: None,
            snapshot: None,
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
            base_commit: None,
            snapshot: None,
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
        // Fails the first attempt (creates a marker), passes the second.
        let wf = parse(
            "name: r\nworkspace: { type: worktree }\nsteps:\n  - id: flaky\n    run: \"test -f .marker || (touch .marker; exit 1)\"\n    retry: { max: 1 }\n",
        );
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
