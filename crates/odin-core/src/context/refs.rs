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

/// Roots that are always allowed but whose children are not statically modeled.
const OPEN_ROOTS: &[&str] = &["trigger", "run"];

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
        let requires: IndexSet<&str> = s
            .artifacts
            .requires
            .iter()
            .map(ArtifactName::as_str)
            .collect();
        let anc = ancestors.get(&s.id).unwrap_or(&empty);

        for tpl in collect_templates(i, s) {
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
                            &shape,
                            &requires,
                            anc,
                            &tpl.pointer,
                            tpl.shell,
                            &mut used_params,
                            d,
                        );
                    }
                }
            }
            // ODIN029 — subscript access (`steps["a"]`) exposes only the bare root to the
            // analysis above, bypassing the unknown-ref / upstream checks; surface it.
            for root in subscripted_roots(&source) {
                if root == "params" {
                    params_subscripted = true;
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

fn collect_templates(i: usize, s: &crate::ir::Step) -> Vec<Templated> {
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
                push(t.clone(), format!("{}.prompt", step_ptr(i)), false, false);
            }
            if let Some(pf) = &p.prompt_file {
                push(
                    pf.clone(),
                    format!("{}.prompt_file", step_ptr(i)),
                    false,
                    false,
                );
            }
        }
        StepKind::Action(a) => {
            for (k, v) in &a.with {
                if let Some(sv) = v.as_str() {
                    // `shell.exec`'s `command` arg is run via `sh -c`; other action args are
                    // passed structurally, not through a shell.
                    let shell = a.action == "shell.exec" && k == "command";
                    push(
                        sv.to_owned(),
                        format!("{}.with.{k}", step_ptr(i)),
                        false,
                        shell,
                    );
                }
            }
        }
        StepKind::Run(r) => push(r.run.clone(), format!("{}.run", step_ptr(i)), false, true),
    }
    for (name, cmd) in &s.gates {
        push(
            cmd.clone(),
            format!("{}.gates.{name}", step_ptr(i)),
            false,
            true,
        );
    }
    if let Some(j) = &s.judge {
        push(
            j.criteria.clone(),
            format!("{}.judge.criteria", step_ptr(i)),
            false,
            false,
        );
    }
    if let Some(w) = &s.when {
        push(w.clone(), format!("{}.when", step_ptr(i)), true, false);
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
        other => {
            d.push(
                unknown_ref(pointer, &format!("{other:?}"))
                    .with_help("valid roots: params, trigger, steps, artifacts, run".to_owned()),
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
        let anc = ancestor_sets(&wf);
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
}
