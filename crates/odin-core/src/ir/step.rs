//! Steps and the exactly-one-of-kind discriminated union.
//!
//! A step is one node in the workflow DAG. Its *kind* — provider invocation, built-in
//! action, shell `run:` hook, human `approval:` gate, or `case:` branch selector — is an
//! exactly-one-of choice. We deserialize a `Step`
//! through a private, `deny_unknown_fields` raw struct and then resolve the kind by
//! hand, so that:
//!
//! - a typo'd field (`tmeout:`) is a hard error (serde's `flatten` would otherwise
//!   swallow it — `deny_unknown_fields` is silently ignored alongside `flatten`), and
//! - "found none" / "found more than one" kind, and a field on the wrong kind
//!   (`prompt:` on an `action:` step), all produce precise parse-time errors.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::duration::HumanDuration;
use crate::ids::{ArtifactName, GateName, ProviderRef, StepId};

/// One node in the workflow DAG.
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct Step {
    /// Stable, author-assigned id. Unique & non-empty (`ODIN002`/`ODIN003`) and a valid
    /// template path segment (`ODIN004`).
    pub id: StepId,

    /// Exactly one of provider/action/run. Flattened on serialize so YAML reads
    /// naturally (`provider: claude` sits directly on the step).
    #[serde(flatten)]
    pub kind: StepKind,

    /// Named artifact data-flow on top of the shared workdir.
    #[serde(default, skip_serializing_if = "Artifacts::is_empty")]
    pub artifacts: Artifacts,

    /// Named gate commands: name → shell command. All must exit 0. Deterministic order.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub gates: IndexMap<GateName, String>,

    /// Optional LLM-as-judge over this step's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeSpec>,

    /// Retry policy. Defaults to no retries.
    #[serde(default, skip_serializing_if = "RetrySpec::is_noop")]
    pub retry: RetrySpec,

    /// Wall-clock timeout for the step body. Defaults to the workflow default / none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<HumanDuration>,

    /// Minijinja boolean expression; the step is skipped when it evaluates false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,

    /// DAG edges: ids of steps that must complete before this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<StepId>,

    /// Run this step in an isolated **scratch** workspace (a throwaway git worktree at the
    /// run's base) instead of the shared workdir. Its file edits never touch the shared tree
    /// — its diff is exposed as `steps.<id>.outputs.diff` — so scratch steps can run
    /// concurrently (fan-out). Pass them data via templating (`steps.*`, `params.*`,
    /// `trigger.*`), not via uncommitted files. See `max_parallel`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub scratch: bool,
}

// serde's `skip_serializing_if` requires `fn(&T) -> bool`, hence `&bool`.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// The body of a step. Exactly one variant is present in valid YAML.
#[derive(Clone, Debug)]
pub enum StepKind {
    /// Invoke a pinned coding-agent provider with a prompt.
    Provider(ProviderStep),
    /// Run a registered in-process [`crate::traits::Action`].
    Action(ActionStep),
    /// Shell out to an external command (code hook, any language).
    Run(RunStep),
    /// A human-in-the-loop gate: the run **pauses** here until a person approves or rejects it.
    Approval(ApprovalStep),
    /// Conditional branching: a *selector* that records which of N labeled branches to take. The
    /// branch *bodies* are ordinary downstream steps the author gates on `outputs.selected`.
    Case(CaseStep),
}

