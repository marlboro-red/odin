//! Static checking of template references (`ODIN017`/`ODIN018`) and unused params
//! (`ODIN024`).
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
                            &mut used_params,
                            d,
                        );
                    }
                }
            }
        }
    }

    // ODIN024 — declared but never referenced (inline templates only; prompt_file
    // contents are not loaded, so a param used only there is not counted).
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

/// A single templated string with where it lives and whether it is a bare expression.
struct Templated {
    text: String,
    pointer: String,
    is_expr: bool,
}

fn collect_templates(i: usize, s: &crate::ir::Step) -> Vec<Templated> {
    let mut out = Vec::new();
    let mut push = |text: String, pointer: String, is_expr: bool| {
        out.push(Templated {
            text,
            pointer,
            is_expr,
        });
    };
    match &s.kind {
        StepKind::Provider(p) => {
            if let Some(t) = &p.prompt {
                push(t.clone(), format!("{}.prompt", step_ptr(i)), false);
            }
            if let Some(pf) = &p.prompt_file {
                push(pf.clone(), format!("{}.prompt_file", step_ptr(i)), false);
            }
        }
        StepKind::Action(a) => {
            for (k, v) in &a.with {
                if let Some(sv) = v.as_str() {
                    push(sv.to_owned(), format!("{}.with.{k}", step_ptr(i)), false);
                }
            }
        }
        StepKind::Run(r) => push(r.run.clone(), format!("{}.run", step_ptr(i)), false),
    }
    for (name, cmd) in &s.gates {
        push(cmd.clone(), format!("{}.gates.{name}", step_ptr(i)), false);
    }
    if let Some(j) = &s.judge {
        push(
            j.criteria.clone(),
            format!("{}.judge.criteria", step_ptr(i)),
            false,
        );
    }
    if let Some(w) = &s.when {
        push(w.clone(), format!("{}.when", step_ptr(i)), true);
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

fn check_var(
    var: &str,
    shape: &ContextShape,
    requires: &IndexSet<&str>,
    ancestors: &IndexSet<StepId>,
    pointer: &str,
    used_params: &mut IndexSet<String>,
    d: &mut Vec<Diagnostic>,
) {
    let mut segs = var.split('.');
    let root = segs.next().unwrap_or_default();
    let second = segs.next();
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
}
