//! A read-only projection of a run's state for **status views** — the `odin status` CLI, the
//! daemon's `/api/runs` JSON API, and the web dashboard all render the SAME shape, so there is
//! one documented status schema. The projection exposes only what a status view needs (no
//! workspace paths, inputs, or trigger payloads).

use serde::{Deserialize, Serialize};

use crate::ids::ArtifactName;
use crate::traits::RunState;

/// The reserved auto-captured diff artifact (mirrors the engine's `DIFF` constant).
const DIFF: &str = "DIFF";

/// A run projected for a status list: identity, status, per-step progress, and — when the run is
/// paused — the approval gate. Statuses are the lowercase serde strings (`succeeded`, `running`,
/// `awaiting_approval`, …), so a view never drifts from the wire representation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunView {
    /// The run id (UUID string).
    pub run_id: String,
    /// The workflow name.
    pub workflow: String,
    /// The run status, lowercase serde string.
    pub status: String,
    /// When the run was created/started (RFC 3339).
    pub created_at: String,
    /// When the run state was last updated (RFC 3339).
    pub updated_at: String,
    /// Wall-clock run duration in milliseconds, for a **terminal** run (`updated_at - created_at`);
    /// `None` while the run is still in flight or paused.
    pub duration_ms: Option<i64>,
    /// Per-step progress, in execution order.
    pub steps: Vec<StepView>,
    /// For an `awaiting_approval` run: the gate step + its message. `None` otherwise.
    pub gate: Option<GateView>,
}

/// One step in a [`RunView`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StepView {
    /// The step id.
    pub id: String,
    /// The step status, lowercase serde string.
    pub status: String,
    /// The last process exit code, if the step ran.
    pub exit_code: Option<i32>,
    /// Why the step failed/was skipped (the recorded reason), if any.
    pub error: Option<String>,
    /// The step's wall-clock duration in milliseconds, if it ran (`finished_at - started_at`).
    pub duration_ms: Option<i64>,
}

/// The approval gate of a paused run.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GateView {
    /// The gate step id.
    pub step: String,
    /// The message shown to the approver, if the gate set one.
    pub message: Option<String>,
}

/// A run's full detail: the [`RunView`] plus the captured diff and run-level error.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunDetailView {
    /// The list-level view (flattened into the same JSON object).
    #[serde(flatten)]
    pub run: RunView,
    /// The captured `DIFF` artifact, if any (and non-empty).
    pub diff: Option<String>,
    /// The run-level terminal error, if it failed.
    pub error: Option<String>,
}

/// Serializes any value to its canonical lowercase string (a `RunStatus`/`StepStatus` serde
/// tag), so the projection never drifts from the wire representation.
fn tag<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

impl RunView {
    /// Projects a stored run into the list view (no diff).
    #[must_use]
    pub fn project(state: &RunState) -> Self {
        let steps = state
            .steps
            .iter()
            .map(|(id, st)| StepView {
                id: id.as_str().to_owned(),
                status: tag(&st.status),
                exit_code: st.exit_code,
                error: st.error.clone(),
                duration_ms: match (st.started_at, st.finished_at) {
                    (Some(s), Some(f)) => Some((f - s).num_milliseconds().max(0)),
                    _ => None,
                },
            })
            .collect();
        // The gate parked at `awaiting_approval`, if any (matched by serde tag so it can't drift
        // from `StepStatus::AwaitingApproval`).
        let gate = state
            .steps
            .iter()
            .find(|(_, st)| tag(&st.status) == "awaiting_approval")
            .map(|(id, st)| GateView {
                step: id.as_str().to_owned(),
                message: st
                    .outputs
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            });
        // A terminal run's duration is settled (updated_at ≈ when it finished); an in-flight or
        // paused run has no final duration yet. Keyed on the enum (not the serde string) so a new
        // status variant can't silently make this `None` forever. `.max(0)` guards a backwards
        // wall-clock step (NTP) from yielding a negative duration.
        let duration_ms = state.status.is_terminal().then(|| {
            (state.updated_at - state.created_at)
                .num_milliseconds()
                .max(0)
        });
        Self {
            run_id: state.run_id.to_string(),
            workflow: state.workflow.as_str().to_owned(),
            status: tag(&state.status),
            created_at: state.created_at.to_rfc3339(),
            updated_at: state.updated_at.to_rfc3339(),
            duration_ms,
            steps,
            gate,
        }
    }
}

