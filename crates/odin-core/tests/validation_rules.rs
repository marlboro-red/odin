//! One integration test per validation rule (`ODIN001`–`ODIN028`), driving the public
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
