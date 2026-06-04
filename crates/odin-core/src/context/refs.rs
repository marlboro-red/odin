//! Static checking of template references (`ODIN017`/`ODIN018`), unused params (`ODIN024`),
//! subscript access that escapes the checks (`ODIN029`), and untrusted `trigger.*` reaching a
//! shell command (`ODIN031`).
//!
//! For every templated string on a step we (1) compile it with minijinja — a compile
//! error is `ODIN018` — and (2) walk the referenced variable paths, checking each
//! against the [`ContextShape`] and the step's dependency-aware visibility rules.

use std::collections::HashSet;

use indexmap::{IndexMap, IndexSet};

use super::shape::ContextShape;
use crate::ids::{ArtifactName, StepId};
use crate::ir::{StepKind, Workflow};
use crate::validate::diagnostic::{DiagCode, Diagnostic};
use crate::validate::rules::{step_ptr, suggest};

/// The engine-reserved artifact, always referenceable.
const DIFF: &str = "DIFF";

/// Roots that are always allowed but whose children are not statically modeled. `retry` exposes
/// the per-attempt `retry.attempt` / `retry.feedback`; `loop` exposes `loop.counter` /
/// `loop.feedback` inside a `loop:` body (both per-attempt/iteration, children not modeled).
const OPEN_ROOTS: &[&str] = &["trigger", "run", "retry", "loop"];

/// minijinja's built-in global functions (minijinja 2.20). `undeclared_variables` reports a call
/// like `range(…)` as an undeclared variable `range`, so without this exemption a perfectly valid
/// `{{ range(loop.counter) }}` would trip a false ODIN017. (Re-verify this list on a minijinja bump.)
const MINIJINJA_GLOBALS: &[&str] = &["range", "dict", "namespace", "debug"];

/// The keys a `steps.<id>` object exposes in the render context (see `build_ctx_with`): the step's
/// `outputs` map, its `exit_code`, and its `status`. A dotted `steps.<id>.<field>` is checked
/// against this set so a typo (`steps.a.exitcode`) is caught; `outputs`' own children are dynamic
/// and so are not modeled past that point.
const STEP_FIELDS: &[&str] = &["outputs", "exit_code", "status"];

/// Checks every template reference in the workflow, appending diagnostics.
pub(crate) fn check(
    wf: &Workflow,
    ancestors: &IndexMap<StepId, IndexSet<StepId>>,
    d: &mut Vec<Diagnostic>,
) {
    let shape = ContextShape::of(wf);
    let mut used_params: IndexSet<String> = IndexSet::new();
    // A subscript on `params` (`params[expr]`) references some param by a key we can't read
    // statically; once seen, we can't prove *any* param unused, so ODIN024 is suppressed.
    let mut params_subscripted = false;
    let empty = IndexSet::new();

    for (i, s) in wf.steps.iter().enumerate() {
        let requires = require_set(s);
        let anc = ancestors.get(&s.id).unwrap_or(&empty);
        process_templates(
            collect_templates(&step_ptr(i), s),
            &shape,
            &requires,
            anc,
            &mut used_params,
            &mut params_subscripted,
            d,
        );

        // A `loop:` body is checked against a SCOPED context: its inner ids are visible (in
        // addition to the top-level ids), and each inner step / the `until` guard resolves
        // `steps.<id>` against the body's own dependency order plus the loop node's outer
        // upstreams. Inner ids never enter the top-level `ContextShape`, so the dotted-id trap
        // (`steps.loop.inner`) cannot arise.
        if let StepKind::Loop(l) = &s.kind {
            check_loop_body(
                i,
                &s.id,
                l,
                &shape,
                ancestors,
                &mut used_params,
                &mut params_subscripted,
                d,
            );
        }
    }

    // ODIN024 — declared but never referenced (inline templates only; prompt_file
    // contents are not loaded, so a param used only there is not counted). A dynamic
    // `params[…]` subscript could reference any param, so it suppresses the check entirely.
    if !params_subscripted {
        for name in wf.params.keys() {
            if !used_params.contains(name.as_str()) {
                d.push(Diagnostic::new(
                    DiagCode::UnusedParam,
                    format!("params.{name}"),
                    format!("param {:?} is declared but never referenced", name.as_str()),
                ));
            }
        }
    }
}

