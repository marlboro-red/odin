//! The public integration contract: what comes **in** to start a run, and what goes
//! **out** when it finishes.
//!
//! Both [`RunInput`] and [`RunSummary`] are plain `Serialize + Deserialize` data with no
//! engine internals or trait objects, so the boundary is JSON over any transport. This
//! is the surface an external tool (a GitHub Action, a web service, a cron job) couples
//! to when it embeds Odin.

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{RunId, StepId, WorkflowId};
use crate::usage::Usage;

/// Requirements coming **in**: everything needed to start a run.
///
/// Two channels: typed `params` (validated against the workflow's declared params) and a
/// free-form `trigger_payload` (the event verbatim, reachable as `trigger.*` in
/// templates). The split gives type-checking where structure is declared and an escape
/// hatch where it cannot be.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunInput {
    /// Which declared trigger this run corresponds to. Defaults to `"manual"`.
    #[serde(default = "default_trigger")]
    pub trigger: String,
    /// Free-form trigger payload, surfaced as `trigger.*` in templates.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub trigger_payload: Value,
    /// Param values, by name. Validated & coerced against the workflow's param schema.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub params: IndexMap<String, Value>,
    /// Optional caller-supplied idempotency key: re-submitting the same key returns the
    /// existing run instead of starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

fn default_trigger() -> String {
    "manual".to_owned()
}

impl RunInput {
    /// A manual run with no params.
    #[must_use]
    pub fn manual() -> Self {
        Self {
            trigger: default_trigger(),
            ..Self::default()
        }
    }

    /// Fluent setter for a typed param.
    #[must_use]
    pub fn param(mut self, k: impl Into<String>, v: impl Into<Value>) -> Self {
        self.params.insert(k.into(), v.into());
        self
    }

    /// Fluent setter for the free-form trigger payload.
    #[must_use]
    pub fn with_trigger(mut self, name: impl Into<String>, payload: Value) -> Self {
        self.trigger = name.into();
        self.trigger_payload = payload;
        self
    }
}

/// Results going **out**: the machine-consumable summary of a finished run. Contains no
/// engine internals or trait objects.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunSummary {
    /// Run identity.
    pub run_id: RunId,
    /// The workflow that ran.
    pub workflow: WorkflowId,
    /// Terminal status.
    pub status: RunStatus,
    /// Per-step results, in execution order.
    pub steps: Vec<StepResult>,
    /// Aggregate usage across all provider/judge invocations.
    pub usage: Usage,
    /// Externally-visible effects (PRs opened, branches pushed) for downstream automation.
    pub side_effects: Vec<SideEffect>,
    /// The git diff captured as the implicit `DIFF` artifact, if any.
    pub diff: Option<String>,
    /// Populated iff `status == Failed`: the terminal error, stringified.
    pub error: Option<String>,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// When the run finished, if it has.
    pub finished_at: Option<DateTime<Utc>>,
}

/// Per-step result in a [`RunSummary`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StepResult {
    /// The step id.
    pub id: StepId,
    /// Final status.
    pub status: StepStatus,
    /// Attempts taken.
    pub attempts: u8,
    /// Last exit code.
    pub exit_code: Option<i32>,
    /// Outputs exposed as `steps.<id>.outputs.*`.
    pub outputs: IndexMap<String, Value>,
    /// Gate name → passed?.
    pub gates: IndexMap<String, bool>,
    /// Judge score, if a judge ran.
    pub judge_score: Option<f32>,
    /// Usage for this step.
    pub usage: Option<Usage>,
    /// Why the step failed (exit code + stderr tail, a failed gate, a sub-threshold judge,
    /// a provider/action error) — or, for a `Skipped` step, why it was skipped. `None` for a
    /// step that passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Lifecycle status of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunStatus {
    /// Created, not started.
    Pending,
    /// Executing.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Failed terminally.
    Failed,
    /// Cancelled (user request, timeout, shutdown).
    Cancelled,
}

