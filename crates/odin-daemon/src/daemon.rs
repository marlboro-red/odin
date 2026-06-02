//! [`Daemon`]: the supervisor loop that turns trigger events into runs.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use odin_core::ir::TriggerDecl;
use odin_core::traits::TriggerEvent;
use odin_core::{Engine, Trigger, Workflow, WorkflowId};

use crate::trigger::CronTrigger;

/// Owns an [`Engine`] and the workflows it can run, and drives a set of long-lived
/// [`Trigger`]s. On [`run`](Daemon::run) it first resumes any incomplete runs (crash
/// recovery), then services every trigger concurrently, dispatching one run per event.
///
/// A failing run never takes the daemon down: the error is logged and the trigger keeps
/// firing. A trigger that errors or is exhausted (`Ok(None)`) simply stops; the others
/// continue.
pub struct Daemon {
    engine: Arc<dyn Engine>,
    workflows: Arc<HashMap<WorkflowId, Workflow>>,
    triggers: Vec<Box<dyn Trigger>>,
}

impl Daemon {
    /// A daemon serving `workflows` with no triggers yet — add them with
    /// [`with_trigger`](Daemon::with_trigger) or [`add_trigger`](Daemon::add_trigger).
    /// A later workflow with a duplicate `name` replaces an earlier one.
    pub fn new(engine: Arc<dyn Engine>, workflows: impl IntoIterator<Item = Workflow>) -> Self {
        let workflows = workflows
            .into_iter()
            .map(|w| (w.name.clone(), w))
            .collect::<HashMap<_, _>>();
        Self {
            engine,
            workflows: Arc::new(workflows),
            triggers: Vec::new(),
        }
    }

    /// Builds a daemon whose triggers are derived from each workflow's `triggers:` block:
    /// `cron` declarations become [`CronTrigger`]s. `manual` triggers are skipped — they
    /// fire via `odin run`, not the daemon — and `github_webhook` triggers are not served
    /// yet (a notice is logged so the omission is visible).
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
                match decl {
                    TriggerDecl::Cron(cron) => {
                        triggers.push(Box::new(CronTrigger::new(
                            &cron.schedule,
                            workflow.name.clone(),
                        )?));
                    }
                    TriggerDecl::GithubWebhook(_) => {
                        eprintln!(
                            "odind: workflow {:?} declares a github_webhook trigger, \
                             which is not served yet (cron + manual only)",
                            workflow.name.as_str()
                        );
                    }
                    // `manual` runs via `odin run`, not the daemon; the wildcard also
                    // covers future #[non_exhaustive] trigger kinds.
                    _ => {}
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
    /// cron/webhook, that is "forever"). Cancel the returned future — e.g. on `ctrl-c` —
    /// to shut down; durable in-flight runs resume from their last checkpoint on restart.
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
            set.spawn(async move {
                let kind = trigger.kind().to_owned();
                loop {
                    match trigger.next_event().await {
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
