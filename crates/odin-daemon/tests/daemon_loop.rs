//! End-to-end tests for the [`Daemon`] supervisor loop: a scripted trigger drives a real
//! engine over a temp git repo (shell-only workflows, no provider/API cost), and we assert
//! the runs land in the store.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use odin_core::traits::TriggerEvent;
use odin_core::{
    EngineBuilder, RunInput, RunStatus, SqliteStore, Store, Trigger, TriggerError, Workflow,
    WorkflowId,
};
use odin_daemon::Daemon;

/// A trigger that replays a fixed script of events, then signals exhaustion with `None`.
struct ScriptTrigger(Mutex<VecDeque<TriggerEvent>>);

impl ScriptTrigger {
    fn new(events: impl IntoIterator<Item = TriggerEvent>) -> Self {
        Self(Mutex::new(events.into_iter().collect()))
    }
}

#[async_trait]
impl Trigger for ScriptTrigger {
    // The trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn kind(&self) -> &str {
        "script"
    }

    async fn next_event(&mut self) -> Result<Option<TriggerEvent>, TriggerError> {
        Ok(self.0.get_mut().unwrap().pop_front())
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "t@odin.invalid"]);
    git(dir, &["config", "user.name", "Odin Test"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "init"]);
}

/// A durable, shell-only workflow named `name` with one always-passing step.
fn tick_workflow(name: &str) -> Workflow {
    let src = format!(
        "name: {name}\nworkspace: {{ type: worktree }}\ndurable: true\nsteps:\n  - {{ id: noop, run: \"true\" }}\n"
    );
    Workflow::from_yaml_str(&src).unwrap()
}

fn event_for(workflow: &str) -> TriggerEvent {
    TriggerEvent::new("script", WorkflowId::new(workflow), RunInput::manual())
}

