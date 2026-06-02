//! [`Daemon`]: the supervisor loop that turns trigger events into runs.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use odin_core::ir::TriggerDecl;
use odin_core::traits::TriggerEvent;
use odin_core::{Engine, Trigger, Workflow, WorkflowId};
use tokio_util::sync::CancellationToken;

use crate::trigger::CronTrigger;

/// Owns an [`Engine`] and the workflows it can run, and drives a set of long-lived
/// [`Trigger`]s. On [`run`](Daemon::run) it first resumes any incomplete runs (crash
/// recovery), then services every trigger concurrently, dispatching one run per event.
///
/// A failing run never takes the daemon down: the error is logged and the trigger keeps
/// firing. A trigger that errors or is exhausted (`Ok(None)`) simply stops; the others
/// continue.
///
/// [`from_workflows`](Daemon::from_workflows) derives only `cron` triggers. `github_webhook`
/// triggers need an HTTP listener, so an embedder must build a [`WebhookServer`] and
/// [`add_trigger`](Daemon::add_trigger) its [`subscribe`](crate::WebhookServer::subscribe)
/// handles (the `odind` binary does exactly this).
///
/// [`WebhookServer`]: crate::WebhookServer
pub struct Daemon {
    engine: Arc<dyn Engine>,
    workflows: Arc<HashMap<WorkflowId, Workflow>>,
    triggers: Vec<Box<dyn Trigger>>,
    shutdown: CancellationToken,
}

impl Daemon {
    /// A daemon serving `workflows` with no triggers yet — add them with
    /// [`with_trigger`](Daemon::with_trigger) or [`add_trigger`](Daemon::add_trigger).
    ///
    /// Workflows are keyed by `name`. A duplicate name is a configuration error: the later
    /// definition wins and a warning is logged, so an operator can spot it rather than
    /// silently serving fewer workflows than were loaded.
    pub fn new(engine: Arc<dyn Engine>, workflows: impl IntoIterator<Item = Workflow>) -> Self {
        let mut map: HashMap<WorkflowId, Workflow> = HashMap::new();
        for workflow in workflows {
            if let Some(prev) = map.insert(workflow.name.clone(), workflow) {
                eprintln!(
                    "odind: duplicate workflow name {:?}; the later definition replaces the earlier",
                    prev.name.as_str()
                );
            }
        }
        Self {
            engine,
            workflows: Arc::new(map),
            triggers: Vec::new(),
            shutdown: CancellationToken::new(),
        }
    }

    /// A handle that, when [cancelled](CancellationToken::cancel), tells [`run`](Daemon::run)
    /// to stop fetching new trigger events and drain in-flight runs. The `odind` binary
    /// wires this to `ctrl-c`; embedders can drive it however they like.
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Builds a daemon whose triggers are derived from each workflow's `triggers:` block:
    /// `cron` declarations become [`CronTrigger`]s. `manual` triggers are skipped (they fire
    /// via `odin run`), and `github_webhook` triggers are wired separately through a
    /// [`WebhookServer`](crate::WebhookServer) — they need an HTTP listener, not just the
    /// supervisor loop — so they are not derived here.
    ///
    /// # Errors
    /// Returns an error if a declared cron expression is invalid.
    pub fn from_workflows(
        engine: Arc<dyn Engine>,
        workflows: impl IntoIterator<Item = Workflow>,
    ) -> Result<Self> {
        let mut daemon = Self::new(engine, workflows);
        let mut triggers: Vec<Box<dyn Trigger>> = Vec::new();
        for workflow in daemon.workflows.values() {
            for decl in &workflow.triggers {
                // Only `cron` is derived here. `github_webhook` is wired by the
                // WebhookServer (it needs an HTTP listener); `manual` runs via `odin run`.
                if let TriggerDecl::Cron(cron) = decl {
                    triggers.push(Box::new(CronTrigger::new(
                        &cron.schedule,
                        workflow.name.clone(),
                    )?));
                }
            }
        }
        daemon.triggers = triggers;
        Ok(daemon)
    }

