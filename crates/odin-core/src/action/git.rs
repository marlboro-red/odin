//! The `git.commit` and `git.push` actions.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use crate::api::SideEffect;
use crate::error::ActionError;
use crate::traits::{Action, ActionCtx, ActionOutcome};

/// `git.commit` — stage everything and commit with `with.message`.
///
/// A no-op (exit 0, no side effect) when there is nothing to commit, so it composes
/// cleanly after a step that may or may not have changed the tree.
pub struct GitCommit;

#[async_trait]
impl Action for GitCommit {
    // The trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "git.commit"
    }

    async fn run(&self, ctx: ActionCtx) -> Result<ActionOutcome, ActionError> {
        let message = super::arg_str(&ctx.args, "message")?;
        let dir = &ctx.workdir;

        super::checked("git", &["add", "-A"], dir).await?;
        let staged = super::checked("git", &["diff", "--cached", "--name-only"], dir).await?;
        if staged.trim().is_empty() {
            return Ok(ActionOutcome::default());
        }

        super::checked("git", &["commit", "-m", message], dir).await?;
        let sha = super::checked("git", &["rev-parse", "HEAD"], dir)
            .await?
            .trim()
            .to_owned();
        let branch = super::checked("git", &["rev-parse", "--abbrev-ref", "HEAD"], dir)
            .await?
            .trim()
            .to_owned();

        let mut outputs = IndexMap::new();
        outputs.insert("sha".to_owned(), Value::String(sha.clone()));
        outputs.insert("branch".to_owned(), Value::String(branch.clone()));
        Ok(ActionOutcome {
            exit_code: 0,
            outputs,
            side_effects: vec![SideEffect::Commit {
                sha,
                branch: Some(branch),
            }],
        })
    }
}

/// `git.push` — push the run's branch to a remote (`with.remote`, default `origin`;
/// `with.branch`, default the current branch).
pub struct GitPush;

#[async_trait]
impl Action for GitPush {
    // The trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "git.push"
    }

    async fn run(&self, ctx: ActionCtx) -> Result<ActionOutcome, ActionError> {
        let dir = &ctx.workdir;
        let remote = super::arg_str_or(&ctx.args, "remote", "origin");
        let branch = match ctx.args.get("branch").and_then(Value::as_str) {
            Some(b) => b.to_owned(),
            None => super::checked("git", &["rev-parse", "--abbrev-ref", "HEAD"], dir)
                .await?
                .trim()
                .to_owned(),
        };

        // Refuse a remote/branch that git would parse as an option (these come from
        // templated `with:` values that may include trigger data).
        if remote.starts_with('-') || branch.starts_with('-') {
            return Err(ActionError::Other(anyhow::anyhow!(
                "git.push: remote/branch must not start with '-' (remote={remote:?}, branch={branch:?})"
            )));
        }
        super::checked(
            "git",
            &["push", "--set-upstream", remote.as_str(), branch.as_str()],
            dir,
        )
        .await?;

        let mut outputs = IndexMap::new();
        outputs.insert("branch".to_owned(), Value::String(branch.clone()));
        outputs.insert("remote".to_owned(), Value::String(remote.clone()));
        Ok(ActionOutcome {
            exit_code: 0,
            outputs,
            side_effects: vec![SideEffect::Push { branch, remote }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{GitCommit, GitPush};
    use crate::api::SideEffect;
    use crate::ids::StepId;
    use crate::traits::{Action, ActionCtx};
    use indexmap::IndexMap;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn ctx(workdir: &Path, args: IndexMap<String, Value>) -> ActionCtx {
        ActionCtx {
            step_id: StepId::new("s"),
            workdir: workdir.to_path_buf(),
            args,
        }
    }

    fn work_repo(dir: &Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "t@odin.invalid"]);
        git(dir, &["config", "user.name", "Odin Test"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "init"]);
    }

    #[tokio::test]
    async fn commit_then_push_to_a_bare_remote() {
        let bare = tempfile::tempdir().unwrap();
        git(bare.path(), &["init", "--bare", "-b", "main"]);
        let repo = tempfile::tempdir().unwrap();
        work_repo(repo.path());
        git(
            repo.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git(repo.path(), &["checkout", "-b", "work"]);
        std::fs::write(repo.path().join("new.txt"), "content\n").unwrap();

        let mut args = IndexMap::new();
        args.insert("message".to_owned(), json!("automated commit"));
        let committed = GitCommit.run(ctx(repo.path(), args)).await.unwrap();
        assert_eq!(committed.exit_code, 0);
        assert!(matches!(
            committed.side_effects.first(),
            Some(SideEffect::Commit { .. })
        ));

        let pushed = GitPush
            .run(ctx(repo.path(), IndexMap::new()))
            .await
            .unwrap();
        assert!(matches!(
            pushed.side_effects.first(),
            Some(SideEffect::Push { .. })
        ));

        // The bare remote now has the pushed `work` branch.
        let listed = Command::new("git")
            .current_dir(bare.path())
            .args(["branch", "--list", "work"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&listed.stdout).contains("work"));
    }

    #[tokio::test]
    async fn commit_is_a_noop_when_nothing_changed() {
        let repo = tempfile::tempdir().unwrap();
        work_repo(repo.path());
        let mut args = IndexMap::new();
        args.insert("message".to_owned(), json!("nothing to do"));
        let out = GitCommit.run(ctx(repo.path(), args)).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(
            out.side_effects.is_empty(),
            "no changes => no commit, no side effect"
        );
    }
}
