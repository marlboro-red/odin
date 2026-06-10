//! Run-time rendering of prompts/arguments and evaluation of `when:` conditionals.
//!
//! These power the executor: it assembles a context value from the run state and calls
//! [`render_template`] for prompts/`with` values and [`eval_when`] for conditionals.
//! Undefined references are *errors* (`UndefinedBehavior::Strict`), matching the static
//! checker, so a typo fails loudly rather than rendering an empty string.

use minijinja::{Environment, UndefinedBehavior, Value};
use serde::Serialize;

use crate::error::{Error, Result};

/// Builds a minijinja context [`Value`] from any serializable structure (typically a
/// `serde_json` object assembled from the run state: `params`, `trigger`, `steps`,
/// `artifacts`).
pub fn build_context<T: Serialize>(value: &T) -> Value {
    Value::from_serialize(value)
}

fn strict_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    // `shquote`: shell-quote an untrusted value for safe interpolation into a `sh -c` command.
    env.add_filter("shquote", shquote);
    env
}

/// Shell-quotes a value for safe interpolation into a POSIX `sh -c` command: wraps it in single
/// quotes and escapes any embedded single quote (`'` -> `'\''`), so an attacker-influenced value —
/// a webhook payload field mapped into a param, or a step's raw agent stdout — becomes one inert
/// shell *word* instead of executable syntax. Use it on any untrusted value in a `run:`/gate
/// command, e.g. `run: "echo {{ params.title | shquote }}"`.
fn shquote(value: &minijinja::Value) -> String {
    let s = value.to_string();
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Renders a template string against `ctx`.
///
/// `what` describes the template's origin (e.g. `step "review" prompt`) and is woven
/// into any [`Error::Template`].
///
/// # Errors
/// Returns [`Error::Template`] on a compile error or an undefined reference.
pub fn render_template(tpl: &str, ctx: &Value, what: &str) -> Result<String> {
    let mut env = strict_env();
    env.add_template_owned("__odin_render", tpl.to_owned())
        .map_err(|source| Error::Template {
            context: what.to_owned(),
            source,
        })?;
    let compiled = env
        .get_template("__odin_render")
        .expect("template was just added");
    compiled.render(ctx).map_err(|source| Error::Template {
        context: what.to_owned(),
        source,
    })
}

/// Evaluates a `when:` expression to a boolean using minijinja truthiness
/// (`false`/`0`/`""`/`none`/empty-collection are falsy). An empty expression is `true`.
///
/// # Errors
/// Returns [`Error::Template`] on a compile error or an undefined reference.
pub fn eval_when(expr: &str, ctx: &Value) -> Result<bool> {
    if expr.trim().is_empty() {
        return Ok(true);
    }
    let env = strict_env();
    let compiled = env
        .compile_expression(expr)
        .map_err(|source| Error::Template {
            context: "when".to_owned(),
            source,
        })?;
    let value = compiled.eval(ctx).map_err(|source| Error::Template {
        context: "when".to_owned(),
        source,
    })?;
    Ok(value.is_true())
}

#[cfg(test)]
mod tests {
    use super::{build_context, eval_when, render_template};
    use serde_json::json;

    #[test]
    fn renders_with_context() {
        let ctx = build_context(&json!({ "params": { "n": 42 } }));
        let out = render_template("issue #{{ params.n }}", &ctx, "test").unwrap();
        assert_eq!(out, "issue #42");
    }

    #[test]
    fn undefined_reference_is_an_error() {
        let ctx = build_context(&json!({ "params": {} }));
        assert!(render_template("{{ params.missing }}", &ctx, "test").is_err());
    }

    #[test]
    fn shquote_neutralizes_shell_metacharacters() {
        let ctx = build_context(&json!({ "params": { "title": "$(rm -rf /); `whoami`" } }));
        let out = render_template("echo {{ params.title | shquote }}", &ctx, "test").unwrap();
        // The whole value is wrapped in single quotes, so nothing is a command substitution.
        assert_eq!(out, "echo '$(rm -rf /); `whoami`'");

        // An embedded single quote is escaped so it can't terminate the quoting and break out.
        let ctx = build_context(&json!({ "params": { "x": "a'b; rm -rf /" } }));
        let out = render_template("echo {{ params.x | shquote }}", &ctx, "test").unwrap();
        assert_eq!(out, r"echo 'a'\''b; rm -rf /'");
    }

    #[test]
    fn evaluates_when() {
        let ctx =
            build_context(&json!({ "steps": { "a": { "exit_code": 0, "status": "passed" } } }));
        assert!(eval_when("steps.a.exit_code == 0", &ctx).unwrap());
        assert!(!eval_when("steps.a.exit_code == 1", &ctx).unwrap());
        // The documented status-gate idiom: status is a snake_case string.
        assert!(eval_when("steps.a.status == 'passed'", &ctx).unwrap());
        assert!(!eval_when("steps.a.status == 'failed'", &ctx).unwrap());
        assert!(eval_when("  ", &ctx).unwrap(), "empty when is true");
    }
}
