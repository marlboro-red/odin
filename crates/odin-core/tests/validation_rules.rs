//! One integration test per validation rule (`ODIN001`–`ODIN042`), driving the public
//! `odin_core` API end-to-end. Each crafts a minimal workflow that should trip exactly
//! the rule under test, plus negative tests asserting clean workflows stay clean.

use odin_core::{DiagCode, KnownNames, ValidationReport, Workflow, validate_source};

fn report(yaml: &str) -> ValidationReport {
    let wf = Workflow::from_yaml_str(yaml).expect("fixture should parse");
    validate_source(yaml, &wf, &KnownNames::builtin())
}

fn assert_fires(yaml: &str, code: DiagCode) {
    let r = report(yaml);
    assert!(
        r.contains(code),
        "expected {code} for:\n{yaml}\n--- got ---\n{r}"
    );
}

fn assert_clean(yaml: &str) {
    let r = report(yaml);
    assert!(
        !r.has_errors(),
        "expected no errors for:\n{yaml}\n--- got ---\n{r}"
    );
}

#[test]
fn odin001_no_steps() {
    assert_fires("name: x\nsteps: []\n", DiagCode::NoSteps);
}

#[test]
fn odin002_empty_step_id() {
    assert_fires(
        "name: x\nsteps:\n  - {id: \"\", run: ./x}\n",
        DiagCode::EmptyStepId,
    );
}

#[test]
fn odin003_duplicate_step_id() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, run: ./x}\n  - {id: a, run: ./y}\n",
        DiagCode::DuplicateStepId,
    );
}

#[test]
fn odin004_invalid_step_id() {
    assert_fires(
        "name: x\nsteps:\n  - {id: \"1bad\", run: ./x}\n",
        DiagCode::InvalidStepId,
    );
}

#[test]
fn odin005_unknown_provider() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, provider: gpt, prompt: hi}\n",
        DiagCode::UnknownProvider,
    );
}

#[test]
fn odin006_missing_prompt() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, provider: claude}\n",
        DiagCode::MissingPrompt,
    );
}

#[test]
fn odin007_duplicate_produces() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, run: ./x, artifacts: {produces: [Y, Y]}}\n",
        DiagCode::DuplicateProduces,
    );
}

#[test]
fn odin008_unsatisfied_requires() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, run: ./x, artifacts: {requires: [NOPE]}}\n",
        DiagCode::UnsatisfiedRequires,
    );
}

#[test]
fn odin009_both_prompt_and_file() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi, prompt_file: p.j2}\n",
        DiagCode::BothPromptAndFile,
    );
}

#[test]
fn odin010_unknown_action() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, action: nope.do}\n",
        DiagCode::UnknownAction,
    );
}

#[test]
fn odin011_judge_threshold_range() {
    assert_fires(
        "name: x\nsteps:\n  - id: a\n    provider: claude\n    prompt: hi\n    judge: {provider: codex, criteria: ok, threshold: 1.5}\n",
        DiagCode::JudgeThresholdRange,
    );
}

#[test]
fn odin012_unknown_dependency() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, run: ./x, depends_on: [ghost]}\n",
        DiagCode::UnknownDependency,
    );
}

#[test]
fn odin013_self_dependency() {
    let yaml = "name: x\nsteps:\n  - {id: a, run: ./x, depends_on: [a]}\n";
    assert_fires(yaml, DiagCode::SelfDependency);
    // A pure self-loop is ODIN013's job; it must not *also* double-report as ODIN014.
    assert!(
        !report(yaml).contains(DiagCode::DependencyCycle),
        "self-loop should not also fire ODIN014"
    );
}

#[test]
fn odin014_dependency_cycle() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, run: ./x, depends_on: [b]}\n  - {id: b, run: ./y, depends_on: [a]}\n",
        DiagCode::DependencyCycle,
    );
}

#[test]
fn odin015_artifact_ordering() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi, artifacts: {produces: [Z]}}\n  - {id: b, run: ./y, artifacts: {requires: [Z]}}\n",
        DiagCode::ArtifactOrdering,
    );
}

