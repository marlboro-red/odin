//! Renders a [`StoreMetrics`] snapshot as Prometheus text-exposition v0.0.4 — hand-written, so
//! the daemon takes no client-library dependency. Terminal-status run counts are a `counter`
//! (they only accumulate as runs finish); the live, fluctuating statuses are `gauge`s.
//!
//! [`Metrics`] adds **cumulative duration histograms** for runs and steps, fed live by the
//! engine's [`on_event`](odin_core::EngineBuilder::on_event) hook (so the daemon never re-scans the
//! store to build them): it tracks each run's/step's start event and observes the elapsed time when
//! the matching finish event fires.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use chrono::{DateTime, Utc};
use odin_core::{RunEvent, RunId, StepId, StoreMetrics};

/// Upper bounds (seconds) for the run/step duration histogram buckets — sub-second shell steps
/// through multi-minute agent steps and long runs.
const DURATION_BUCKETS_SECS: &[f64] = &[
    0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
];

/// A cumulative Prometheus histogram with fixed buckets. Lock-free (atomics) — the engine hook may
/// call [`observe_ms`](Self::observe_ms) from any run's thread concurrently with a `/metrics` read.
struct Histogram {
    /// `buckets[i]` = count of observations ≤ `DURATION_BUCKETS_SECS[i]` (cumulative per bucket).
    buckets: Vec<AtomicU64>,
    /// Sum of all observed durations, in milliseconds (rendered as seconds).
    sum_ms: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            buckets: DURATION_BUCKETS_SECS
                .iter()
                .map(|_| AtomicU64::new(0))
                .collect(),
            sum_ms: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Records one duration. Negative inputs (a backwards wall-clock) are clamped to 0.
    fn observe_ms(&self, ms: i64) {
        let ms = u64::try_from(ms.max(0)).unwrap_or(0);
        self.count.fetch_add(1, Relaxed);
        self.sum_ms.fetch_add(ms, Relaxed);
        #[allow(clippy::cast_precision_loss)]
        let secs = ms as f64 / 1000.0;
        for (i, le) in DURATION_BUCKETS_SECS.iter().enumerate() {
            if secs <= *le {
                self.buckets[i].fetch_add(1, Relaxed);
            }
        }
    }

    fn render(&self, out: &mut String, name: &str, help: &str) {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} histogram");
        for (i, le) in DURATION_BUCKETS_SECS.iter().enumerate() {
            let _ = writeln!(
                out,
                "{name}_bucket{{le=\"{le}\"}} {}",
                self.buckets[i].load(Relaxed)
            );
        }
        let count = self.count.load(Relaxed);
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {count}");
        #[allow(clippy::cast_precision_loss)]
        let sum_secs = self.sum_ms.load(Relaxed) as f64 / 1000.0;
        let _ = writeln!(out, "{name}_sum {sum_secs}");
        let _ = writeln!(out, "{name}_count {count}");
    }
}

/// In-memory daemon metrics fed by the engine event hook: duration histograms for runs and steps.
/// Register [`record`](Self::record) as the engine's `on_event` callback; expose
/// [`render`](Self::render) from `/metrics`.
pub struct Metrics {
    /// Per-run start time (`RunStarted` → removed on `RunFinished`); bounded by in-flight runs.
    run_starts: Mutex<HashMap<RunId, DateTime<Utc>>>,
    /// Per-(run, step) start time; the latest attempt's start wins, removed on the step's finish.
    step_starts: Mutex<HashMap<(RunId, StepId), DateTime<Utc>>>,
    run_hist: Histogram,
    step_hist: Histogram,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// An empty metrics registry. Register [`record`](Self::record) as the engine's `on_event` hook
    /// to start populating it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            run_starts: Mutex::new(HashMap::new()),
            step_starts: Mutex::new(HashMap::new()),
            run_hist: Histogram::new(),
            step_hist: Histogram::new(),
        }
    }

    /// Folds one run event into the histograms. Cheap and non-blocking (brief lock + atomics), as
    /// the `on_event` contract requires.
    pub fn record(&self, run_id: RunId, event: &RunEvent) {
        match event {
            RunEvent::RunStarted { at, .. } => {
                self.run_starts.lock().unwrap().insert(run_id, *at);
            }
            RunEvent::RunFinished { at, .. } => {
                if let Some(start) = self.run_starts.lock().unwrap().remove(&run_id) {
                    self.run_hist.observe_ms((*at - start).num_milliseconds());
                }
                // Drop any step starts left dangling for this run (a finish event we never saw),
                // so the map can't grow without bound across many runs.
                self.step_starts
                    .lock()
                    .unwrap()
                    .retain(|(rid, _), _| *rid != run_id);
            }
            RunEvent::StepStarted { step, at, .. } => {
                self.step_starts
                    .lock()
                    .unwrap()
                    .insert((run_id, step.clone()), *at);
            }
            RunEvent::StepFinished { step, at, .. } => {
                if let Some(start) = self
                    .step_starts
                    .lock()
                    .unwrap()
                    .remove(&(run_id, step.clone()))
                {
                    self.step_hist.observe_ms((*at - start).num_milliseconds());
                }
            }
            _ => {}
        }
    }

    /// The histogram families as Prometheus text (appended after the store-snapshot families).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.run_hist.render(
            &mut out,
            "odin_run_duration_seconds",
            "Wall-clock duration of completed runs.",
        );
        self.step_hist.render(
            &mut out,
            "odin_step_duration_seconds",
            "Wall-clock duration of completed step attempts.",
        );
        out
    }
}