impl RunDetailView {
    /// Projects a stored run into the full detail view (the list view + diff + run-level error).
    #[must_use]
    pub fn project(state: &RunState) -> Self {
        let diff = state
            .artifacts
            .get(&ArtifactName::new(DIFF))
            .filter(|d| !d.is_empty())
            .cloned();
        Self {
            run: RunView::project(state),
            diff,
            error: state.error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RunDetailView, RunView};
    use crate::api::{RunInput, RunStatus, StepStatus};
    use crate::ids::{ArtifactName, RunId, StepId, WorkflowId};
    use crate::traits::{RunState, StepState};
    use chrono::Utc;
    use indexmap::IndexMap;

    fn step(status: StepStatus, message: Option<&str>) -> StepState {
        let mut outputs = IndexMap::new();
        if let Some(m) = message {
            outputs.insert(
                "message".to_owned(),
                serde_json::Value::String(m.to_owned()),
            );
        }
        StepState {
            status,
            attempts: 1,
            exit_code: Some(0),
            outputs,
            usage: None,
            gates: IndexMap::new(),
            judge_score: None,
            side_effects: Vec::new(),
            error: None,
            started_at: None,
            finished_at: None,
        }
    }

    fn awaiting_state() -> RunState {
        let mut steps = IndexMap::new();
        steps.insert(StepId::new("build"), step(StepStatus::Passed, None));
        steps.insert(
            StepId::new("gate"),
            step(StepStatus::AwaitingApproval, Some("Ship it?")),
        );
        let mut artifacts = IndexMap::new();
        artifacts.insert(ArtifactName::new("DIFF"), "diff --git a/x b/x".to_owned());
        RunState {
            run_id: RunId::new(),
            workflow: WorkflowId::new("wf"),
            schema_major: 1,
            status: RunStatus::AwaitingApproval,
            error: None,
            steps,
            artifacts,
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: None,
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn projects_status_steps_and_the_gate() {
        let v = RunView::project(&awaiting_state());
        assert_eq!(v.status, "awaiting_approval");
        assert_eq!(v.workflow, "wf");
        assert_eq!(v.steps.len(), 2);
        assert_eq!(v.steps[0].status, "passed");
        let gate = v.gate.expect("an awaiting run exposes its gate");
        assert_eq!(gate.step, "gate");
        assert_eq!(gate.message.as_deref(), Some("Ship it?"));
    }

    #[test]
    fn detail_includes_the_diff_and_flattens_the_view() {
        let d = RunDetailView::project(&awaiting_state());
        assert_eq!(d.diff.as_deref(), Some("diff --git a/x b/x"));
        assert_eq!(d.run.status, "awaiting_approval");
        // The detail serializes flat (no nested "run" object) — same shape the API promises.
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["status"], "awaiting_approval");
        assert_eq!(json["diff"], "diff --git a/x b/x");
        assert!(
            json.get("run").is_none(),
            "RunView is flattened, not nested"
        );
    }

    #[test]
    fn a_terminal_run_has_no_gate_and_no_empty_diff() {
        let mut s = awaiting_state();
        s.status = RunStatus::Succeeded;
        s.steps
            .insert(StepId::new("gate"), step(StepStatus::Passed, None));
        s.artifacts.insert(ArtifactName::new("DIFF"), String::new()); // empty diff ⇒ None
        let v = RunView::project(&s);
        assert!(v.gate.is_none());
        assert!(RunDetailView::project(&s).diff.is_none());
    }
}
