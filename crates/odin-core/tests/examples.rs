//! Integration tests over the shipped example workflows: they must parse, validate as
//! documented, and round-trip through serde.

use odin_core::{DiagCode, KnownNames, Workflow, validate_source};

const ISSUE_TO_PR: &str = include_str!("../../../examples/issue-to-pr.yaml");
const FIX_FLAKY: &str = include_str!("../../../examples/fix-flaky-test.yaml");
const NIGHTLY: &str = include_str!("../../../examples/nightly-maintenance.yaml");
const MULTI_AGENT: &str = include_str!("../../../examples/multi-agent-eval.yaml");
const GATED_DEPLOY: &str = include_str!("../../../examples/gated-deploy.yaml");
const ITERATE: &str = include_str!("../../../examples/iterate.yaml");
const SELF_CORRECT: &str = include_str!("../../../examples/self-correct.yaml");
const TRIAGE: &str = include_str!("../../../examples/triage.yaml");
const SHIP_RELEASE: &str = include_str!("../../../examples/ship-release.yaml");
const LOOP_WITH_CASE: &str = include_str!("../../../examples/loop-with-case.yaml");
const ADVERSARIAL_REVIEW: &str = include_str!("../../../examples/adversarial-review.yaml");
const LOCAL_REVIEW: &str = include_str!("../../../examples/local-review.yaml");

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
fn fix_flaky_has_only_the_documented_warnings() {
    let wf = Workflow::from_yaml_str(FIX_FLAKY).expect("parses");
    let report = validate_source(FIX_FLAKY, &wf, &KnownNames::builtin());
    assert!(!report.has_errors(), "expected no errors, got:\n{report}");
    // Two documented warnings: the inert `on_fallback_provider` (ODIN023) and, because the example
    // pairs `durable` with a `slot_pool` workspace, the resume-fragility advisory (ODIN044).
    assert_eq!(
        report.warning_count(),
        2,
        "expected exactly two warnings:\n{report}"
    );
    assert!(report.contains(DiagCode::InertFallbackProvider));
    assert!(report.contains(DiagCode::SlotPoolNotDurable));
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
            StepKind::Case(_) => "case",
            StepKind::Loop(_) => "loop",
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

    // And the reserialized form still validates to exactly the documented warnings.
    let report = validate_source(&reserialized, &again, &KnownNames::builtin());
    assert_eq!(report.warning_count(), 2);
    assert!(report.contains(DiagCode::InertFallbackProvider));
    assert!(report.contains(DiagCode::SlotPoolNotDurable));
}

/// Parses, validates clean, and confirms the headline feature of each remaining shipped example,
/// so a stale or broken example file fails CI rather than misleading a reader.
#[test]
fn gated_deploy_validates_and_gates_on_approval() {
    use odin_core::StepKind;
    let wf = Workflow::from_yaml_str(GATED_DEPLOY).expect("parses");
    assert!(
        validate_source(GATED_DEPLOY, &wf, &KnownNames::builtin()).is_empty(),
        "gated-deploy should validate clean"
    );
    assert!(wf.durable, "an approval workflow must be durable (ODIN032)");
    assert!(
        wf.steps
            .iter()
            .any(|s| matches!(s.kind, StepKind::Approval(_))),
        "gated-deploy must demonstrate an approval gate"
    );
}

#[test]
fn iterate_validates_and_has_a_loop() {
    use odin_core::StepKind;
    let wf = Workflow::from_yaml_str(ITERATE).expect("parses");
    assert!(
        validate_source(ITERATE, &wf, &KnownNames::builtin()).is_empty(),
        "iterate should validate clean"
    );
    assert!(
        wf.steps.iter().any(|s| matches!(s.kind, StepKind::Loop(_))),
        "iterate must demonstrate a loop"
    );
}

#[test]
fn self_correct_validates_and_retries_with_feedback() {
    let wf = Workflow::from_yaml_str(SELF_CORRECT).expect("parses");
    assert!(
        validate_source(SELF_CORRECT, &wf, &KnownNames::builtin()).is_empty(),
        "self-correct should validate clean"
    );
    assert!(
        wf.steps.iter().any(|s| s.retry.max > 0),
        "self-correct must demonstrate a retrying step"
    );
}