impl RunStatus {
    /// True for terminal states.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Lifecycle status of a single step.
///
/// Defined here (rather than in `traits::store`) so it is available without the
/// `runtime` feature and so neither `api` nor `traits` depends on the other's gating.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepStatus {
    /// Not yet started.
    Pending,
    /// Currently executing.
    Running,
    /// Completed successfully (gates/judge passed).
    Passed,
    /// Failed terminally.
    Failed,
    /// Skipped because its `when:` evaluated false or an upstream failed.
    Skipped,
}

impl StepStatus {
    /// True for terminal states.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Passed | Self::Failed | Self::Skipped)
    }
}

/// A structured, externally-visible effect a run had on the outside world.
///
/// Internally tagged on `kind`. Both the enum and each struct variant are
/// `#[non_exhaustive]`, so adding a new kind *or* a new field to an existing kind is a
/// non-breaking change — external integrators must match with `..` and ignore the rest.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SideEffect {
    /// A pull request was opened.
    #[non_exhaustive]
    PullRequest {
        /// The PR URL.
        url: String,
        /// The PR number.
        number: u64,
    },
    /// A comment was posted.
    #[non_exhaustive]
    Comment {
        /// The comment URL.
        url: String,
    },
    /// A commit was made.
    #[non_exhaustive]
    Commit {
        /// The commit SHA.
        sha: String,
        /// The branch it landed on, if known.
        branch: Option<String>,
    },
    /// A branch was pushed.
    #[non_exhaustive]
    Push {
        /// The branch name.
        branch: String,
        /// The remote it was pushed to.
        remote: String,
    },
    /// An artifact was written.
    #[non_exhaustive]
    Artifact {
        /// The artifact name.
        name: String,
        /// Its path.
        path: String,
    },
}

impl SideEffect {
    /// A pull request was opened. Constructors are provided because every variant is
    /// `#[non_exhaustive]`; a custom [`crate::Action`] in another crate uses these to record
    /// an outward effect in its [`crate::traits::ActionOutcome`].
    #[must_use]
    pub fn pull_request(url: impl Into<String>, number: u64) -> Self {
        Self::PullRequest {
            url: url.into(),
            number,
        }
    }

    /// A comment was posted at `url`.
    #[must_use]
    pub fn comment(url: impl Into<String>) -> Self {
        Self::Comment { url: url.into() }
    }

    /// A commit `sha` was made, optionally on `branch`.
    #[must_use]
    pub fn commit(sha: impl Into<String>, branch: Option<String>) -> Self {
        Self::Commit {
            sha: sha.into(),
            branch,
        }
    }

    /// `branch` was pushed to `remote`.
    #[must_use]
    pub fn push(branch: impl Into<String>, remote: impl Into<String>) -> Self {
        Self::Push {
            branch: branch.into(),
            remote: remote.into(),
        }
    }

    /// An artifact `name` was written at `path`.
    #[must_use]
    pub fn artifact(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::Artifact {
            name: name.into(),
            path: path.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RunInput, RunStatus, StepStatus};

    #[test]
    fn run_input_builder() {
        let input = RunInput::manual()
            .param("issue", 42)
            .param("branch", "main");
        assert_eq!(input.trigger, "manual");
        assert_eq!(input.params["issue"], serde_json::json!(42));
        assert_eq!(input.params["branch"], serde_json::json!("main"));
    }

    #[test]
    fn run_input_round_trips_as_json() {
        let input = RunInput::manual().param("n", 1);
        let json = serde_json::to_string(&input).unwrap();
        let back: RunInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.params["n"], serde_json::json!(1));
    }

    #[test]
    fn terminal_states() {
        assert!(RunStatus::Succeeded.is_terminal());
        assert!(!RunStatus::Running.is_terminal());
        assert!(StepStatus::Skipped.is_terminal());
        assert!(!StepStatus::Pending.is_terminal());
    }
}
