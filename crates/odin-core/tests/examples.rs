//! Integration tests over the shipped example workflows: they must parse, validate as
//! documented, and round-trip through serde.

use odin_core::{DiagCode, KnownNames, Workflow, validate_source};

const ISSUE_TO_PR: &str = include_str!("../../../examples/issue-to-pr.yaml");
const FIX_FLAKY: &str = include_str!("../../../examples/fix-flaky-test.yaml");
const NIGHTLY: &str = include_str!("../../../examples/nightly-maintenance.yaml");
const MULTI_AGENT: &str = include_str!("../../../examples/multi-agent-eval.yaml");

#[test]
fn issue_to_pr_is_completely_clean() {
    let wf = Workflow::from_yaml_str(ISSUE_TO_PR).expect("parses");
    let report = validate_source(ISSUE_TO_PR, &wf, &KnownNames::builtin());
    assert!(
        report.is_empty(),
        "expected zero diagnostics, got:\n{report}"
    );
}

#[test]
fn multi_agent_eval_is_clean_and_parallel() {
    let wf = Workflow::from_yaml_str(MULTI_AGENT).expect("parses");
    let report = validate_source(MULTI_AGENT, &wf, &KnownNames::builtin());
    assert!(
        report.is_empty(),
        "expected zero diagnostics, got:\n{report}"
    );
    // The point of this example: concurrent scratch candidates that converge at a judge.
    assert_eq!(wf.max_parallel.map(std::num::NonZeroUsize::get), Some(3));
    let scratch = wf.steps.iter().filter(|s| s.scratch).count();
    assert_eq!(scratch, 3, "three concurrent scratch candidates");
}

#[test]
fn nightly_maintenance_is_clean_and_cron_triggered() {
    let wf = Workflow::from_yaml_str(NIGHTLY).expect("parses");
    let report = validate_source(NIGHTLY, &wf, &KnownNames::builtin());
    assert!(
        report.is_empty(),
        "expected zero diagnostics, got:\n{report}"
    );
    // The daemon's reason to exist: this example is served by a cron trigger, not manually.
    assert_eq!(wf.triggers.len(), 1);
    assert!(matches!(
        wf.triggers[0],
        odin_core::ir::TriggerDecl::Cron(_)
    ));
}

#[test]
fn fix_flaky_has_only_the_documented_warning() {
    let wf = Workflow::from_yaml_str(FIX_FLAKY).expect("parses");
    let report = validate_source(FIX_FLAKY, &wf, &KnownNames::builtin());
    assert!(!report.has_errors(), "expected no errors, got:\n{report}");
    assert_eq!(
        report.warning_count(),
        1,
        "expected exactly one warning:\n{report}"
    );
    assert!(report.contains(DiagCode::InertFallbackProvider));
}

#[test]
fn fix_flaky_exercises_every_step_kind_and_trigger() {
    use odin_core::StepKind;
    let wf = Workflow::from_yaml_str(FIX_FLAKY).expect("parses");
    assert_eq!(wf.triggers.len(), 3);
    assert_eq!(wf.steps.len(), 5);
    let kinds: Vec<&str> = wf
        .steps
        .iter()
        .map(|s| match s.kind {
            StepKind::Provider(_) => "provider",
            StepKind::Action(_) => "action",
            StepKind::Run(_) => "run",
            StepKind::Approval(_) => "approval",
        })
        .collect();
    assert!(kinds.contains(&"provider"));
    assert!(kinds.contains(&"action"));
    assert!(kinds.contains(&"run"));
}

#[test]
fn examples_round_trip_through_serde() {
    for src in [ISSUE_TO_PR, FIX_FLAKY, NIGHTLY, MULTI_AGENT] {
        let wf = Workflow::from_yaml_str(src).expect("parses");
        let reserialized = serde_yaml_ng::to_string(&wf).expect("serializes");
        let again = Workflow::from_yaml_str(&reserialized).expect("re-parses");
        assert_eq!(wf.name, again.name);
        assert_eq!(wf.steps.len(), again.steps.len());
    }
}

#[test]
fn fix_flaky_round_trip_preserves_rich_content() {
    use odin_core::StepKind;
    let wf = Workflow::from_yaml_str(FIX_FLAKY).expect("parses");
    let reserialized = serde_yaml_ng::to_string(&wf).expect("serializes");
    let again = Workflow::from_yaml_str(&reserialized).expect("re-parses");

    // The kitchen-sink content must survive: gates, a cross-provider judge with its
    // threshold, a when-conditional, prompt_file vs prompt, and the inert-fallback retry.
    let implement = again
        .steps
        .iter()
        .find(|s| s.id.as_str() == "implement")
        .unwrap();
    assert!(!implement.gates.is_empty(), "gates dropped on round-trip");
    assert!(
        implement.retry.on_fallback_provider.is_some(),
        "fallback dropped"
    );
    assert!(matches!(&implement.kind, StepKind::Provider(p) if p.prompt_file.is_some()));

    let review = again
        .steps
        .iter()
        .find(|s| s.id.as_str() == "review")
        .unwrap();
    let judge = review.judge.as_ref().expect("judge dropped");
    assert!((judge.threshold - 0.7).abs() < f32::EPSILON);
    assert!(again.steps.iter().any(|s| s.when.is_some()), "when dropped");

    // And the reserialized form still validates to exactly the documented warning.
    let report = validate_source(&reserialized, &again, &KnownNames::builtin());
    assert_eq!(report.warning_count(), 1);
    assert!(report.contains(DiagCode::InertFallbackProvider));
}