/// A step's `artifacts.requires` as a name set (the visibility scope for `artifacts.*` refs).
fn require_set(s: &crate::ir::Step) -> IndexSet<&str> {
    s.artifacts
        .requires
        .iter()
        .map(ArtifactName::as_str)
        .collect()
}

/// Analyzes and checks a batch of templated strings against one `(shape, requires, ancestors)`
/// scope, accumulating used params and the `params[…]`-subscript flag. The single per-template
/// engine used by both the top-level pass and a `loop:` body.
#[allow(clippy::too_many_arguments)]
fn process_templates(
    templates: Vec<Templated>,
    shape: &ContextShape,
    requires: &IndexSet<&str>,
    ancestors: &IndexSet<StepId>,
    used_params: &mut IndexSet<String>,
    params_subscripted: &mut bool,
    d: &mut Vec<Diagnostic>,
) {
    for tpl in templates {
        let source = if tpl.is_expr {
            format!("{{{{ {} }}}}", tpl.text)
        } else {
            tpl.text.clone()
        };
        match analyze(&source) {
            Err(e) => d.push(Diagnostic::new(
                DiagCode::TemplateSyntax,
                tpl.pointer.clone(),
                format!("template syntax error: {e}"),
            )),
            Ok(vars) => {
                for var in vars {
                    check_var(
                        &var,
                        shape,
                        requires,
                        ancestors,
                        &tpl.pointer,
                        tpl.shell,
                        used_params,
                        d,
                    );
                }
            }
        }
        // ODIN029 — subscript access (`steps["a"]`) exposes only the bare root to the
        // analysis above, bypassing the unknown-ref / upstream checks; surface it.
        for root in subscripted_roots(&source) {
            if root == "params" {
                *params_subscripted = true;
            }
            d.push(Diagnostic::new(
                DiagCode::DynamicTemplateRef,
                tpl.pointer.clone(),
                format!(
                    "{root:?} is accessed with subscript syntax (`{root}[…]`); only dot \
                     notation (`{root}.name`) is statically checked, so an unknown or \
                     forward reference here will not be caught"
                ),
            ));
        }
        // ODIN029 — a `{% set %}` / `{% for %}` binds a new name the analysis can't follow, so a
        // reference reached *through* that name (`{% set x = steps.a %}{{ x.bogus }}`) escapes the
        // unknown-ref check. The binding's right-hand side is still checked; only paths off the new
        // name are blind. Surface it once per template.
        if binds_a_name(&source) {
            d.push(Diagnostic::new(
                DiagCode::DynamicTemplateRef,
                tpl.pointer.clone(),
                "uses a `{% set %}` / `{% for %}` binding; references reached through the \
                 introduced name are not statically checked, so an unknown or forward reference \
                 there will not be caught"
                    .to_owned(),
            ));
        }
    }
}

/// True if `source` contains a `{% set … %}` or `{% for … %}` tag (tolerating the `{%-`
/// whitespace-trim form). These introduce a binding the static ref checker can't follow.
fn binds_a_name(source: &str) -> bool {
    let mut rest = source;
    while let Some(start) = rest.find("{%") {
        let after = rest[start + 2..].trim_start_matches(['-', ' ', '\t', '\n', '\r']);
        if after.starts_with("set ") || after.starts_with("for ") {
            return true;
        }
        rest = &rest[start + 2..];
    }
    false
}