#[test]
fn odin016_invalid_pool_size() {
    assert_fires(
        "name: x\nworkspace: {type: slot_pool, pool: 0}\nsteps:\n  - {id: a, run: ./x}\n",
        DiagCode::InvalidPoolSize,
    );
}

#[test]
fn odin017_unknown_template_ref() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ params.ghost }}\"}\n",
        DiagCode::UnknownTemplateRef,
    );
}

#[test]
fn odin018_template_syntax() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ oops \"}\n",
        DiagCode::TemplateSyntax,
    );
}

#[test]
fn odin019_reserved_diff() {
    assert_fires(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi, artifacts: {produces: [DIFF]}}\n",
        DiagCode::ReservedArtifactDiff,
    );
}

#[test]
fn odin020_invalid_cron() {
    assert_fires(
        "name: x\ntriggers:\n  - {type: cron, schedule: \"not a cron\"}\nsteps:\n  - {id: a, run: ./x}\n",
        DiagCode::InvalidCron,
    );
}

#[test]
fn odin021_same_provider_judge() {
    assert_fires(
        "name: x\nsteps:\n  - id: a\n    provider: claude\n    prompt: hi\n    judge: {provider: claude, criteria: ok}\n",
        DiagCode::SameProviderJudge,
    );
}

#[test]
fn odin022_required_with_default() {
    assert_fires(
        "name: x\nparams:\n  p: {required: true, default: 1}\nsteps:\n  - {id: a, run: \"./x {{ params.p }}\"}\n",
        DiagCode::RequiredWithDefault,
    );
}

#[test]
fn odin023_inert_fallback() {
    assert_fires(
        "name: x\nsteps:\n  - id: a\n    provider: claude\n    prompt: hi\n    retry: {max: 1, on_fallback_provider: codex}\n",
        DiagCode::InertFallbackProvider,
    );
}

#[test]
fn odin024_unused_param() {
    assert_fires(
        "name: x\nparams:\n  unused: {}\nsteps:\n  - {id: a, run: ./x}\n",
        DiagCode::UnusedParam,
    );
}

#[test]
fn odin027_webhook_param_undeclared() {
    // `ghost` is mapped by the webhook but never declared in `params` → inert mapping.
    assert_fires(
        "name: x\ntriggers:\n  - type: github_webhook\n    events: [\"issues.labeled\"]\n    params:\n      ghost: issue.html_url\nsteps:\n  - {id: a, run: ./x}\n",
        DiagCode::WebhookParamUndeclared,
    );
}

#[test]
fn odin028_scratch_on_action_step() {
    // An action's effects are discarded with the scratch worktree → warn.
    assert_fires(
        "name: x\nsteps:\n  - { id: c, action: git.commit, scratch: true, with: { message: hi } }\n",
        DiagCode::ScratchOnAction,
    );
}

#[test]
fn odin028_does_not_fire_for_scratch_provider_or_run() {
    // scratch on provider/run is the intended use — no warning.
    let r =
        report("name: x\nmax_parallel: 2\nsteps:\n  - { id: a, run: \"true\", scratch: true }\n");
    assert!(!r.contains(DiagCode::ScratchOnAction), "got:\n{r}");
}

#[test]
fn odin027_does_not_fire_for_a_declared_mapped_param() {
    // A mapping to a declared (and used) param is clean.
    let r = report(
        "name: x\ntriggers:\n  - type: github_webhook\n    events: [\"issues.labeled\"]\n    params:\n      url: issue.html_url\nparams:\n  url: {required: true}\nsteps:\n  - {id: a, run: \"./x {{ params.url }}\"}\n",
    );
    assert!(!r.contains(DiagCode::WebhookParamUndeclared), "got:\n{r}");
}

#[test]
fn odin025_unknown_root_field() {
    assert_fires(
        "name: x\nmystery: 1\nsteps:\n  - {id: a, run: ./x}\n",
        DiagCode::UnknownRootField,
    );
}

