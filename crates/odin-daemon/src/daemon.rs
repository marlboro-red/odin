//! [`Daemon`]: the supervisor loop that turns trigger events into runs.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use odin_core::ir::TriggerDecl;
use odin_core::traits::TriggerEvent;
use odin_core::{Engine, PrunePolicy, Trigger, Workflow, WorkflowId};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::trigger::CronTrigger;

/// A scheduled retention sweep: apply `policy` every `period`.
struct PruneSchedule {
    policy: PrunePolicy,
    period: std::time::Duration,
}

/// Default ceiling on runs executing at once across the whole daemon (a webhook burst or a
/// fast-firing cron won't spawn unbounded concurrent runs).
const DEFAULT_MAX_CONCURRENT_RUNS: usize = 4;

/// On shutdown, how long to wait for an already-queued trigger event before concluding the queue
/// is drained. A webhook channel hands back its buffered (202-acked) events well within this
/// window, then blocks — ending the drain; a cron trigger's `next_event` sleeps to its next tick,
/// so it always times out here and contributes nothing.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Owns an [`Engine`] and the workflows it can run, and drives a set of long-lived
/// [`Trigger`]s. On [`run`](Daemon::run) it first resumes any incomplete runs (crash
/// recovery), then services every trigger concurrently, dispatching a run per event —
/// up to [`with_max_concurrent_runs`](Daemon::with_max_concurrent_runs) runs at once, so a
/// burst of events (e.g. webhooks) executes in parallel rather than one-at-a-time.
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
    max_concurrent_runs: usize,
    prune: Option<PruneSchedule>,
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
                tracing::warn!(
                    workflow = %prev.name.as_str(),
                    "duplicate workflow name; the later definition replaces the earlier"
                );
            }
        }
        Self {
            engine,
            workflows: Arc::new(map),
            triggers: Vec::new(),
            shutdown: CancellationToken::new(),
            max_concurrent_runs: DEFAULT_MAX_CONCURRENT_RUNS,
            prune: None,
        }
    }

    /// Enables a periodic retention sweep: every `period`, apply `policy` (deleting matching
    /// terminal runs). The FIRST sweep is one `period` after start, never at startup — startup
    /// is the crash-resume path, which deletion must not race. Off unless this is called.
    #[must_use]
    pub fn with_prune(mut self, policy: PrunePolicy, period: std::time::Duration) -> Self {
        self.prune = Some(PruneSchedule { policy, period });
        self
    }

    /// Sets the ceiling on runs executing concurrently across the daemon (default 4). A burst
    /// of events queues for a free slot rather than launching unbounded runs. Clamped to at
    /// least 1.
    #[must_use]
    pub fn with_max_concurrent_runs(mut self, limit: usize) -> Self {
        self.max_concurrent_runs = limit.max(1);
        self
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
    /// A cron expression that fails to parse is **skipped with a warning** rather than
    /// aborting the daemon — one malformed schedule must not take down every workflow in the
    /// directory (mirroring the skip-unreadable-workflow-file behavior). `odin validate`
    /// (ODIN020) is the first line of defense; this is the runtime backstop.
    ///
    /// # Errors
    /// Currently infallible; returns `Result` for forward-compatibility (future wiring may
    /// reintroduce a fallible step here).
    #[allow(clippy::unnecessary_wraps)]
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
                    match CronTrigger::new(&cron.schedule, workflow.name.clone()) {
                        Ok(trigger) => triggers.push(Box::new(trigger)),
                        Err(e) => tracing::warn!(
                            workflow = %workflow.name.as_str(),
                            error = %e,
                            "skipping cron trigger (invalid schedule)"
                        ),
                    }
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
    /// fires. On cancellation each trigger stops fetching new events and **cancels its in-flight
    /// runs** (killing the running step's subprocess) so shutdown is prompt rather than blocking on
    /// a long agentic step; a `durable` run is checkpointed and resumes via `resume_all` on the
    /// next start, a non-durable one is abandoned. `run` returns once the cancelled runs drain.
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
                tracing::info!(count = resumed.len(), "resumed incomplete runs");
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "resume_all failed"),
        }

        if self.triggers.is_empty() && self.prune.is_none() {
            tracing::warn!("no triggers registered; nothing to serve");
            return Ok(());
        }
        tracing::info!(
            triggers = self.triggers.len(),
            max_concurrent_runs = self.max_concurrent_runs,
            prune = self.prune.is_some(),
            "serving"
        );

        // Bounds concurrent runs across ALL triggers; a burst queues for a free slot.
        let permits = Arc::new(Semaphore::new(self.max_concurrent_runs));
        let mut set = tokio::task::JoinSet::new();

        // The optional periodic retention sweep, alongside the trigger tasks (so it runs even
        // for a webhook/approval-only daemon with no cron triggers).
        if let Some(sched) = self.prune {
            let engine = Arc::clone(&self.engine);
            let shutdown = self.shutdown.clone();
            set.spawn(async move { prune_loop(&engine, &sched, &shutdown).await });
        }

        for mut trigger in self.triggers {
            let engine = Arc::clone(&self.engine);
            let workflows = Arc::clone(&self.workflows);
            let shutdown = self.shutdown.clone();
            let permits = Arc::clone(&permits);
            set.spawn(async move {
                let kind = trigger.kind().to_owned();
                // In-flight dispatches for this trigger, so a burst runs concurrently and
                // shutdown can drain them.
                let mut dispatches = tokio::task::JoinSet::new();
                // Spawns a run for one event. The concurrency permit is acquired by the LOOP before
                // calling this (not inside the task) and moved in, so it's held for the whole run
                // AND the loop parks on a full pool — propagating backpressure to the bounded
                // channel and its 503. `None` only on the shutdown-drain path (best-effort
                // last-gasp dispatches that are about to be cancelled).
                let spawn_dispatch = |dispatches: &mut tokio::task::JoinSet<()>,
                                      permit: Option<tokio::sync::OwnedSemaphorePermit>,
                                      event: TriggerEvent| {
                    let engine = Arc::clone(&engine);
                    let workflows = Arc::clone(&workflows);
                    dispatches.spawn(async move {
                        let _permit = permit; // held for the whole run
                        dispatch(engine.as_ref(), &workflows, event).await;
                    });
                };
                loop {
                    // Wait for the next event, but bail the instant shutdown is requested.
                    // `next_event` is cancel-safe, so dropping its future here is fine.
                    let event = tokio::select! {
                        biased;
                        () = shutdown.cancelled() => {
                            // Drain events already queued (e.g. webhook deliveries that were
                            // 202-acked but not yet consumed) before stopping, so an accepted event
                            // is never silently dropped. Pull only what is immediately available:
                            // a webhook channel returns its buffered events at once and then blocks
                            // (the `DRAIN_GRACE` timeout ends the drain); a cron trigger's
                            // `next_event` sleeps to its next tick, so it times out contributing
                            // nothing.
                            while let Ok(Ok(Some(event))) =
                                tokio::time::timeout(DRAIN_GRACE, trigger.next_event()).await
                            {
                                while dispatches.try_join_next().is_some() {}
                                // Take a permit if one is free; else dispatch unbounded — these
                                // are about to be cancelled, so don't wedge the drain on a full
                                // pool whose runs aren't cancelled until after this loop.
                                let permit = Arc::clone(&permits).try_acquire_owned().ok();
                                spawn_dispatch(&mut dispatches, permit, event);
                            }
                            break;
                        }
                        event = trigger.next_event() => event,
                    };
                    // Reap finished dispatches so the set doesn't grow without bound.
                    while dispatches.try_join_next().is_some() {}
                    match event {
                        Ok(Some(event)) => {
                            // Acquire a concurrency permit BEFORE spawning: when every slot is busy
                            // the loop parks here, the bounded channel fills, and the webhook
                            // handler's `try_send` rejects with 503 — the documented backpressure
                            // that was previously defeated by acquiring inside the spawned task.
                            // Race shutdown so a full pool can't wedge a stop.
                            let permit = tokio::select! {
                                biased;
                                () = shutdown.cancelled() => break,
                                p = Arc::clone(&permits).acquire_owned() => {
                                    let Ok(p) = p else {
                                        tracing::error!(%kind, "dispatch semaphore closed; stopping trigger");
                                        break;
                                    };
                                    p
                                },
                            };
                            spawn_dispatch(&mut dispatches, Some(permit), event);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::error!(%kind, error = %e, "trigger stopped");
                            break;
                        }
                    }
                }
                // On shutdown, cancel in-flight runs so a long agentic run can't hold up ctrl-c
                // (otherwise the drain below waits out the per-step timeout). A `durable` run is
                // checkpointed and resumes on the next start; a non-durable one is abandoned. This
                // is best-effort — a dispatch that hadn't yet registered its run still drains.
                if shutdown.is_cancelled() {
                    let n = engine.cancel_all_active();
                    if n > 0 {
                        tracing::info!(%kind, cancelled = n, "shutdown: cancelling in-flight runs");
                    }
                }
                // Drain in-flight dispatches before this trigger task ends.
                while dispatches.join_next().await.is_some() {}
            });
        }
        while let Some(joined) = set.join_next().await {
            if let Err(e) = joined {
                tracing::error!(error = %e, "trigger task panicked");
            }
        }
        Ok(())
    }
}

