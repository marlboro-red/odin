//! The `github.open_pr` action: open a pull request with the `gh` CLI.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use crate::api::SideEffect;
use crate::error::ActionError;
use crate::traits::{Action, ActionCtx, ActionOutcome};

/// `github.open_pr` — runs `gh pr create` in the workspace.
///
/// Args: `title` (required), `body` (default empty), `base`/`head` (optional; default to
/// the repo default branch / current branch). Requires `gh` on `PATH`, an authenticated
/// session, a GitHub remote, and the branch already pushed (use `git.push` first).
pub struct OpenPr;

#[async_trait]
impl Action for OpenPr {
    // The trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "github.open_pr"
    }

    async fn run(&self, ctx: ActionCtx) -> Result<ActionOutcome, ActionError> {
        let title = super::arg_str(&ctx.args, "title")?;
        let body = super::arg_str_or(&ctx.args, "body", "");

        // `base`/`head` are often templated from params/trigger payloads. A value beginning
        // with `-` would be parsed by `gh` as another flag, not a branch — argument injection.
        // A real git ref can't start with `-` anyway, so reject it outright.
        let base = ctx.args.get("base").and_then(Value::as_str);
        let head = ctx.args.get("head").and_then(Value::as_str);
        for (name, value) in [("base", base), ("head", head)] {
            if let Some(v) = value {
                if v.starts_with('-') {
                    return Err(ActionError::Other(anyhow::anyhow!(
                        "github.open_pr {name} {v:?} is not a valid branch (must not start with '-')"
                    )));
                }
            }
        }

        let mut args = vec!["pr", "create", "--title", title, "--body", body.as_str()];
        if let Some(base) = base {
            args.push("--base");
            args.push(base);
        }
        if let Some(head) = head {
            args.push("--head");
            args.push(head);
        }

        let out = super::exec("gh", &args, &ctx.workdir).await?;
        if out.exit_code != 0 {
            return Err(ActionError::Other(anyhow::anyhow!(
                "gh pr create failed: {}",
                out.stderr.trim()
            )));
        }

        // gh prints the PR URL on its last non-empty stdout line; the number is the last
        // non-empty path segment (tolerant of a trailing slash).
        let url = out
            .stdout
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_owned();
        let number = url
            .rsplit('/')
            .find(|s| !s.is_empty())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let mut outputs = IndexMap::new();
        outputs.insert("url".to_owned(), Value::String(url.clone()));
        outputs.insert("number".to_owned(), Value::Number(number.into()));
        Ok(ActionOutcome {
            exit_code: 0,
            outputs,
            side_effects: vec![SideEffect::pull_request(url, number)],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::OpenPr;
    use crate::ids::StepId;
    use crate::traits::{Action, ActionCtx};
    use indexmap::IndexMap;
    use serde_json::json;

    fn ctx(args: IndexMap<String, serde_json::Value>) -> ActionCtx {
        ActionCtx {
            step_id: StepId::new("pr"),
            workdir: std::env::temp_dir(),
            args,
        }
    }

    #[tokio::test]
    async fn rejects_a_base_that_looks_like_a_flag() {
        // A `-`-leading branch (e.g. from a templated payload) would be parsed by `gh` as a
        // flag. The guard must reject it before `gh` is ever spawned.
        let mut args = IndexMap::new();
        args.insert("title".to_owned(), json!("t"));
        args.insert("base".to_owned(), json!("--version"));
        let err = OpenPr.run(ctx(args)).await.unwrap_err();
        assert!(
            err.to_string().contains("base"),
            "expected a base-rejection error, got: {err}"
        );
    }

    #[tokio::test]
    async fn rejects_a_head_that_looks_like_a_flag() {
        let mut args = IndexMap::new();
        args.insert("title".to_owned(), json!("t"));
        args.insert("head".to_owned(), json!("-X"));
        let err = OpenPr.run(ctx(args)).await.unwrap_err();
        assert!(
            err.to_string().contains("head"),
            "expected a head-rejection error, got: {err}"
        );
    }
}