#[test]
fn odin026_newer_schema_minor() {
    assert_fires(
        "schema_version: \"1.9\"\nname: x\nsteps:\n  - {id: a, run: ./x}\n",
        DiagCode::NewerSchemaMinor,
    );
}

#[test]
fn odin029_subscript_template_ref() {
    // `steps['a']` exposes only the bare `steps` root to the checker, bypassing the
    // unknown-ref / upstream-dependency checks → ODIN029 surfaces it (warning).
    assert_fires(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: \"echo {{ steps['a'].outputs.x }}\", depends_on: [a]}\n",
        DiagCode::DynamicTemplateRef,
    );
}

#[test]
fn odin029_does_not_fire_for_dot_notation() {
    let r = report(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: \"echo {{ steps.a.outputs.x }}\", depends_on: [a]}\n",
    );
    assert!(!r.contains(DiagCode::DynamicTemplateRef), "got:\n{r}");
}

#[test]
fn odin029_does_not_fire_for_a_nested_key_named_like_a_root() {
    // `trigger.steps[0]` subscripts a *nested* key named `steps`, not the checked root — and
    // `trigger` is an open root, so it must stay clean (no ODIN029, no ODIN017).
    let r = report("name: x\nsteps:\n  - {id: a, run: \"echo {{ trigger.steps[0] }}\"}\n");
    assert!(!r.contains(DiagCode::DynamicTemplateRef), "got:\n{r}");
    assert!(!r.has_errors(), "got:\n{r}");
}

#[test]
fn odin030_param_default_type_mismatch() {
    // `type: number` with a string default is a real authoring bug → ODIN030.
    assert_fires(
        "name: x\nparams:\n  n: {type: number, default: \"not-a-number\"}\nsteps:\n  - {id: a, run: \"echo {{ params.n }}\"}\n",
        DiagCode::ParamDefaultType,
    );
}

#[test]
fn odin030_does_not_fire_for_a_matching_default() {
    let r = report(
        "name: x\nparams:\n  n: {type: number, default: 3}\nsteps:\n  - {id: a, run: \"echo {{ params.n }}\"}\n",
    );
    assert!(!r.contains(DiagCode::ParamDefaultType), "got:\n{r}");
}

#[test]
fn odin031_trigger_payload_into_a_shell_command() {
    // An untrusted `trigger.*` value reaching `sh -c` in a `run:` step → injection risk.
    assert_fires(
        "name: x\nsteps:\n  - {id: a, run: \"echo {{ trigger.issue.title }}\"}\n",
        DiagCode::TriggerIntoShell,
    );
}

#[test]
fn odin031_does_not_fire_for_trigger_in_a_prompt() {
    // A prompt is handed to the agent, not a shell — no injection, no ODIN031.
    let r = report(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ trigger.issue.title }}\"}\n",
    );
    assert!(!r.contains(DiagCode::TriggerIntoShell), "got:\n{r}");
}

#[test]
fn odin032_approval_step_requires_durable() {
    // A pausable approval gate is unresumable without persistence → must be durable.
    // (`durable` defaults to true, so the mistake is explicitly setting it false.)
    assert_fires(
        "name: x\ndurable: false\nsteps:\n  - {id: g, approval: {message: ok?}}\n",
        DiagCode::ApprovalRequiresDurable,
    );
}

#[test]
fn odin032_does_not_fire_for_a_durable_approval_workflow() {
    let r = report("name: x\ndurable: true\nsteps:\n  - {id: g, approval: {message: ok?}}\n");
    assert!(!r.contains(DiagCode::ApprovalRequiresDurable), "got:\n{r}");
}

#[test]
fn odin006_empty_prompt_is_flagged() {
    // A present-but-blank prompt is as good as missing.
    assert_fires(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"   \"}\n",
        DiagCode::MissingPrompt,
    );
}

// ── negative tests: no false positives ──

#[test]
fn requiring_builtin_diff_is_allowed() {
    // `requires: [DIFF]` is legal without any producer — DIFF is auto-captured.
    let r = report(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: \"echo {{ artifacts.DIFF }}\", depends_on: [a], artifacts: {requires: [DIFF]}}\n",
    );
    assert!(!r.contains(DiagCode::UnsatisfiedRequires));
    assert!(!r.has_errors(), "{r}");
}

