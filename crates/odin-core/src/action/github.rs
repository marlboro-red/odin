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

        let mut args = vec!["pr", "create", "--title", title, "--body", body.as_str()];
        if let Some(base) = ctx.args.get("base").and_then(Value::as_str) {
            args.push("--base");
            args.push(base);
        }
        if let Some(head) = ctx.args.get("head").and_then(Value::as_str) {
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

        // gh prints the PR URL on stdout; the number is its last path segment.
        let url = out.stdout.lines().last().unwrap_or("").trim().to_owned();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let mut outputs = IndexMap::new();
        outputs.insert("url".to_owned(), Value::String(url.clone()));
        outputs.insert("number".to_owned(), Value::Number(number.into()));
        Ok(ActionOutcome {
            exit_code: 0,
            outputs,
            side_effects: vec![SideEffect::PullRequest { url, number }],
        })
    }
}