/// Checks a `loop:` body's templates against a scoped context (`loop` step at top-level index `i`).
///
/// Scope: the body's inner ids are added to a clone of the top-level `ContextShape`, so an inner
/// `steps.<id>` resolving to a sibling is known, and one resolving to a non-existent id is still
/// `ODIN017`. Visibility (the upstream check) for an inner step = its in-body ancestors ∪ the loop
/// node's own outer ancestors (steps that ran before the loop). The `until` guard runs *after* the
/// whole body, so every inner step is visible to it.
#[allow(clippy::too_many_arguments)]
fn check_loop_body(
    i: usize,
    loop_id: &StepId,
    l: &crate::ir::LoopStep,
    top_shape: &ContextShape,
    ancestors: &IndexMap<StepId, IndexSet<StepId>>,
    used_params: &mut IndexSet<String>,
    params_subscripted: &mut bool,
    d: &mut Vec<Diagnostic>,
) {
    let mut scoped = top_shape.clone();
    for inner in &l.steps {
        scoped.steps.insert(inner.id.as_str().to_owned());
    }
    // The loop node's outer ancestors (steps that completed before the loop began).
    let outer = ancestors.get(loop_id).cloned().unwrap_or_default();
    let inner_anc = crate::validate::graph::ancestor_sets(&l.steps);

    for (j, inner) in l.steps.iter().enumerate() {
        let mut visible = outer.clone();
        if let Some(a) = inner_anc.get(&inner.id) {
            visible.extend(a.iter().cloned());
        }
        let requires = require_set(inner);
        process_templates(
            collect_templates(&format!("{}.loop.steps[{j}]", step_ptr(i)), inner),
            &scoped,
            &requires,
            &visible,
            used_params,
            params_subscripted,
            d,
        );
    }

    // The `until` guard: every inner step has run, so all are visible (plus the outer ancestors).
    let mut until_visible = outer;
    for inner in &l.steps {
        until_visible.insert(inner.id.clone());
    }
    let until = Templated {
        text: l.until.clone(),
        pointer: format!("{}.loop.until", step_ptr(i)),
        is_expr: true,
        shell: false,
    };
    process_templates(
        vec![until],
        &scoped,
        &IndexSet::new(),
        &until_visible,
        used_params,
        params_subscripted,
        d,
    );
}

/// A single templated string with where it lives, whether it is a bare expression, and
/// whether it is executed by a shell (a `run:` step, a gate, or `shell.exec`'s `command`) —
/// the contexts where an interpolated untrusted `trigger.*` value is an injection risk
/// (`ODIN031`).
struct Templated {
    text: String,
    pointer: String,
    is_expr: bool,
    shell: bool,
}

/// Collects the templated strings on one step, anchored under `ptr` (its structural pointer).
/// `ptr` is a prefix rather than a step index so this serves both a top-level step (`steps[i]`)
/// and a `loop:` body step (`steps[i].loop.steps[j]`). A loop step's own `until` is collected by
/// the caller (it sees the whole body), so the [`StepKind::Loop`] arm contributes nothing here.
fn collect_templates(ptr: &str, s: &crate::ir::Step) -> Vec<Templated> {
    let mut out = Vec::new();
    let mut push = |text: String, pointer: String, is_expr: bool, shell: bool| {
        out.push(Templated {
            text,
            pointer,
            is_expr,
            shell,
        });
    };
    match &s.kind {
        StepKind::Provider(p) => {
            if let Some(t) = &p.prompt {
                push(t.clone(), format!("{ptr}.prompt"), false, false);
            }
            if let Some(pf) = &p.prompt_file {
                push(pf.clone(), format!("{ptr}.prompt_file"), false, false);
            }
        }
        StepKind::Action(a) => {
            for (k, v) in &a.with {
                if let Some(sv) = v.as_str() {
                    // `shell.exec`'s `command` arg is run via `sh -c`; other action args are
                    // passed structurally, not through a shell.
                    let shell = a.action == "shell.exec" && k == "command";
                    push(sv.to_owned(), format!("{ptr}.with.{k}"), false, shell);
                }
            }
        }
        StepKind::Run(r) => push(r.run.clone(), format!("{ptr}.run"), false, true),
        StepKind::Approval(a) => {
            // The approver message is templated (it can surface `{{ steps… }}` context to the
            // human) but is shown in a UI, never a shell — so it is not an injection sink.
            if let Some(msg) = &a.message {
                push(msg.clone(), format!("{ptr}.approval.message"), false, false);
            }
        }
        StepKind::Case(c) => {
            // Branch guards are bare boolean expressions, like `when:`, checked the same way.
            for (bi, b) in c.branches.iter().enumerate() {
                if let Some(w) = &b.when {
                    push(
                        w.clone(),
                        format!("{ptr}.case.branches[{bi}].when"),
                        true,
                        false,
                    );
                }
            }
        }
        // The loop's `until` and its inner steps are checked by the caller against a scoped
        // context (inner ids + the loop node's outer upstreams).
        StepKind::Loop(_) => {}
    }
    for (name, cmd) in &s.gates {
        push(cmd.clone(), format!("{ptr}.gates.{name}"), false, true);
    }
    if let Some(j) = &s.judge {
        push(
            j.criteria.clone(),
            format!("{ptr}.judge.criteria"),
            false,
            false,
        );
    }
    if let Some(w) = &s.when {
        push(w.clone(), format!("{ptr}.when"), true, false);
    }
    out
}

