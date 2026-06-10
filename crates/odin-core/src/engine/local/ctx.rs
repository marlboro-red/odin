//! Pure, `&self`-free helpers for the executor: template-context assembly, retry/backoff
//! policy, judge-score parsing, feedback shaping, and run-state summarization.
//!
//! These were carved out of `local.rs` to shrink the scheduler file; they hold no engine state
//! and are called as free functions from the `LocalEngine` methods. A child module of `local`, so
//! it can construct [`super::StepOutcome`] and read [`super::DIFF`] without widening their
//! visibility.

use std::time::Duration;

use indexmap::IndexMap;
use serde_json::{Value, json};

use super::{DIFF, StepOutcome};
use crate::api::{SideEffect, StepResult, StepStatus};
use crate::context::render::build_context;
use crate::ids::StepId;
use crate::ir::{
    Backoff, FeedbackMode, HumanDuration, RetrySpec, Step, StepKind, Workflow, WorkflowDefaults,
};
use crate::traits::{RunState, StepState};
use crate::usage::Usage;

/// The built-in timeout applied to a subprocess-executing step (`provider` / `run` / `action`) that
/// sets no `timeout:` and whose workflow sets no `defaults.timeout`. Without it, such a step awaits
/// a timeout future that never resolves (the process layer's `sleep_opt(None)`), so a **hung agent
/// or command runs forever** — blocking the run and the daemon's shutdown drain. It's generous so
/// it bounds a genuine hang without cutting off a legitimately long step; override it per-step
/// (`timeout:`) or workflow-wide (`defaults.timeout`).
pub(crate) const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Resolves a step's effective timeout: its own `timeout:`, else the workflow `defaults.timeout`,
/// else — for subprocess-executing kinds only — [`DEFAULT_STEP_TIMEOUT`].
///
/// Human-waiting (`approval`) and coordinating/instant (`case` / `loop`) steps get **no** implicit
/// timeout: an approval gate may legitimately pause for hours, a `case` selector is instant, and a
/// `loop`'s inner steps each carry their own effective timeout.
pub(crate) fn effective_timeout(step: &Step, defaults: &WorkflowDefaults) -> Option<Duration> {
    step.timeout
        .or(defaults.timeout)
        .map(HumanDuration::as_duration)
        .or_else(|| {
            matches!(
                step.kind,
                StepKind::Provider(_) | StepKind::Run(_) | StepKind::Action(_)
            )
            .then_some(DEFAULT_STEP_TIMEOUT)
        })
}

pub(crate) fn skipped_outcome() -> StepOutcome {
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
        raw_stdout: String::new(),
        attempts: 1,
        judge_score: None,
        started_at: None,
        finished_at: None,
    }
}

/// Sums the persisted per-step usage across a run's steps.
pub(crate) fn total_usage(steps: &IndexMap<StepId, StepState>) -> Usage {
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
pub(crate) fn collect_side_effects(steps: &IndexMap<StepId, StepState>) -> Vec<SideEffect> {
    steps
        .values()
        .flat_map(|st| st.side_effects.iter().cloned())
        .collect()
}

pub(crate) fn step_result(id: &StepId, state: &StepState) -> StepResult {
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
        started_at: state.started_at,
        finished_at: state.finished_at,
    }
}

/// Appends a truncated tail of `stderr` to a failure `message`, so a failed step records the
/// actual cause (compiler errors, an auth failure, a stack trace) rather than just an exit
/// code. Keeps the *end* of stderr (where the real error usually is), on a char boundary.
pub(crate) fn with_stderr_tail(message: &str, stderr: &str) -> String {
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
pub(crate) fn join_streams(stdout: &str, stderr: &str) -> String {
    match (stderr.trim(), stdout.trim()) {
        ("", out) => out.to_owned(),
        (err, "") => err.to_owned(),
        (err, out) => format!("{err}\n{out}"),
    }
}

/// Upper bound on the bytes of failure context fed into `retry.feedback`. Each retry re-renders a
/// (paid) provider prompt, so the feedback is capped regardless of which failure path produced it.
pub(crate) const FEEDBACK_MAX: usize = 2000;

/// The last `max` bytes of `s` on a char boundary, prefixed with `…` when truncated.
pub(crate) fn clip_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

/// The maximum stdout persisted/exposed per step. Generous (1 MiB) so it never truncates a real
/// agent answer or downstream-needed output, while bounding the run-state blob (and every later
/// checkpoint's re-serialization of it) when a step emits pathologically large output.
pub(crate) const STDOUT_MAX: usize = 1 << 20;

/// Caps `s` to ~`max` bytes by keeping its HEAD and TAIL (both ends carry signal for a step's
/// output) with a marker in the middle, on char boundaries. Used to bound a step's persisted
/// `outputs.stdout`, which is otherwise re-serialized into the run-state blob at every later
/// checkpoint.
pub(crate) fn clip_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let half = max / 2;
    let mut head_end = half.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - half;
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}\n…[{omitted} bytes truncated]…\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

