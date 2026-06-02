//! The [`WorktreeWorkspace`]: one throwaway `git worktree` per run.

use std::path::PathBuf;

use async_trait::async_trait;

use super::git::git;
use crate::error::WorkspaceError;
use crate::ir::WorkspaceConfig;
use crate::traits::{AcquireCtx, Workspace, WorkspaceHandle};

/// Provisions each run an isolated `git worktree` cut from the repo, on a fresh
/// `odin/run/<run-id>` branch. Worktrees live under `<repo>/.odin/worktrees/<run-id>`.
pub struct WorktreeWorkspace {
    repo_root: PathBuf,
}

impl WorktreeWorkspace {
    /// Creates a worktree provider rooted at an existing git repository.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    fn worktrees_dir(&self) -> PathBuf {
        self.repo_root.join(".odin").join("worktrees")
    }
}

#[async_trait]
impl Workspace for WorktreeWorkspace {
    // Trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn kind(&self) -> &str {
        "worktree"
    }

    async fn acquire(&self, ctx: AcquireCtx) -> Result<WorkspaceHandle, WorkspaceError> {
        let base = match &ctx.config {
            WorkspaceConfig::Worktree(c) => c.base.clone(),
            WorkspaceConfig::SlotPool(_) => None,
        };
        let run = ctx.run_id;
        let branch = format!("odin/run/{run}");
        let path = self.worktrees_dir().join(run.to_string());
        tokio::fs::create_dir_all(self.worktrees_dir()).await?;

        let path_str = path.to_string_lossy().into_owned();
        // `--` terminates options so a `base` (or path) beginning with `-` can't be
        // reinterpreted by git as a flag.
        let mut args = vec![
            "worktree",
            "add",
            "-b",
            branch.as_str(),
            "--",
            path_str.as_str(),
        ];
        if let Some(b) = base.as_deref() {
            args.push(b);
        }
        git(&self.repo_root, &args).await?;

        Ok(WorkspaceHandle {
            run_id: run,
            path,
            branch: Some(branch),
            token: path_str,
        })
    }

    async fn release(&self, handle: WorkspaceHandle) -> Result<(), WorkspaceError> {
        // Remove the worktree directory; the branch is kept (it may hold committed work
        // the executor still needs to push or open a PR from).
        git(
            &self.repo_root,
            &["worktree", "remove", "--force", handle.token.as_str()],
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::WorktreeWorkspace;
    use crate::ids::RunId;
    use crate::ir::{WorkspaceConfig, WorktreeConfig};
    use crate::traits::{AcquireCtx, Workspace};
    use crate::workspace::testutil::init_repo;

    #[tokio::test]
    async fn acquire_creates_worktree_then_release_removes_it() {
        let repo = init_repo().await;
        let ws = WorktreeWorkspace::new(repo.path());
        let run = RunId::new();
        let handle = ws
            .acquire(AcquireCtx {
                run_id: run,
                config: WorkspaceConfig::Worktree(WorktreeConfig::default()),
            })
            .await
            .unwrap();

        assert!(handle.path.exists());
        assert!(
            handle.path.join("README.md").exists(),
            "worktree has repo content"
        );
        assert_eq!(
            handle.branch.as_deref(),
            Some(format!("odin/run/{run}").as_str())
        );

        ws.release(handle.clone()).await.unwrap();
        assert!(!handle.path.exists(), "worktree removed on release");
    }

    #[tokio::test]
    async fn two_runs_get_isolated_worktrees() {
        let repo = init_repo().await;
        let ws = WorktreeWorkspace::new(repo.path());
        let h1 = ws
            .acquire(AcquireCtx {
                run_id: RunId::new(),
                config: WorkspaceConfig::default(),
            })
            .await
            .unwrap();
        let h2 = ws
            .acquire(AcquireCtx {
                run_id: RunId::new(),
                config: WorkspaceConfig::default(),
            })
            .await
            .unwrap();

        assert_ne!(h1.path, h2.path);
        std::fs::write(h1.path.join("only-in-one.txt"), "x").unwrap();
        assert!(
            !h2.path.join("only-in-one.txt").exists(),
            "worktrees are isolated"
        );

        ws.release(h1).await.unwrap();
        ws.release(h2).await.unwrap();
    }
}