/// Compiles `source` and returns the set of (possibly dotted) variable paths it uses.
/// A compile error becomes `ODIN018`.
fn analyze(source: &str) -> Result<HashSet<String>, minijinja::Error> {
    let mut env = minijinja::Environment::new();
    env.add_template_owned("__odin_check", source.to_owned())?;
    let tmpl = env
        .get_template("__odin_check")
        .expect("template was just added");
    Ok(tmpl.undeclared_variables(true))
}

#[allow(clippy::too_many_arguments)]
fn check_var(
    var: &str,
    shape: &ContextShape,
    requires: &IndexSet<&str>,
    ancestors: &IndexSet<StepId>,
    pointer: &str,
    shell: bool,
    used_params: &mut IndexSet<String>,
    d: &mut Vec<Diagnostic>,
) {
    let mut segs = var.split('.');
    let root = segs.next().unwrap_or_default();
    let second = segs.next();
    // ODIN031 — an untrusted `trigger.*` value flowing into a shell command (`run:`, a gate,
    // or `shell.exec`'s `command`) is an injection risk; a webhook payload reaches `sh -c`
    // unescaped. Warn regardless of how `trigger`'s children resolve (they aren't modeled).
    if root == "trigger" && shell {
        d.push(Diagnostic::new(
            DiagCode::TriggerIntoShell,
            pointer.to_owned(),
            "interpolates an untrusted trigger payload (`trigger.*`) into a shell command; \
             a webhook-supplied value reaches `sh -c` unescaped (injection risk). Map the \
             fields you trust into typed params and reference those instead"
                .to_owned(),
        ));
    }
    match root {
        "params" => {
            if let Some(name) = second {
                used_params.insert(name.to_owned());
                if !shape.params.contains(name) {
                    let mut diag = unknown_ref(pointer, &format!("param {name:?}"));
                    if let Some(sg) = suggest(name, shape.params.iter().map(String::as_str)) {
                        diag = diag.with_help(format!("did you mean {sg:?}?"));
                    }
                    d.push(diag);
                }
            }
        }
        "steps" => {
            if let Some(name) = second {
                if !shape.steps.contains(name) {
                    let mut diag = unknown_ref(pointer, &format!("step {name:?}"));
                    if let Some(sg) = suggest(name, shape.steps.iter().map(String::as_str)) {
                        diag = diag.with_help(format!("did you mean {sg:?}?"));
                    }
                    d.push(diag);
                } else if !ancestors.iter().any(|a| a.as_str() == name) {
                    d.push(Diagnostic::new(
                        DiagCode::UnknownTemplateRef,
                        pointer.to_owned(),
                        format!(
                            "references step {name:?} which is not an upstream dependency (add it to depends_on)"
                        ),
                    ));
                } else if let Some(field) = segs.next() {
                    // The step id is a known upstream dependency; validate the leaf accessor so a
                    // typo like `steps.a.exitcode` is caught (only `outputs`/`exit_code`/`status`
                    // exist). `outputs`' children are dynamic, so a 4th+ segment is not checked.
                    if !STEP_FIELDS.contains(&field) {
                        let mut diag =
                            unknown_ref(pointer, &format!("field {field:?} on step {name:?}"));
                        diag = match suggest(field, STEP_FIELDS.iter().copied()) {
                            Some(sg) => diag.with_help(format!(
                                "did you mean {sg:?}? a step exposes: outputs, exit_code, status"
                            )),
                            None => diag
                                .with_help("a step exposes: outputs, exit_code, status".to_owned()),
                        };
                        d.push(diag);
                    }
                }
            }
        }
        "artifacts" => {
            if let Some(name) = second {
                if name != DIFF && !requires.contains(name) {
                    d.push(Diagnostic::new(
                        DiagCode::UnknownTemplateRef,
                        pointer.to_owned(),
                        format!(
                            "references artifact {name:?} not in this step's requires (add it to artifacts.requires)"
                        ),
                    ));
                }
            }
        }
        r if OPEN_ROOTS.contains(&r) => { /* allowed; children not modeled */ }
        r if MINIJINJA_GLOBALS.contains(&r) => { /* a built-in minijinja function, not a variable */
        }
        other => {
            // Derive the hint from the actual root sets so it can't drift when a root is added.
            let roots = CHECKED_ROOTS
                .iter()
                .chain(OPEN_ROOTS)
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            d.push(
                unknown_ref(pointer, &format!("{other:?}"))
                    .with_help(format!("valid roots: {roots}")),
            );
        }
    }
}

