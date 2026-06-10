//! The `/metrics` duration histograms are fed by the engine's `on_event` hook (not a store re-scan):
//! run a real workflow over a temp git repo and confirm the run + its step were observed. This is
//! the proper test for `Metrics::record`, whose `RunEvent` inputs can't be constructed cross-crate
//! (they're `#[non_exhaustive]`) — so we drive it with real events from the engine.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use odin_core::{EngineBuilder, RunInput, Workflow};
use odin_daemon::Metrics;

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "t@odin.invalid"]);
    git(dir, &["config", "user.name", "Odin Test"]);
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "init"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_hook_records_run_and_step_durations() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());

    let metrics = Arc::new(Metrics::new());
    let engine = EngineBuilder::new()
        .repo(repo.path())
        .on_event({
            let m = metrics.clone();
            move |id, ev| m.record(id, ev)
        })
        .build()
        .unwrap();

    let wf = Workflow::from_yaml_str(
        "name: m\nworkspace: { type: worktree }\nsteps:\n  - {id: a, run: \"true\"}\n",
    )
    .unwrap();
    engine.run(&wf, RunInput::manual()).await.unwrap();

    let out = metrics.render();
    assert!(
        out.contains("odin_run_duration_seconds_count 1"),
        "run histogram not recorded:\n{out}"
    );
    assert!(
        out.contains("odin_step_duration_seconds_count 1"),
        "step histogram not recorded:\n{out}"
    );
    // A real observation lands in a finite bucket and the +Inf total equals the count.
    assert!(
        out.contains("odin_run_duration_seconds_bucket{le=\"+Inf\"} 1"),
        "{out}"
    );
}

/// A step that retries emits multiple `StepStarted` but ONE `StepFinished`, so it must be observed
/// exactly ONCE (not per attempt) — the earliest start to settle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_step_is_observed_once() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());

    let metrics = Arc::new(Metrics::new());
    let engine = EngineBuilder::new()
        .repo(repo.path())
        .on_event({
            let m = metrics.clone();
            move |id, ev| m.record(id, ev)
        })
        .build()
        .unwrap();

    // `false` always fails; `max: 2` runs two attempts, then the step settles Failed (one finish).
    let wf = Workflow::from_yaml_str(
        "name: r\ndurable: true\nworkspace: { type: worktree }\nsteps:\n  - {id: flaky, run: \"false\", retry: { max: 2 }}\n",
    )
    .unwrap();
    let s = engine.run(&wf, RunInput::manual()).await.unwrap();
    assert_eq!(s.status, odin_core::RunStatus::Failed);

    let out = metrics.render();
    assert!(
        out.contains("odin_step_duration_seconds_count 1"),
        "a retried (2-attempt) step must be observed once, not per attempt:\n{out}"
    );
}