/// The raw diagnostic `retry.feedback` surfaces: the un-wrapped `detail` (tail-capped to the most
/// recent bytes, with no synthetic headline so its *first* line is real content), or the `headline`
/// alone when there is no detail. Distinct from [`with_stderr_tail`], which keeps the headline for a
/// human/log summary.
/// The first line of a (possibly multi-line) message — for a tracing field, so a `reason`/`error`
/// that ends in a multi-line `stderr:` tail doesn't inject raw newlines into one log event (which
/// breaks line-oriented log shippers). The full detail is still in the run's stored error.
pub(crate) fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

pub(crate) fn failure_detail(headline: &str, detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        return headline.to_owned();
    }
    clip_tail(detail, FEEDBACK_MAX)
}

/// The retry policy a step actually runs under: its own `retry:` if it declared one, else the
/// workflow `defaults.retry`. A *no-op* `step_retry` (the serde default — no retries) counts as
/// "unset" and inherits the default, in the spirit of `step.timeout.or(defaults.timeout)`. Unlike
/// `timeout` (an `Option`, so `None` and `Some(0)` differ), `retry` is a non-`Option` struct whose
/// no-op default and an explicit `retry: { max: 0 }` collapse to the same value — so a step can't
/// *explicitly* opt out of a default; give it any non-default `retry:` to take control.
pub(crate) fn effective_retry<'a>(
    step_retry: &'a RetrySpec,
    default_retry: Option<&'a RetrySpec>,
) -> &'a RetrySpec {
    if step_retry.is_noop() {
        default_retry.unwrap_or(step_retry)
    } else {
        step_retry
    }
}

/// Base inter-attempt retry delay.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

/// The delay before re-attempting after `completed_attempt` failed.
pub(crate) fn backoff_delay(backoff: Backoff, completed_attempt: u32) -> Duration {
    match backoff {
        Backoff::Fixed => RETRY_BASE_DELAY,
        Backoff::Exponential => {
            RETRY_BASE_DELAY * 2u32.pow(completed_attempt.saturating_sub(1).min(6))
        }
    }
}

/// Extracts a `0.0..=1.0` score from judge output: a JSON object with a `score` field,
/// even when the model wraps it in prose or other braces.
pub(crate) fn parse_score(text: &str) -> Option<f32> {
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
pub(crate) fn build_ctx(
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
pub(crate) fn build_ctx_with(
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
pub(crate) fn attempt_context(
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
    use super::clip_middle;

    #[test]
    fn clip_middle_keeps_short_output_whole() {
        assert_eq!(clip_middle("hello", 1024), "hello");
        assert_eq!(clip_middle("", 1024), "");
    }

    #[test]
    fn clip_middle_keeps_head_and_tail_with_a_marker() {
        let s = "A".repeat(100) + &"B".repeat(100);
        let out = clip_middle(&s, 40);
        assert!(
            out.starts_with("AAAAAAAAAAAAAAAAAAAA\n"),
            "keeps the head: {out:?}"
        );
        assert!(
            out.ends_with("\nBBBBBBBBBBBBBBBBBBBB"),
            "keeps the tail: {out:?}"
        );
        assert!(
            out.contains("bytes truncated"),
            "marks the omission: {out:?}"
        );
        // The omitted count is the gap between the kept head and tail.
        assert!(out.contains("160 bytes truncated"), "{out:?}");
    }

    #[test]
    fn clip_middle_respects_char_boundaries() {
        // Multi-byte chars must not be split (no panic, valid UTF-8 out).
        let s = "é".repeat(1000); // 2 bytes each
        let out = clip_middle(&s, 101); // odd, lands mid-char without the boundary walk
        assert!(out.is_char_boundary(0));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}