/// Builds an engine over `repo` backed by a fresh SQLite store, returning both the engine
/// and the store handle (so the test can inspect recorded runs afterwards).
fn engine_with_store(repo: &Path) -> (Arc<dyn odin_core::Engine>, Arc<SqliteStore>) {
    let db = repo.join("state.db");
    let store = Arc::new(SqliteStore::open(&db).unwrap());
    let engine = EngineBuilder::new()
        .repo(repo)
        .store(store.clone())
        .build()
        .unwrap();
    (engine, store)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_runs_a_workflow_when_a_trigger_fires() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let (engine, store) = engine_with_store(repo.path());

    let daemon = Daemon::new(engine, [tick_workflow("tick")])
        .with_trigger(ScriptTrigger::new([event_for("tick")]));
    daemon.run().await.unwrap();

    let runs = store.recent(10).await.unwrap();
    assert_eq!(runs.len(), 1, "exactly one run should have been recorded");
    assert_eq!(runs[0].workflow, WorkflowId::new("tick"));
    assert_eq!(runs[0].status, RunStatus::Succeeded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_for_unknown_workflows_are_ignored_but_known_ones_still_run() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let (engine, store) = engine_with_store(repo.path());

    // The unknown-workflow event must be swallowed (logged) without taking the daemon down
    // or blocking the legitimate run that follows it.
    let daemon = Daemon::new(engine, [tick_workflow("tick")])
        .with_trigger(ScriptTrigger::new([event_for("ghost"), event_for("tick")]));
    daemon.run().await.unwrap();

    let runs = store.recent(10).await.unwrap();
    assert_eq!(runs.len(), 1, "only the known workflow should have run");
    assert_eq!(runs[0].workflow, WorkflowId::new("tick"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cron_run_of_a_required_param_workflow_fails_without_killing_the_daemon() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let (engine, store) = engine_with_store(repo.path());

    // A cron event carries no params, so a workflow with a *required* param must fail
    // validation at dispatch — and the daemon must log it and keep running, recording no
    // successful run (this is the failure mode the CronTrigger docstring promises).
    let src = "name: needs-param\nworkspace: { type: worktree }\ndurable: true\nparams:\n  who: { required: true }\nsteps:\n  - { id: noop, run: \"true\" }\n";
    let workflow = Workflow::from_yaml_str(src).unwrap();
    let daemon = Daemon::new(engine, [workflow])
        .with_trigger(ScriptTrigger::new([event_for("needs-param")]));
    daemon.run().await.unwrap();

    let runs = store.recent(10).await.unwrap();
    assert!(
        runs.iter().all(|r| r.status != RunStatus::Succeeded),
        "a param-less cron run of a required-param workflow must not succeed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_triggers_run_concurrently_and_add_trigger_registers() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let (engine, store) = engine_with_store(repo.path());

    // Two independent triggers (one via the builder, one via add_trigger) drive two
    // different workflows; both must run.
    let mut daemon = Daemon::new(engine, [tick_workflow("alpha"), tick_workflow("beta")])
        .with_trigger(ScriptTrigger::new([event_for("alpha")]));
    daemon.add_trigger(Box::new(ScriptTrigger::new([event_for("beta")])));
    assert_eq!(daemon.trigger_count(), 2);
    daemon.run().await.unwrap();

    let runs = store.recent(10).await.unwrap();
    let names: std::collections::HashSet<String> = runs
        .iter()
        .map(|r| r.workflow.as_str().to_owned())
        .collect();
    assert_eq!(runs.len(), 2, "both triggered workflows should have run");
    assert!(names.contains("alpha") && names.contains("beta"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_of_events_all_run_and_drain_on_shutdown() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let (engine, store) = engine_with_store(repo.path());

    // One trigger emits four events at once; each must produce a run, and run() must drain
    // all in-flight dispatches before returning (the trigger is exhausted after the burst).
    let burst = (0..4).map(|_| event_for("tick")).collect::<Vec<_>>();
    let daemon =
        Daemon::new(engine, [tick_workflow("tick")]).with_trigger(ScriptTrigger::new(burst));
    daemon.run().await.unwrap();

    let runs = store.recent(20).await.unwrap();
    assert_eq!(runs.len(), 4, "all four burst events should have run");
    assert!(runs.iter().all(|r| r.status == RunStatus::Succeeded));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_concurrent_runs_is_clamped_to_at_least_one() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let (engine, store) = engine_with_store(repo.path());

    // A limit of 0 must clamp to 1 — `Semaphore::new(0)` would otherwise deadlock and this
    // test would hang. Two events should still both run (serialized).
    let daemon = Daemon::new(engine, [tick_workflow("tick")])
        .with_max_concurrent_runs(0)
        .with_trigger(ScriptTrigger::new([event_for("tick"), event_for("tick")]));
    daemon.run().await.unwrap();

    assert_eq!(store.recent(20).await.unwrap().len(), 2);
}

#[test]
fn from_workflows_derives_one_trigger_per_cron_decl() {
    // Manual-only → 0 triggers; one cron → 1; two crons → 2. Webhooks are not served yet.
    let engine = EngineBuilder::new().build().unwrap();
    let manual = Workflow::from_yaml_str(
        "name: m\ntriggers: [{ type: manual }]\nsteps: [{ id: s, run: \"true\" }]\n",
    )
    .unwrap();
    let one_cron = Workflow::from_yaml_str(
        "name: c1\ntriggers: [{ type: cron, schedule: \"0 3 * * 1\" }]\nsteps: [{ id: s, run: \"true\" }]\n",
    )
    .unwrap();
    let two_cron = Workflow::from_yaml_str(
        "name: c2\ntriggers:\n  - { type: cron, schedule: \"0 0 * * *\" }\n  - { type: cron, schedule: \"*/15 * * * *\" }\nsteps: [{ id: s, run: \"true\" }]\n",
    )
    .unwrap();

    let daemon = Daemon::from_workflows(engine, [manual, one_cron, two_cron]).unwrap();
    assert_eq!(daemon.trigger_count(), 3);
}

#[test]
fn shipped_nightly_example_derives_a_cron_trigger() {
    // The end-to-end tie: the example `odind` is documented to serve must yield a working
    // cron trigger through the same `from_workflows` path the binary uses.
    const NIGHTLY: &str = include_str!("../../../examples/nightly-maintenance.yaml");
    let engine = EngineBuilder::new().build().unwrap();
    let workflow = Workflow::from_yaml_str(NIGHTLY).unwrap();
    let daemon = Daemon::from_workflows(engine, [workflow]).unwrap();
    assert_eq!(daemon.trigger_count(), 1);
}

#[test]
fn from_workflows_skips_an_invalid_cron_without_aborting() {
    // A cron decl that passes IR shape-checking but is semantically unparseable must NOT
    // abort the whole daemon — it is skipped with a warning, so other workflows still run.
    // (`odin validate`/ODIN020 is the first line of defense against shipping such a schedule.)
    let engine = EngineBuilder::new().build().unwrap();
    let bad = Workflow::from_yaml_str(
        "name: bad\ntriggers: [{ type: cron, schedule: \"99 99 * * *\" }]\nsteps: [{ id: s, run: \"true\" }]\n",
    )
    .unwrap();
    let good = Workflow::from_yaml_str(
        "name: good\ntriggers: [{ type: cron, schedule: \"0 3 * * 1-7\" }]\nsteps: [{ id: s, run: \"true\" }]\n",
    )
    .unwrap();
    let daemon = Daemon::from_workflows(engine, [bad, good]).expect("must not abort on a bad cron");
    // Only the valid schedule (incl. the every-day `1-7` that used to crash) is registered.
    assert_eq!(daemon.trigger_count(), 1);
}