#[test]
fn current_minor_does_not_warn() {
    // The ODIN026 boundary: an explicit current minor must NOT warn (guards `>` vs `>=`).
    let r = report("schema_version: \"1.0\"\nname: x\nsteps:\n  - {id: a, run: ./x}\n");
    assert!(!r.contains(DiagCode::NewerSchemaMinor));
}

// ── negative tests: clean workflows must not produce errors ──

#[test]
fn linear_pipeline_is_clean() {
    assert_clean(
        "name: x\nparams:\n  url: {required: true}\nsteps:\n  - {id: plan, provider: claude, prompt: \"plan {{ params.url }}\", artifacts: {produces: [P]}}\n  - {id: build, provider: codex, prompt: build, depends_on: [plan], artifacts: {requires: [P]}}\n",
    );
}

#[test]
fn fan_in_dag_is_clean() {
    assert_clean(
        "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: ./b, depends_on: [a]}\n  - {id: c, run: ./c, depends_on: [a]}\n  - {id: d, action: github.open_pr, depends_on: [b, c]}\n",
    );
}

#[test]
fn odin033_case_with_no_branches_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - {id: r, case: {else: other}}\n",
        DiagCode::CaseNoBranches,
    );
}

#[test]
fn odin034_case_duplicate_branch_label_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - id: r\n    case:\n      branches:\n        - {label: a, when: \"true\"}\n        - {label: a, when: \"false\"}\n",
        DiagCode::CaseDuplicateBranchLabel,
    );
}

#[test]
fn odin034_else_label_colliding_with_a_branch_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - id: r\n    case:\n      branches:\n        - {label: a, when: \"true\"}\n      else: a\n",
        DiagCode::CaseDuplicateBranchLabel,
    );
}

#[test]
fn odin035_case_empty_branch_label_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - id: r\n    case:\n      branches:\n        - {label: \"\", when: \"true\"}\n",
        DiagCode::CaseEmptyBranchLabel,
    );
}

#[test]
fn a_valid_case_step_is_clean() {
    assert_clean(
        "name: x\nsteps:\n  - {id: c, run: \"echo bug\"}\n  - id: r\n    depends_on: [c]\n    case:\n      branches:\n        - {label: bug,  when: \"steps.c.outputs.stdout == 'bug'\"}\n        - {label: docs, when: \"steps.c.outputs.stdout == 'docs'\"}\n      else: other\n  - {id: fix, run: \"echo fix\", depends_on: [r], when: \"steps.r.outputs.selected == 'bug'\"}\n",
    );
}

#[test]
fn odin036_gates_or_judge_on_a_case_selector_warns() {
    assert_fires(
        "name: x\nsteps:\n  - id: r\n    gates: {check: \"true\"}\n    case:\n      branches:\n        - {label: a, when: \"true\"}\n",
        DiagCode::CaseInertChecks,
    );
}

// ── loop: validation (ODIN037–042 + inner-step coverage via all_steps) ──────────

/// A loop whose body is a self-contained, acyclic sub-DAG with unique inner ids is clean.
#[test]
fn a_valid_loop_step_is_clean() {
    assert_clean(
        "name: x\nsteps:\n  - id: fix\n    loop:\n      until: \"steps.test.status == 'passed'\"\n      max: 3\n      steps:\n        - {id: edit, run: \"echo edit\"}\n        - {id: test, run: \"echo test\", depends_on: [edit]}\n",
    );
}

#[test]
fn odin037_blank_until_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - {id: f, loop: {until: \" \", max: 2, steps: [{id: e, run: x}]}}\n",
        DiagCode::LoopMissingUntil,
    );
}

#[test]
fn odin038_zero_max_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - {id: f, loop: {until: a, max: 0, steps: [{id: e, run: x}]}}\n",
        DiagCode::LoopZeroMax,
    );
}

