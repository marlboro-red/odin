//! Built-in [`crate::traits::Workspace`] providers.
//!
//! - [`WorktreeWorkspace`] — one throwaway `git worktree` per run, cheap and isolated.
//! - [`SlotPoolWorkspace`] — a fixed pool of pre-cloned slots, claimed/reset/released,
//!   for true parallelism without `git worktree`'s shared-`.git` contention.
//!
//! Both shell out to the `git` CLI. They are constructed by the engine from a workflow's
//! [`crate::ir::WorkspaceConfig`] (with the repo root), not registered as registry
//! singletons, because a worktree set and a slot pool are repo-specific.

mod git;
mod slot;
mod worktree;

pub use slot::SlotPoolWorkspace;
pub use worktree::WorktreeWorkspace;

#[cfg(test)]
pub(crate) mod testutil {
    //! Shared test fixtures: a throwaway git repo with one commit on `main`.

    use tempfile::TempDir;

    use super::git::git;

    /// Creates a temp git repo with an initial commit, returning the (RAII) tempdir.
    pub(crate) async fn init_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        git(p, &["init", "-b", "main"]).await.unwrap();
        git(p, &["config", "user.email", "test@odin.invalid"])
            .await
            .unwrap();
        git(p, &["config", "user.name", "Odin Test"]).await.unwrap();
        std::fs::write(p.join("README.md"), "hello\n").unwrap();
        git(p, &["add", "."]).await.unwrap();
        git(p, &["commit", "-m", "init"]).await.unwrap();
        dir
    }
}