/// Statically-checked roots whose subscript access (`root[…]`) bypasses [`check_var`].
const CHECKED_ROOTS: &[&str] = &["params", "steps", "artifacts"];

/// Returns the statically-checked roots that are accessed with subscript syntax (`root[…]`)
/// inside any `{{ … }}` expression of `source`. Only the bodies are scanned, so a literal
/// `arr[steps]` in surrounding shell text is not mistaken for a template subscript.
fn subscripted_roots(source: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        // Best-effort body extraction: a `}}` *inside* a string literal (e.g. `{{ '}}' }}`)
        // truncates `body` early, so a subscript after it can be missed. This only ever
        // under-reports a best-effort warning (ODIN029); the real parse (`analyze`, which
        // drives ODIN017/018/031) uses minijinja on the full source and is unaffected.
        let Some(end) = after.find("}}") else { break };
        let body = &after[..end];
        let bytes = body.as_bytes();
        // Bytes inside a `'…'` / `"…"` string literal — a `steps[…]` *inside* a quoted string
        // (e.g. `{{ lookup("steps[0]") }}`) is data, not a subscript, so it must not match.
        let in_string = string_mask(bytes);
        for &root in CHECKED_ROOTS {
            if found.contains(&root) {
                continue;
            }
            let mut from = 0;
            while let Some(pos) = body[from..].find(root) {
                let idx = from + pos;
                // The match is the path *root* only if the preceding non-space byte is neither
                // an identifier byte (`mysteps`) nor `.` (a nested attribute like
                // `trigger.steps` or `out.params`) — otherwise it's a deeper key, not a root.
                let mut b = idx;
                while b > 0 && bytes[b - 1].is_ascii_whitespace() {
                    b -= 1;
                }
                let at_root = b == 0 || (bytes[b - 1] != b'.' && !is_ident_byte(bytes[b - 1]));
                // ...and it's subscripted iff the next non-space byte after the name is `[`.
                let mut j = idx + root.len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let real = at_root
                    && bytes.get(j) == Some(&b'[')
                    && !in_string[idx]
                    && !in_string.get(j).copied().unwrap_or(false);
                if real {
                    found.push(root);
                    break;
                }
                from = idx + root.len();
            }
        }
        rest = &after[end + 2..];
    }
    found
}