#[test]
fn odin039_empty_body_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - {id: f, loop: {until: a, max: 2, steps: []}}\n",
        DiagCode::LoopNoSteps,
    );
}

#[test]
fn odin040_nested_loop_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - id: f\n    loop:\n      until: a\n      max: 2\n      steps:\n        - {id: inner, loop: {until: b, max: 2, steps: [{id: x, run: z}]}}\n",
        DiagCode::LoopNested,
    );
}

#[test]
fn odin041_inner_approval_is_flagged() {
    assert_fires(
        "name: x\ndurable: true\nsteps:\n  - id: f\n    loop:\n      until: a\n      max: 2\n      steps:\n        - {id: gate, approval: {}}\n",
        DiagCode::LoopInnerApproval,
    );
}

#[test]
fn odin042_gates_on_a_loop_node_warns() {
    assert_fires(
        "name: x\nsteps:\n  - id: f\n    gates: {check: \"true\"}\n    loop: {until: a, max: 2, steps: [{id: e, run: x}]}\n",
        DiagCode::LoopInertChecks,
    );
}

/// Inner ids share the flat namespace: a collision with a top-level id is ODIN003.
#[test]
fn loop_inner_id_colliding_with_top_level_is_a_duplicate() {
    assert_fires(
        "name: x\nsteps:\n  - {id: edit, run: x}\n  - {id: f, loop: {until: a, max: 2, steps: [{id: edit, run: y}]}}\n",
        DiagCode::DuplicateStepId,
    );
}

/// Inner provider/action/prompt refs are validated like any other step (via `all_steps`).
#[test]
fn loop_inner_unknown_provider_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - {id: f, loop: {until: a, max: 2, steps: [{id: e, provider: bogus, prompt: hi}]}}\n",
        DiagCode::UnknownProvider,
    );
}

/// A loop body is self-contained: an inner `depends_on` to an outer step is ODIN012.
#[test]
fn loop_inner_depends_on_outer_step_is_unknown() {
    assert_fires(
        "name: x\nsteps:\n  - {id: setup, run: x}\n  - id: f\n    depends_on: [setup]\n    loop:\n      until: a\n      max: 2\n      steps:\n        - {id: e, run: y, depends_on: [setup]}\n",
        DiagCode::UnknownDependency,
    );
}

/// A cycle within the loop body is ODIN014, the same as a top-level cycle.
#[test]
fn loop_inner_cycle_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - id: f\n    loop:\n      until: a\n      max: 2\n      steps:\n        - {id: e1, run: x, depends_on: [e2]}\n        - {id: e2, run: x, depends_on: [e1]}\n",
        DiagCode::DependencyCycle,
    );
}

/// The flat namespace also catches a collision between two different loops' inner ids.
#[test]
fn two_loops_sharing_an_inner_id_collide() {
    assert_fires(
        "name: x\nsteps:\n  - {id: f1, loop: {until: a, max: 2, steps: [{id: e, run: x}]}}\n  - {id: f2, loop: {until: b, max: 2, steps: [{id: e, run: y}]}}\n",
        DiagCode::DuplicateStepId,
    );
}

/// Inner-step artifacts are validated over the body sub-graph: an unproduced require is ODIN008.
#[test]
fn loop_inner_unsatisfied_require_is_flagged() {
    assert_fires(
        "name: x\nsteps:\n  - id: f\n    loop:\n      until: a\n      max: 2\n      steps:\n        - {id: e, run: x, artifacts: {requires: [NOPE]}}\n",
        DiagCode::UnsatisfiedRequires,
    );
}

/// A body whose producer is an inner upstream of the consumer is clean (ordering holds).
#[test]
fn loop_inner_artifact_produced_upstream_is_clean() {
    assert_clean(
        "name: x\nsteps:\n  - id: f\n    loop:\n      until: \"true\"\n      max: 2\n      steps:\n        - {id: make, run: x, artifacts: {produces: [ART]}}\n        - {id: use, run: y, depends_on: [make], artifacts: {requires: [ART]}}\n",
    );
}