/// The terminal run statuses (their serde strings), emitted as the `odin_runs_total` counter.
const TERMINAL: &[&str] = &["succeeded", "failed", "cancelled"];

/// Formats the snapshot in Prometheus text exposition format. Always emits the gauge families
/// (with `0` when none) so dashboards have a stable series set; the counter has one sample per
/// (workflow, terminal-status) group present.
#[must_use]
pub(crate) fn render(metrics: &StoreMetrics) -> String {
    let mut out = String::new();

    out.push_str("# HELP odin_runs_total Completed runs by workflow and terminal status.\n");
    out.push_str("# TYPE odin_runs_total counter\n");
    let (mut in_flight, mut awaiting, mut pending) = (0_u64, 0_u64, 0_u64);
    for r in &metrics.runs {
        match r.status.as_str() {
            s if TERMINAL.contains(&s) => {
                let _ = writeln!(
                    out,
                    "odin_runs_total{{workflow=\"{}\",status=\"{}\"}} {}",
                    escape(&r.workflow),
                    escape(&r.status),
                    r.count
                );
            }
            "running" => in_flight += r.count,
            "awaiting_approval" => awaiting += r.count,
            "pending" => pending += r.count,
            // A status a newer build understands but this one doesn't: skip rather than
            // mislabel it (the enum is non_exhaustive).
            _ => {}
        }
    }

    gauge(
        &mut out,
        "odin_runs_in_flight",
        "Runs currently executing.",
        in_flight,
    );
    gauge(
        &mut out,
        "odin_runs_awaiting_approval",
        "Runs paused awaiting a human decision.",
        awaiting,
    );
    gauge(
        &mut out,
        "odin_runs_pending",
        "Runs created but not yet started.",
        pending,
    );
    out
}

fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {value}");
}

/// Escapes a Prometheus label value: backslash, double-quote, and newline.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::{Histogram, render};
    use odin_core::{RunStatusCount, StoreMetrics};

    #[test]
    fn histogram_buckets_sum_and_count() {
        let h = Histogram::new();
        h.observe_ms(1500); // 1.5s
        h.observe_ms(3000); // 3s
        let mut out = String::new();
        h.render(&mut out, "test_seconds", "help");
        assert!(out.contains("# TYPE test_seconds histogram"));
        assert!(out.contains("test_seconds_count 2"), "{out}");
        assert!(out.contains("test_seconds_sum 4.5"), "{out}"); // 1.5 + 3
        // Cumulative buckets: le=1 → 0, le=2.5 → 1 (the 1.5s), le=5 → 2 (both), +Inf → 2.
        assert!(out.contains("test_seconds_bucket{le=\"1\"} 0"), "{out}");
        assert!(out.contains("test_seconds_bucket{le=\"2.5\"} 1"), "{out}");
        assert!(out.contains("test_seconds_bucket{le=\"5\"} 2"), "{out}");
        assert!(out.contains("test_seconds_bucket{le=\"+Inf\"} 2"), "{out}");
    }

    #[test]
    fn histogram_clamps_a_negative_duration() {
        let h = Histogram::new();
        h.observe_ms(-50); // a backwards wall-clock → counts as 0, not a giant/under-flowed value
        let mut out = String::new();
        h.render(&mut out, "t", "h");
        assert!(out.contains("t_count 1"));
        assert!(out.contains("t_sum 0"), "{out}");
        assert!(out.contains("t_bucket{le=\"0.5\"} 1"), "{out}"); // 0 <= 0.5
    }

    fn count(workflow: &str, status: &str, count: u64) -> RunStatusCount {
        RunStatusCount::new(workflow, status, count)
    }

    #[test]
    fn renders_counter_for_terminal_and_gauges_for_live_statuses() {
        let m = StoreMetrics::new(vec![
            count("issue-to-pr", "succeeded", 142),
            count("issue-to-pr", "failed", 7),
            count("gated-deploy", "awaiting_approval", 2),
            count("issue-to-pr", "running", 3),
            count("nightly", "pending", 1),
        ]);
        let out = render(&m);
        assert!(out.contains("# TYPE odin_runs_total counter"));
        assert!(out.contains("odin_runs_total{workflow=\"issue-to-pr\",status=\"succeeded\"} 142"));
        assert!(out.contains("odin_runs_total{workflow=\"issue-to-pr\",status=\"failed\"} 7"));
        // Live statuses are summed into gauges, NOT the counter.
        assert!(!out.contains("status=\"running\""));
        assert!(!out.contains("status=\"awaiting_approval\""));
        assert!(out.contains("# TYPE odin_runs_in_flight gauge\nodin_runs_in_flight 3"));
        assert!(out.contains("odin_runs_awaiting_approval 2"));
        assert!(out.contains("odin_runs_pending 1"));
    }

    #[test]
    fn gauges_are_always_present_even_when_empty() {
        let out = render(&StoreMetrics::default());
        assert!(out.contains("odin_runs_in_flight 0"));
        assert!(out.contains("odin_runs_awaiting_approval 0"));
        assert!(out.contains("odin_runs_pending 0"));
    }

    #[test]
    fn label_values_are_escaped() {
        let m = StoreMetrics::new(vec![count("a\"b\\c", "succeeded", 1)]);
        let out = render(&m);
        assert!(out.contains(r#"workflow="a\"b\\c""#), "got: {out}");
    }

    #[test]
    fn an_unknown_future_status_is_skipped_not_mislabeled() {
        let m = StoreMetrics::new(vec![count("w", "quantum_superposition", 9)]);
        let out = render(&m);
        assert!(!out.contains("quantum_superposition"));
        // The known gauges still render.
        assert!(out.contains("odin_runs_in_flight 0"));
    }
}