/// Marks each byte of a `{{ … }}` expression body that lies inside a `'…'` or `"…"` string
/// literal, so a path-looking substring inside a quoted string isn't read as a real reference.
/// A backslash escapes the next byte within a string.
fn string_mask(bytes: &[u8]) -> Vec<bool> {
    let mut mask = vec![false; bytes.len()];
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                mask[i] = true;
                if c == b'\\' && i + 1 < bytes.len() {
                    mask[i + 1] = true; // the escaped byte is still inside the string
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == b'\'' || c == b'"' {
                    quote = Some(c);
                    mask[i] = true;
                }
            }
        }
        i += 1;
    }
    mask
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn unknown_ref(pointer: &str, what: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::UnknownTemplateRef,
        pointer.to_owned(),
        format!("template references unknown {what}"),
    )
}

#[cfg(test)]
mod tests {
    use crate::ir::Workflow;
    use crate::validate::diagnostic::DiagCode;
    use crate::validate::graph::ancestor_sets;

    fn check(yaml: &str) -> Vec<crate::validate::diagnostic::Diagnostic> {
        let wf = Workflow::from_yaml_str(yaml).unwrap();
        let anc = ancestor_sets(&wf.steps);
        let mut d = Vec::new();
        super::check(&wf, &anc, &mut d);
        d
    }

