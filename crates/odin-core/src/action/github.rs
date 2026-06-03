//! The `github.open_pr` action: open a pull request with the `gh` CLI.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use crate::api::SideEffect;
use crate::error::ActionError;
use crate::traits::{Action, ActionCtx, ActionOutcome};

/// `github.open_pr` — opens a pull request with the `gh` CLI.
///
/// Args: `title` (required), `body` (default empty), `base`/`head` (optional; default to
/// the repo default branch / current branch). Requires `gh` on `PATH`, an authenticated
/// session, a GitHub remote, and the branch already pushed (use `git.push` first).
///
/// **Idempotent:** before creating, it checks for an existing open PR on the head branch and
/// returns that one if found. So a crash-resumed run (or any re-invocation) that already opened
/// the PR reattaches to it instead of failing with "a pull request already exists" or opening a
/// duplicate — preserving the durability contract across the action's non-atomic boundary.
pub struct OpenPr;

/// Parses `gh pr list --json number,url` output, returning the first PR's `(number, url)`.
fn first_pr(stdout: &str) -> Option<(u64, String)> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let first = value.as_array()?.first()?;
    let number = first.get("number")?.as_u64()?;
    let url = first.get("url")?.as_str()?.to_owned();
    Some((number, url))
}

/// The current branch name in `workdir`, or `None` if detached/unresolvable.
async fn current_branch(workdir: &std::path::Path) -> Option<String> {
    let out = super::exec("git", &["rev-parse", "--abbrev-ref", "HEAD"], workdir)
        .await
        .ok()?;
    if out.exit_code != 0 {
        return None;
    }
    let branch = out.stdout.trim();
    // "HEAD" means a detached checkout — there is no branch to key a PR on.
    (!branch.is_empty() && branch != "HEAD").then(|| branch.to_owned())
}

/// Builds the action outcome for a PR `(number, url)` — same shape for a found or created PR.
fn pr_outcome(number: u64, url: String) -> ActionOutcome {
    let mut outputs = IndexMap::new();
    outputs.insert("url".to_owned(), Value::String(url.clone()));
    outputs.insert("number".to_owned(), Value::Number(number.into()));
    ActionOutcome {
        exit_code: 0,
        outputs,
        side_effects: vec![SideEffect::pull_request(url, number)],
    }
}

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

        // Idempotency: if an open PR already exists on the head branch (e.g. a prior attempt of
        // this run opened it, then crashed before checkpointing), reattach to it rather than
        // re-creating. Best-effort — a failed/empty query just falls through to create.
        let head_branch = match head {
            Some(h) => Some(h.to_owned()),
            None => current_branch(&ctx.workdir).await,
        };
        if let Some(branch) = &head_branch {
            let list = [
                "pr",
                "list",
                "--head",
                branch,
                "--state",
                "open",
                "--json",
                "number,url",
                "--limit",
                "1",
            ];
            if let Ok(out) = super::exec("gh", &list, &ctx.workdir).await {
                if out.exit_code == 0 {
                    if let Some((number, url)) = first_pr(&out.stdout) {
                        return Ok(pr_outcome(number, url));
                    }
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

        Ok(pr_outcome(number, url))
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenPr, first_pr};
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

    #[test]
    fn first_pr_parses_an_existing_pr() {
        // The idempotency query `gh pr list --json number,url` returns a JSON array.
        let (n, url) =
            first_pr(r#"[{"number":42,"url":"https://github.com/o/r/pull/42"}]"#).unwrap();
        assert_eq!(n, 42);
        assert_eq!(url, "https://github.com/o/r/pull/42");
        // Multiple PRs: take the first (the query is --limit 1, but be tolerant).
        let (n2, _) = first_pr(
            r#"[{"number":7,"url":"https://github.com/o/r/pull/7"},{"number":9,"url":"x"}]"#,
        )
        .unwrap();
        assert_eq!(n2, 7);
    }

    #[test]
    fn first_pr_is_none_for_empty_or_malformed() {
        assert!(first_pr("[]").is_none(), "no open PR → create one");
        assert!(first_pr("").is_none());
        assert!(first_pr("not json").is_none());
        assert!(
            first_pr(r#"[{"url":"x"}]"#).is_none(),
            "missing number → can't reattach"
        );
    }
}