impl StepKind {
    /// Resolves the raw discriminant fields into a kind, rejecting none / more-than-one
    /// and any field that belongs to a different kind.
    #[allow(clippy::too_many_arguments)]
    fn from_discriminants(
        provider: Option<ProviderRef>,
        prompt: Option<String>,
        prompt_file: Option<String>,
        action: Option<String>,
        with: Option<IndexMap<String, Value>>,
        run: Option<String>,
        approval: Option<ApprovalStep>,
        case: Option<CaseStep>,
    ) -> Result<Self, String> {
        let count = [
            provider.is_some(),
            action.is_some(),
            run.is_some(),
            approval.is_some(),
            case.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        if count == 0 {
            return Err(
                "step must declare exactly one of `provider:`, `action:`, `run:`, `approval:`, \
                 or `case:` (found none)"
                    .to_owned(),
            );
        }
        if count > 1 {
            return Err(
                "step declares more than one of `provider:`, `action:`, `run:`, `approval:`, \
                 `case:` — choose exactly one"
                    .to_owned(),
            );
        }

        if let Some(case) = case {
            if prompt.is_some() || prompt_file.is_some() {
                return Err("`prompt`/`prompt_file` are only valid on `provider:` steps".to_owned());
            }
            if with.is_some() {
                return Err("`with:` is only valid on `action:` steps".to_owned());
            }
            return Ok(StepKind::Case(case));
        }

        if let Some(provider) = provider {
            if with.is_some() {
                return Err("`with:` is only valid on `action:` steps".to_owned());
            }
            Ok(StepKind::Provider(ProviderStep {
                provider,
                prompt,
                prompt_file,
            }))
        } else if let Some(action) = action {
            if prompt.is_some() || prompt_file.is_some() {
                return Err("`prompt`/`prompt_file` are only valid on `provider:` steps".to_owned());
            }
            Ok(StepKind::Action(ActionStep {
                action,
                with: with.unwrap_or_default(),
            }))
        } else if let Some(approval) = approval {
            if prompt.is_some() || prompt_file.is_some() {
                return Err("`prompt`/`prompt_file` are only valid on `provider:` steps".to_owned());
            }
            if with.is_some() {
                return Err("`with:` is only valid on `action:` steps".to_owned());
            }
            Ok(StepKind::Approval(approval))
        } else {
            let run = run.expect("count == 1 and only run remains");
            if prompt.is_some() || prompt_file.is_some() {
                return Err("`prompt`/`prompt_file` are only valid on `provider:` steps".to_owned());
            }
            if with.is_some() {
                return Err("`with:` is only valid on `action:` steps".to_owned());
            }
            Ok(StepKind::Run(RunStep { run }))
        }
    }
}

/// Provider-invocation step body.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderStep {
    /// Registry key of the provider to invoke (e.g. `"claude"`). Validated (`ODIN005`).
    pub provider: ProviderRef,
    /// Inline prompt template (minijinja). Mutually exclusive with `prompt_file` (`ODIN009`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Path to a prompt template file. Mutually exclusive with `prompt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_file: Option<String>,
}

/// Built-in-action step body.
#[derive(Clone, Debug, Serialize)]
pub struct ActionStep {
    /// Registry key of the action (e.g. `"github.open_pr"`). Validated (`ODIN010`).
    pub action: String,
    /// Free-form, templated arguments passed to the action.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub with: IndexMap<String, Value>,
}

/// Run-hook step body.
#[derive(Clone, Debug, Serialize)]
pub struct RunStep {
    /// Shell command line. Runs in the step's workdir.
    pub run: String,
}

/// Human-in-the-loop approval gate. The run pauses at this step (status `AwaitingApproval`)
/// until a person approves it (the gate passes and downstream proceeds) or rejects it (the
/// gate fails, carrying the reviewer's note as feedback) — via `odin approve`/`odin reject`
/// or the daemon's `POST /approve`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ApprovalStep {
    /// Message shown to the approver (e.g. "Review the diff before opening the PR").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A conditional-branching **selector**. Evaluates each branch's `when:` guard in order and
/// records the **first** matching label (or `else`) as `outputs.selected`; it always passes
/// (branching is a decision, not a failure). Branch *bodies* are ordinary downstream steps the
/// author gates on the decision, e.g. `when: "steps.<id>.outputs.selected == 'bug'"`, and a join
/// `depends_on: [<id>]` (the selector always passes) so the merge-back works.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CaseStep {
    /// The labeled branches, tried in order; the first whose guard is true wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<CaseBranch>,
    /// Fallback label selected when no guard matched. If absent, `selected` is the empty string.
    #[serde(default, rename = "else", skip_serializing_if = "Option::is_none")]
    pub else_: Option<String>,
}

/// One labeled arm of a [`CaseStep`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CaseBranch {
    /// The branch label, recorded as `outputs.selected` when chosen. Unique within the case.
    pub label: String,
    /// Minijinja boolean guard. `None` matches unconditionally (an explicit catch-all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