    #[test]
    fn unknown_param_ref_is_flagged() {
        let d = check(
            "name: x\nparams:\n  foo: {}\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ params.bar }}\"}\n",
        );
        assert!(d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef));
    }

    #[test]
    fn known_param_ref_passes() {
        let d = check(
            "name: x\nparams:\n  foo: {}\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ params.foo }}\"}\n",
        );
        assert!(!d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef));
    }

    #[test]
    fn non_upstream_step_ref_is_flagged() {
        // `b` references `a` but does not depend on it.
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: \"echo {{ steps.a.outputs.x }}\"}\n",
        );
        assert!(d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef));
    }

    #[test]
    fn upstream_step_ref_passes() {
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: \"echo {{ steps.a.outputs.x }}\", depends_on: [a]}\n",
        );
        assert!(!d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef));
    }

    #[test]
    fn minijinja_global_is_not_an_unknown_ref() {
        // `range`/`dict`/`namespace`/`debug` are built-in minijinja functions, not variables —
        // calling one must not trip ODIN017.
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ range(3) }} {{ dict(k=1) }} {{ namespace(x=1) }}\"}\n",
        );
        assert!(
            !d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef),
            "minijinja globals must not be flagged: {d:?}"
        );
    }

    #[test]
    fn typoed_step_field_is_flagged_with_a_suggestion() {
        // `a` IS an upstream dependency, so the id resolves; the leaf `exitcode` (should be
        // `exit_code`) is the bug, and it must now be caught.
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: \"echo {{ steps.a.exitcode }}\", depends_on: [a]}\n",
        );
        let diag = d
            .iter()
            .find(|x| x.code == DiagCode::UnknownTemplateRef)
            .expect("a typoed step field must be flagged");
        assert!(
            diag.help
                .as_deref()
                .unwrap_or_default()
                .contains("exit_code"),
            "help should suggest exit_code: {:?}",
            diag.help
        );
    }

    #[test]
    fn valid_step_fields_pass() {
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: \"{{ steps.a.exit_code }} {{ steps.a.status }} {{ steps.a.outputs.stdout }}\", depends_on: [a]}\n",
        );
        assert!(
            !d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef),
            "outputs/exit_code/status are all valid: {d:?}"
        );
    }

    #[test]
    fn set_or_for_binding_warns() {
        // A `{% set %}` binding hides references reached through the new name — warn (ODIN029).
        let d = check(
            "name: x\nparams:\n  x: {}\nsteps:\n  - {id: a, provider: claude, prompt: \"{% set s = params.x %}{{ s.bogus }}\"}\n",
        );
        assert!(
            d.iter().any(|x| x.code == DiagCode::DynamicTemplateRef),
            "a set/for binding must raise ODIN029: {d:?}"
        );
    }

    #[test]
    fn syntax_error_is_flagged() {
        let d = check("name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ unclosed \"}\n");
        assert!(d.iter().any(|x| x.code == DiagCode::TemplateSyntax));
    }

    #[test]
    fn diff_artifact_is_always_allowed() {
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ artifacts.DIFF }}\"}\n",
        );
        assert!(!d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef));
    }

    #[test]
    fn unused_param_warns() {
        let d = check("name: x\nparams:\n  foo: {}\nsteps:\n  - {id: a, run: ./x.sh}\n");
        assert!(d.iter().any(|x| x.code == DiagCode::UnusedParam));
    }

    #[test]
    fn trigger_refs_are_always_allowed() {
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ trigger.issue.number }}\"}\n",
        );
        assert!(!d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef));
    }

    #[test]
    fn non_required_artifact_ref_is_flagged() {
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ artifacts.FOO }}\"}\n",
        );
        assert!(d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef));
    }

    #[test]
    fn required_artifact_ref_passes() {
        let d = check(
            "name: x\nsteps:\n  - {id: p, provider: claude, prompt: hi, artifacts: {produces: [FOO]}}\n  - {id: a, run: \"echo {{ artifacts.FOO }}\", depends_on: [p], artifacts: {requires: [FOO]}}\n",
        );
        assert!(!d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef));
    }

    #[test]
    fn trigger_interpolated_into_a_run_step_warns() {
        // ODIN031 — untrusted `trigger.*` reaching `sh -c` in a `run:` step.
        let d = check("name: x\nsteps:\n  - {id: a, run: \"echo {{ trigger.issue.title }}\"}\n");
        assert!(d.iter().any(|x| x.code == DiagCode::TriggerIntoShell));
    }

    #[test]
    fn trigger_in_a_prompt_does_not_warn() {
        // A prompt is fed to the agent, not a shell — no injection, no ODIN031.
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ trigger.issue.title }}\"}\n",
        );
        assert!(!d.iter().any(|x| x.code == DiagCode::TriggerIntoShell));
    }

    #[test]
    fn param_used_only_via_subscript_is_not_unused() {
        // `params['foo']` references foo dynamically: ODIN024 must be suppressed (we can't
        // attribute the key), though ODIN029 still flags the un-checkable subscript.
        let d = check(
            "name: x\nparams:\n  foo: {}\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ params['foo'] }}\"}\n",
        );
        assert!(
            !d.iter().any(|x| x.code == DiagCode::UnusedParam),
            "a subscript use must suppress ODIN024:\n{d:?}"
        );
        assert!(d.iter().any(|x| x.code == DiagCode::DynamicTemplateRef));
    }

    #[test]
    fn subscript_inside_a_string_literal_is_not_flagged() {
        // A quoted `steps[0]` is data, not a subscript — ODIN029 must not fire.
        let d =
            check("name: x\nsteps:\n  - {id: a, provider: claude, prompt: \"{{ 'steps[0]' }}\"}\n");
        assert!(
            !d.iter().any(|x| x.code == DiagCode::DynamicTemplateRef),
            "a quoted literal must not be read as a subscript:\n{d:?}"
        );
    }

    #[test]
    fn a_real_subscript_is_still_flagged() {
        let d = check(
            "name: x\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: \"echo {{ steps['a'].outputs.x }}\", depends_on: [a]}\n",
        );
        assert!(d.iter().any(|x| x.code == DiagCode::DynamicTemplateRef));
    }

    // ── scoped template checking inside a loop: body ─────────────────────────────

    fn has_unknown_ref(d: &[crate::validate::diagnostic::Diagnostic]) -> bool {
        d.iter().any(|x| x.code == DiagCode::UnknownTemplateRef)
    }

    #[test]
    fn loop_until_referencing_an_inner_step_passes() {
        // `until` runs after the body, so every inner step (here `test`) is visible to it.
        let d = check(
            "name: x\nsteps:\n  - id: f\n    loop:\n      until: \"steps.test.status == 'passed'\"\n      max: 3\n      steps:\n        - {id: edit, run: \"echo edit\"}\n        - {id: test, run: \"echo test\", depends_on: [edit]}\n",
        );
        assert!(!has_unknown_ref(&d), "{d:?}");
    }

    #[test]
    fn loop_until_referencing_an_unknown_step_is_flagged() {
        let d = check(
            "name: x\nsteps:\n  - id: f\n    loop:\n      until: \"steps.ghost.status == 'passed'\"\n      max: 2\n      steps:\n        - {id: edit, run: \"echo hi\"}\n",
        );
        assert!(has_unknown_ref(&d), "{d:?}");
    }

    #[test]
    fn loop_inner_ref_to_an_upstream_sibling_passes() {
        let d = check(
            "name: x\nsteps:\n  - id: f\n    loop:\n      until: \"true\"\n      max: 2\n      steps:\n        - {id: a, run: \"echo hi\"}\n        - {id: b, run: \"echo {{ steps.a.outputs.stdout }}\", depends_on: [a]}\n",
        );
        assert!(!has_unknown_ref(&d), "{d:?}");
    }

    #[test]
    fn loop_inner_ref_to_a_non_upstream_sibling_is_flagged() {
        // `b` references sibling `a` without depending on it — visible-but-not-upstream.
        let d = check(
            "name: x\nsteps:\n  - id: f\n    loop:\n      until: \"true\"\n      max: 2\n      steps:\n        - {id: a, run: \"echo hi\"}\n        - {id: b, run: \"echo {{ steps.a.outputs.stdout }}\"}\n",
        );
        assert!(has_unknown_ref(&d), "{d:?}");
    }

    #[test]
    fn loop_inner_ref_to_an_unknown_step_is_flagged() {
        let d = check(
            "name: x\nsteps:\n  - id: f\n    loop:\n      until: \"true\"\n      max: 2\n      steps:\n        - {id: edit, run: \"echo {{ steps.ghost.outputs.x }}\"}\n",
        );
        assert!(has_unknown_ref(&d), "{d:?}");
    }

    #[test]
    fn loop_inner_ref_to_an_outer_ancestor_passes() {
        // An inner step may read an outer step the LOOP depends on (it ran before the loop).
        let d = check(
            "name: x\nsteps:\n  - {id: setup, provider: claude, prompt: hi}\n  - id: f\n    depends_on: [setup]\n    loop:\n      until: \"true\"\n      max: 2\n      steps:\n        - {id: edit, run: \"echo {{ steps.setup.outputs.stdout }}\"}\n",
        );
        assert!(!has_unknown_ref(&d), "{d:?}");
    }

    #[test]
    fn loop_counter_and_feedback_resolve() {
        let d = check(
            "name: x\nsteps:\n  - id: f\n    loop:\n      until: \"true\"\n      max: 2\n      steps:\n        - {id: edit, provider: claude, prompt: \"try {{ loop.counter }}: {{ loop.feedback }}\"}\n",
        );
        assert!(!has_unknown_ref(&d), "loop.* is an open root: {d:?}");
    }

    #[test]
    fn param_used_only_in_a_loop_body_is_not_unused() {
        let d = check(
            "name: x\nparams:\n  foo: {}\nsteps:\n  - id: f\n    loop:\n      until: \"true\"\n      max: 2\n      steps:\n        - {id: edit, provider: claude, prompt: \"{{ params.foo }}\"}\n",
        );
        assert!(
            !d.iter().any(|x| x.code == DiagCode::UnusedParam),
            "a param referenced only inside a loop body is used: {d:?}"
        );
    }

    #[test]
    fn loop_inner_template_syntax_error_is_flagged() {
        let d = check(
            "name: x\nsteps:\n  - id: f\n    loop:\n      until: \"true\"\n      max: 2\n      steps:\n        - {id: edit, provider: claude, prompt: \"{{ unclosed \"}\n",
        );
        assert!(
            d.iter().any(|x| x.code == DiagCode::TemplateSyntax),
            "{d:?}"
        );
    }
}