/// The periodic retention sweep. The first tick is one `period` after start (via `interval_at`),
/// so a prune never races the startup crash-resume; thereafter it applies the policy each period
/// until shutdown. A prune failure is logged, not fatal — the daemon stays up.
async fn prune_loop(engine: &Arc<dyn Engine>, sched: &PruneSchedule, shutdown: &CancellationToken) {
    // Delay the first tick by one period (checked, so an absurd interval degrades to a warning
    // rather than a panic on `Instant + Duration`).
    let Some(start) = tokio::time::Instant::now().checked_add(sched.period) else {
        tracing::warn!("prune interval is too large to schedule; scheduled pruning disabled");
        return;
    };
    let mut tick = tokio::time::interval_at(start, sched.period);
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            _ = tick.tick() => match engine.prune(&sched.policy, false).await {
                Ok(report) if report.runs_pruned > 0 => tracing::info!(
                    runs_pruned = report.runs_pruned,
                    events_pruned = report.events_pruned,
                    "scheduled prune"
                ),
                Ok(_) => tracing::debug!("scheduled prune: nothing eligible"),
                Err(e) => tracing::warn!(error = %e, "scheduled prune failed"),
            },
        }
    }
}

/// Looks up the event's target workflow and runs it, logging the outcome. A missing
/// workflow or a failing run is logged and swallowed so the daemon stays up.
#[tracing::instrument(
    name = "dispatch",
    skip_all,
    fields(source = %event.source, workflow = %event.workflow.as_str())
)]
async fn dispatch(
    engine: &dyn Engine,
    workflows: &HashMap<WorkflowId, Workflow>,
    event: TriggerEvent,
) {
    let Some(workflow) = workflows.get(&event.workflow) else {
        tracing::warn!("event targets an unknown workflow; ignoring");
        return;
    };
    tracing::info!("dispatching run");
    match engine.run(workflow, event.input).await {
        // The engine already logs the run's terminal outcome at the right level (a failed run at
        // ERROR with its reason), so just note the dispatch result at INFO here — no duplicate WARN.
        Ok(summary) => tracing::info!(
            run_id = %summary.run_id,
            status = ?summary.status,
            "dispatched run finished"
        ),
        Err(e) => tracing::error!(error = %e, "dispatch returned an error"),
    }
}