impl Serialize for StepKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let mut m = s.serialize_map(None)?;
        match self {
            StepKind::Case(c) => {
                m.serialize_entry("case", c)?;
            }
            StepKind::Provider(p) => {
                m.serialize_entry("provider", &p.provider)?;
                if let Some(x) = &p.prompt {
                    m.serialize_entry("prompt", x)?;
                }
                if let Some(x) = &p.prompt_file {
                    m.serialize_entry("prompt_file", x)?;
                }
            }
            StepKind::Action(a) => {
                m.serialize_entry("action", &a.action)?;
                if !a.with.is_empty() {
                    m.serialize_entry("with", &a.with)?;
                }
            }
            StepKind::Run(r) => {
                m.serialize_entry("run", &r.run)?;
            }
            StepKind::Approval(a) => {
                m.serialize_entry("approval", a)?;
            }
        }
        m.end()
    }
}

/// Private raw form: every possible step field, flat, with unknown fields rejected.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepRaw {
    id: StepId,
    #[serde(default)]
    provider: Option<ProviderRef>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    prompt_file: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    with: Option<IndexMap<String, Value>>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    approval: Option<ApprovalStep>,
    #[serde(default)]
    case: Option<CaseStep>,
    #[serde(default)]
    artifacts: Artifacts,
    #[serde(default)]
    gates: IndexMap<GateName, String>,
    #[serde(default)]
    judge: Option<JudgeSpec>,
    #[serde(default)]
    retry: RetrySpec,
    #[serde(default)]
    timeout: Option<HumanDuration>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    depends_on: Vec<StepId>,
    #[serde(default)]
    scratch: bool,
}

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let r = StepRaw::deserialize(d)?;
        let kind = StepKind::from_discriminants(
            r.provider,
            r.prompt,
            r.prompt_file,
            r.action,
            r.with,
            r.run,
            r.approval,
            r.case,
        )
        .map_err(|msg| D::Error::custom(format!("step \"{}\": {msg}", r.id)))?;
        Ok(Step {
            id: r.id,
            kind,
            artifacts: r.artifacts,
            gates: r.gates,
            judge: r.judge,
            retry: r.retry,
            timeout: r.timeout,
            when: r.when,
            depends_on: r.depends_on,
            scratch: r.scratch,
        })
    }
}

/// Artifact data-flow declared by a step.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Artifacts {
    /// Named artifacts this step needs present before it runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<ArtifactName>,
    /// Named artifacts this step is expected to produce. `DIFF` is engine-auto-captured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produces: Vec<ArtifactName>,
}

impl Artifacts {
    /// True if neither requires nor produces anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.requires.is_empty() && self.produces.is_empty()
    }
}

/// LLM-as-judge configuration for a step.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct JudgeSpec {
    /// Provider used as the judge (a registry key). Validated (`ODIN005`).
    pub provider: ProviderRef,
    /// Natural-language criteria the output must satisfy.
    pub criteria: String,
    /// Pass threshold in `0.0..=1.0` (`ODIN011`). Defaults to 0.5.
    #[serde(default = "default_threshold")]
    pub threshold: f32,
}

fn default_threshold() -> f32 {
    0.5
}

/// Retry policy for a step.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RetrySpec {
    /// Max *additional* attempts after the first. 0 = no retry (default).
    #[serde(default)]
    pub max: u8,
    /// Backoff strategy between attempts.
    #[serde(default)]
    pub backoff: Backoff,
    /// Provider to switch to on final failure. **Inert in v1** (routing is a later
    /// layer); parsed and validated so workflows are forward-compatible (`ODIN023`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fallback_provider: Option<ProviderRef>,
}

impl RetrySpec {
    /// True if this is the do-nothing default (used to skip it on serialize).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.max == 0 && self.backoff == Backoff::Fixed && self.on_fallback_provider.is_none()
    }
}

/// Backoff strategy between retry attempts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Backoff {
    /// Constant delay between attempts. The default.
    #[default]
    Fixed,
    /// Exponentially increasing delay.
    Exponential,
}

