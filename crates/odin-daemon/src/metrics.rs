//! Renders a [`StoreMetrics`] snapshot as Prometheus text-exposition v0.0.4 — hand-written, so
//! the daemon takes no client-library dependency. Terminal-status run counts are a `counter`
//! (they only accumulate as runs finish); the live, fluctuating statuses are `gauge`s.

use std::fmt::Write as _;

use odin_core::StoreMetrics;

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
    use super::render;
    use odin_core::{RunStatusCount, StoreMetrics};

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