    /// Registers a trigger (builder style).
    #[must_use]
    pub fn with_trigger(mut self, trigger: impl Trigger + 'static) -> Self {
        self.triggers.push(Box::new(trigger));
        self
    }

    /// Registers an already-boxed trigger.
    pub fn add_trigger(&mut self, trigger: Box<dyn Trigger>) {
        self.triggers.push(trigger);
    }

    /// Number of registered triggers (after `from_workflows` derivation).
    #[must_use]
    pub fn trigger_count(&self) -> usize {
        self.triggers.len()
    }

    /// Resumes incomplete runs, then services every trigger until each is exhausted (for
    /// cron, that is "forever") or the [cancellation token](Daemon::cancellation_token)
    /// fires. On cancellation each trigger stops fetching new events, but a run already in
    /// flight is awaited to completion — a **graceful drain** — before `run` returns.
    ///
    /// Crash recovery applies to **durable** workflows (`durable: true`): their state is
    /// checkpointed at each step, so a run lost to a hard kill resumes via `resume_all` on
    /// the next start. A non-durable run interrupted mid-flight is abandoned with no resume.
    ///
    /// # Errors
    /// Currently infallible (resume and run failures are logged, not propagated), but
    /// returns `Result` so a future fatal-error path is non-breaking.
    pub async fn run(self) -> Result<()> {
        let pending = self.workflows.values().cloned().collect::<Vec<_>>();
        match self.engine.resume_all(&pending).await {
            Ok(resumed) if !resumed.is_empty() => {
                eprintln!("odind: resumed {} incomplete run(s)", resumed.len());
            }
            Ok(_) => {}
            Err(e) => eprintln!("odind: resume_all failed: {e}"),
        }

        if self.triggers.is_empty() {
            eprintln!("odind: no triggers registered; nothing to serve");
            return Ok(());
        }
        eprintln!("odind: serving {} trigger(s)", self.triggers.len());

        let mut set = tokio::task::JoinSet::new();
        for mut trigger in self.triggers {
            let engine = Arc::clone(&self.engine);
            let workflows = Arc::clone(&self.workflows);
            let shutdown = self.shutdown.clone();
            set.spawn(async move {
                let kind = trigger.kind().to_owned();
                loop {
                    // Wait for the next event, but bail the instant shutdown is requested.
                    // `next_event` is cancel-safe, so dropping its future here is fine; a
                    // dispatch already running below is *not* interrupted — it is awaited to
                    // completion before the next iteration observes the shutdown and breaks.
                    let event = tokio::select! {
                        biased;
                        () = shutdown.cancelled() => break,
                        event = trigger.next_event() => event,
                    };
                    match event {
                        Ok(Some(event)) => dispatch(engine.as_ref(), &workflows, event).await,
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("odind: {kind} trigger stopped: {e}");
                            break;
                        }
                    }
                }
            });
        }
        while let Some(joined) = set.join_next().await {
            if let Err(e) = joined {
                eprintln!("odind: trigger task panicked: {e}");
            }
        }
        Ok(())
    }
}

/// Looks up the event's target workflow and runs it, logging the outcome. A missing
/// workflow or a failing run is logged and swallowed so the daemon stays up.
async fn dispatch(
    engine: &dyn Engine,
    workflows: &HashMap<WorkflowId, Workflow>,
    event: TriggerEvent,
) {
    let Some(workflow) = workflows.get(&event.workflow) else {
        eprintln!(
            "odind: event from {} targets unknown workflow {:?}",
            event.source,
            event.workflow.as_str()
        );
        return;
    };
    eprintln!(
        "odind: {} → starting {}",
        event.source,
        workflow.name.as_str()
    );
    match engine.run(workflow, event.input).await {
        Ok(summary) => eprintln!(
            "odind: run {} of {} finished: {:?}",
            summary.run_id,
            workflow.name.as_str(),
            summary.status
        ),
        Err(e) => eprintln!("odind: run of {} failed: {e}", workflow.name.as_str()),
    }
}