#[cfg(test)]
mod tests {
    use super::{Step, StepKind};

    fn parse(y: &str) -> Result<Step, serde_yaml_ng::Error> {
        serde_yaml_ng::from_str(y)
    }

    #[test]
    fn provider_step_parses() {
        let s = parse("id: plan\nprovider: claude\nprompt: do it\n").unwrap();
        assert_eq!(s.id.as_str(), "plan");
        assert!(matches!(s.kind, StepKind::Provider(_)));
    }

    #[test]
    fn action_and_run_parse() {
        let a = parse("id: pr\naction: github.open_pr\nwith: {title: hi}\n").unwrap();
        assert!(matches!(a.kind, StepKind::Action(_)));
        let r = parse("id: gen\nrun: ./x.sh\n").unwrap();
        assert!(matches!(r.kind, StepKind::Run(_)));
    }

    #[test]
    fn zero_kinds_is_a_precise_error() {
        let err = parse("id: x\nprompt: hi\n").unwrap_err().to_string();
        assert!(err.contains("exactly one"), "got: {err}");
        assert!(err.contains("found none"), "got: {err}");
    }

    #[test]
    fn case_step_parses_with_branches_and_else() {
        let s = parse(
            "id: route\ncase:\n  branches:\n    - {label: bug, when: \"a == 1\"}\n    - {label: docs}\n  else: other\n",
        )
        .unwrap();
        let StepKind::Case(c) = s.kind else {
            panic!("expected a case step");
        };
        assert_eq!(c.branches.len(), 2);
        assert_eq!(c.branches[0].label, "bug");
        assert_eq!(c.branches[0].when.as_deref(), Some("a == 1"));
        assert_eq!(c.branches[1].when, None); // a guard-less branch is a catch-all
        assert_eq!(c.else_.as_deref(), Some("other"));
    }

    #[test]
    fn case_with_another_kind_is_more_than_one_error() {
        let err = parse("id: x\nprovider: claude\nprompt: hi\ncase: {branches: []}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than one"), "got: {err}");
    }

    #[test]
    fn case_step_round_trips_through_yaml() {
        let yaml =
            "id: route\ncase:\n  branches:\n    - {label: bug, when: \"a == 1\"}\n  else: other\n";
        let once = serde_yaml_ng::to_string(&parse(yaml).unwrap()).unwrap();
        let twice = serde_yaml_ng::to_string(&parse(&once).unwrap()).unwrap();
        assert_eq!(once, twice, "case serialization should be idempotent");
        assert!(once.contains("case:") && once.contains("label: bug"));
    }

    #[test]
    fn two_kinds_is_a_precise_error() {
        let err = parse("id: x\nprovider: claude\nrun: ./x.sh\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than one"), "got: {err}");
    }

    #[test]
    fn prompt_on_action_step_is_rejected() {
        let err = parse("id: x\naction: a\nprompt: hi\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("only valid on `provider:`"), "got: {err}");
    }

    #[test]
    fn prompt_on_run_step_is_rejected() {
        let err = parse("id: x\nrun: ./s\nprompt: hi\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("only valid on `provider:`"), "got: {err}");
    }

    #[test]
    fn with_on_run_step_is_rejected() {
        let err = parse("id: x\nrun: ./s\nwith: {k: v}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("only valid on `action:`"), "got: {err}");
    }

    #[test]
    fn with_on_provider_step_is_rejected() {
        let err = parse("id: x\nprovider: claude\nprompt: hi\nwith: {k: v}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("only valid on `action:`"), "got: {err}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        // `tmeout` typo — this is exactly what flatten+deny_unknown_fields would miss.
        let err = parse("id: x\nprovider: claude\ntmeout: 5m\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("tmeout") || err.contains("unknown field"),
            "got: {err}"
        );
    }

    #[test]
    fn round_trips() {
        let s = parse("id: plan\nprovider: claude\nprompt: do it\ndepends_on: [a]\n").unwrap();
        let out = serde_yaml_ng::to_string(&s).unwrap();
        let again = parse(&out).unwrap();
        assert_eq!(again.id, s.id);
        assert_eq!(again.depends_on, s.depends_on);
    }
}