#[test]
fn triage_validates_and_branches_on_a_case() {
    use odin_core::StepKind;
    let wf = Workflow::from_yaml_str(TRIAGE).expect("parses");
    assert!(
        validate_source(TRIAGE, &wf, &KnownNames::builtin()).is_empty(),
        "triage should validate clean"
    );
    assert!(
        wf.steps.iter().any(|s| matches!(s.kind, StepKind::Case(_))),
        "triage must demonstrate a case selector"
    );
}

#[test]
fn ship_release_validates_and_uses_shell_exec() {
    use odin_core::StepKind;
    let wf = Workflow::from_yaml_str(SHIP_RELEASE).expect("parses");
    assert!(
        validate_source(SHIP_RELEASE, &wf, &KnownNames::builtin()).is_empty(),
        "ship-release should validate clean"
    );
    assert!(
        wf.steps
            .iter()
            .any(|s| matches!(&s.kind, StepKind::Action(a) if a.action == "shell.exec")),
        "ship-release must demonstrate the shell.exec action"
    );
}

#[test]
fn loop_with_case_validates_and_nests_a_case_in_the_loop() {
    use odin_core::StepKind;
    let wf = Workflow::from_yaml_str(LOOP_WITH_CASE).expect("parses");
    assert!(
        validate_source(LOOP_WITH_CASE, &wf, &KnownNames::builtin()).is_empty(),
        "loop-with-case should validate clean"
    );
    let body = wf
        .steps
        .iter()
        .find_map(|s| match &s.kind {
            StepKind::Loop(l) => Some(&l.steps),
            _ => None,
        })
        .expect("loop-with-case must have a loop");
    assert!(
        body.iter().any(|s| matches!(s.kind, StepKind::Case(_))),
        "the loop body must nest a case selector"
    );
}

#[test]
fn adversarial_review_validates_and_fans_out_reviewers() {
    use odin_core::StepKind;
    // Odin reviewing its own PRs: webhook-triggered, three concurrent scratch reviewers, a
    // cross-provider judge, and an approval gate before the comment is posted.
    let wf = Workflow::from_yaml_str(ADVERSARIAL_REVIEW).expect("parses");
    assert!(
        validate_source(ADVERSARIAL_REVIEW, &wf, &KnownNames::builtin()).is_empty(),
        "adversarial-review should validate clean"
    );
    assert!(
        wf.triggers
            .iter()
            .any(|t| matches!(t, odin_core::ir::TriggerDecl::GithubWebhook(_))),
        "must be webhook-triggered (the dogfood entry point)"
    );
    assert_eq!(
        wf.steps.iter().filter(|s| s.scratch).count(),
        3,
        "three concurrent scratch reviewers"
    );
    assert_eq!(wf.max_parallel.map(std::num::NonZeroUsize::get), Some(3));
    assert!(
        wf.steps
            .iter()
            .any(|s| matches!(s.kind, StepKind::Approval(_))),
        "an approval gate before posting"
    );
    assert!(
        wf.steps.iter().any(|s| s.judge.is_some()),
        "a cross-provider judge on the synthesis"
    );
}

#[test]
fn local_review_validates_and_is_a_stateless_one_shot() {
    use odin_core::StepKind;
    // The simplest dogfood: fetch a diff, review it with one provider, write a report — no
    // trigger, no durability, no approval. (Verified end-to-end against PR #68 with the real
    // `claude` CLI; this test pins the shape.)
    let wf = Workflow::from_yaml_str(LOCAL_REVIEW).expect("parses");
    assert!(
        validate_source(LOCAL_REVIEW, &wf, &KnownNames::builtin()).is_empty(),
        "local-review should validate clean"
    );
    assert!(!wf.durable, "a stateless one-shot (run with --no-store)");
    assert!(
        wf.triggers.is_empty(),
        "no trigger — run manually via `odin run`"
    );
    assert!(
        wf.steps
            .iter()
            .any(|s| matches!(s.kind, StepKind::Provider(_))),
        "must have a provider review step"
    );
    assert!(
        !wf.steps
            .iter()
            .any(|s| matches!(s.kind, StepKind::Approval(_))),
        "no approval gate — it never posts anything outward"
    );
}
