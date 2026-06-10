//! The [`SlotPoolWorkspace`]: a fixed pool of pre-cloned slots claimed per run.
//!
//! Unlike worktrees (which share one `.git`), each slot is an independent local clone,
//! so N runs get N genuinely independent checkouts. The pool size caps concurrency:
//! `acquire` **blocks** when every slot is in use until one is released.
//!
//! The pool must be a **single shared instance** for the cap to hold — the engine caches one
//! per (kind, config) so all concurrent runs share it (see `LocalEngine::make_workspace`).
//! Lease state is **in-memory**: it is *not* durable across a process restart. So a run that
//! is resumed after a daemon restart executes against its persisted slot path, but the fresh
//! pool no longer knows that slot is leased — if you need bounded concurrency *and*
//! crash-resume across restarts, prefer the `worktree` workspace.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::sync::{Mutex, Semaphore};

use super::git::git;
use crate::error::WorkspaceError;
use crate::ir::ResetMode;
use crate::traits::{AcquireCtx, Workspace, WorkspaceHandle};

/// Durable ref (inside each slot's own `.git`) recording the slot's pristine commit, so a
/// `git_clean` reset can fully restore the slot between leases — including undoing any commits a
/// prior lease made, which a plain `reset --hard` to the *current* HEAD would leave behind.
const PRISTINE_REF: &str = "refs/odin/pristine";
/// Git-config key (inside each slot) recording the pristine branch name (empty if the clone left
/// HEAD detached at `base`), so the reset restores the original branch, not a detached HEAD.
const PRISTINE_BRANCH_CFG: &str = "odin.pristineBranch";

/// A pool of `size` pre-cloned slots under `pool_dir`, each reset before reuse.
pub struct SlotPoolWorkspace {
    repo_root: PathBuf,
    pool_dir: PathBuf,
    reset: ResetMode,
    /// Optional ref each slot is checked out at after cloning (else the clone's default HEAD).
    base: Option<String>,
    size: usize,
    /// Indices of currently-free slots.
    free: Mutex<VecDeque<usize>>,
    /// One permit per free slot; `acquire` waits here when the pool is exhausted.
    slots: Semaphore,
    /// Guards one-time lazy cloning of the slots.
    initialized: Mutex<bool>,
    /// Slot indices this instance has handed out and not yet reclaimed. `release` only frees a
    /// slot recorded here, so a stray/duplicate release (or a `release` on a fresh instance
    /// after a restart) can't inject a phantom permit that lets the pool over-hand-out slots.
    leased: Mutex<HashSet<usize>>,
}

impl SlotPoolWorkspace {
    /// Creates a slot pool of `size` clones of `repo_root` under `pool_dir`.
    ///
    /// Slots are cloned lazily on the first [`Workspace::acquire`].
    pub fn new(
        repo_root: impl Into<PathBuf>,
        pool_dir: impl Into<PathBuf>,
        size: usize,
        reset: ResetMode,
        base: Option<String>,
    ) -> Self {
        Self {
            repo_root: repo_root.into(),
            pool_dir: pool_dir.into(),
            reset,
            base,
            size,
            free: Mutex::new(VecDeque::new()),
            slots: Semaphore::new(size),
            initialized: Mutex::new(false),
            leased: Mutex::new(HashSet::new()),
        }
    }

    fn slot_path(&self, index: usize) -> PathBuf {
        self.pool_dir.join(format!("slot-{index}"))
    }

    /// True if `slot` is a usable git clone — used to detect a slot left half-cloned by a crash
    /// (the dir exists but is not a valid repo), which must be re-cloned rather than trusted.
    async fn is_healthy(slot: &Path) -> bool {
        slot.join(".git").exists() && git(slot, &["rev-parse", "--git-dir"]).await.is_ok()
    }

    /// (Re-)clones a slot from the repo and checks out `base` if one was requested. The caller
    /// removes any stale dir first.
    async fn provision(&self, slot: &Path) -> Result<(), WorkspaceError> {
        let repo = self.repo_root.to_string_lossy().into_owned();
        let slot_str = slot.to_string_lossy().into_owned();
        git(
            &self.pool_dir,
            &["clone", "--local", repo.as_str(), slot_str.as_str()],
        )
        .await?;
        if let Some(base) = &self.base {
            git(slot, &["checkout", base.as_str()]).await?;
        }
        // Record the pristine state (a durable ref + the branch name) so `reset_slot` can restore
        // the slot exactly between leases, undoing not just uncommitted changes but any commits or
        // branch moves a prior lease made. Both live in the slot's own `.git`, so they also survive
        // a daemon restart (a healthy slot is reused, not re-cloned).
        git(slot, &["update-ref", PRISTINE_REF, "HEAD"]).await?;
        let branch = git(slot, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await
            .unwrap_or_default();
        git(slot, &["config", PRISTINE_BRANCH_CFG, branch.trim()]).await?;
        Ok(())
    }

    /// Restores a slot to its recorded pristine commit/branch, discarding all working-tree changes
    /// AND any commits a prior lease added. Errors if the slot isn't a healthy clone with the
    /// pristine markers (e.g. an old slot, or a crash-corrupted one) — the caller then re-clones.
    async fn restore_pristine(&self, slot: &Path) -> Result<(), WorkspaceError> {
        let branch = git(slot, &["config", "--get", PRISTINE_BRANCH_CFG])
            .await
            .unwrap_or_default();
        let branch = branch.trim();
        if branch.is_empty() {
            // The clone left HEAD detached at `base`; restore that detached pristine commit.
            git(slot, &["checkout", "-f", "--detach", PRISTINE_REF]).await?;
        } else {
            // `-B` re-creates/moves the original branch to the pristine commit and checks it out,
            // force-discarding tracked changes and undoing any commits the prior lease added.
            git(slot, &["checkout", "-f", "-B", branch, PRISTINE_REF]).await?;
        }
        git(slot, &["clean", "-fdx"]).await?;
        Ok(())
    }

    /// Clones the slots once, on first use.
    async fn ensure_initialized(&self) -> Result<(), WorkspaceError> {
        let mut done = self.initialized.lock().await;
        if *done {
            return Ok(());
        }
        tokio::fs::create_dir_all(&self.pool_dir).await?;
        // Clone every slot first; publish the free indices only once they ALL succeed, so
        // a failed-then-retried init can never push the same index twice (which would let
        // two runs claim one slot). A slot the previous process left half-cloned (exists but
        // not a valid repo — a crash mid-`git clone`) is removed and re-cloned, rather than
        // trusted as present and then poisoning the pool when every reset fails on it.
        for i in 0..self.size {
            let slot = self.slot_path(i);
            if !Self::is_healthy(&slot).await {
                let _ = tokio::fs::remove_dir_all(&slot).await;
                self.provision(&slot).await?;
            }
        }
        let mut free = self.free.lock().await;
        free.clear();
        free.extend(0..self.size);
        *done = true;
        Ok(())
    }

    /// Restores a slot to a pristine state before it is handed out.
    async fn reset_slot(&self, slot: &Path) -> Result<(), WorkspaceError> {
        match self.reset {
            ResetMode::GitClean => {
                // Restore to the recorded pristine commit/branch (undoing a prior lease's commits,
                // not just its uncommitted edits). If that fails the slot isn't a healthy clone
                // with pristine markers (corrupted by a crash, or cloned by an older odin) — re-
                // clone it rather than cycle a dead slot back to the free set forever.
                if self.restore_pristine(slot).await.is_err() {
                    let _ = tokio::fs::remove_dir_all(slot).await;
                    return self.provision(slot).await;
                }
            }
            ResetMode::Reclone => {
                let _ = tokio::fs::remove_dir_all(slot).await;
                self.provision(slot).await?;
            }
        }
        Ok(())
    }

    /// Returns a slot index to the free set and wakes one waiter.
    async fn return_slot(&self, index: usize) {
        self.free.lock().await.push_back(index);
        self.slots.add_permits(1);
    }
}

#[async_trait]
impl Workspace for SlotPoolWorkspace {
    // Trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn kind(&self) -> &str {
        "slot_pool"
    }

    async fn acquire(&self, ctx: AcquireCtx) -> Result<WorkspaceHandle, WorkspaceError> {
        self.ensure_initialized().await?;

        // Wait for a free slot, then forget the permit — the slot is reclaimed by index
        // (stored in the handle), not by a non-serializable permit guard.
        let permit = self
            .slots
            .acquire()
            .await
            .map_err(|_| WorkspaceError::Git("slot pool is closed".to_owned()))?;
        permit.forget();

        let index = self
            .free
            .lock()
            .await
            .pop_front()
            .expect("a permit guarantees a free slot index");
        let slot = self.slot_path(index);

        if let Err(e) = self.reset_slot(&slot).await {
            // Don't leak the slot if reset fails.
            self.return_slot(index).await;
            return Err(e);
        }
        self.leased.lock().await.insert(index);

        Ok(WorkspaceHandle::new(
            ctx.run_id,
            slot,
            None,
            index.to_string(),
        ))
    }

    async fn release(&self, handle: WorkspaceHandle) -> Result<(), WorkspaceError> {
        let index: usize = handle
            .token
            .parse()
            .map_err(|_| WorkspaceError::Git(format!("invalid slot token {:?}", handle.token)))?;
        // Only reclaim a slot THIS instance leased. A release of an index we never handed out
        // — a double-release, or a resume after a restart on a fresh pool — must not push a
        // phantom permit (which would let the pool hand out more than `size` slots at once).
        if self.leased.lock().await.remove(&index) {
            self.return_slot(index).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SlotPoolWorkspace;
    use crate::ids::RunId;
    use crate::ir::{ResetMode, SlotPoolConfig, WorkspaceConfig};
    use crate::traits::{AcquireCtx, Workspace, WorkspaceHandle};
    use crate::workspace::testutil::init_repo;
    use std::sync::Arc;
    use std::time::Duration;

    fn ctx() -> AcquireCtx {
        AcquireCtx {
            run_id: RunId::new(),
            config: WorkspaceConfig::SlotPool(SlotPoolConfig {
                pool: 1,
                reset: ResetMode::GitClean,
                base: None,
            }),
        }
    }

    #[tokio::test]
    async fn slot_has_repo_content_and_is_reset_between_uses() {
        let repo = init_repo().await;
        let pool = tempfile::tempdir().unwrap();
        let ws = SlotPoolWorkspace::new(repo.path(), pool.path(), 1, ResetMode::GitClean, None);

        let h1 = ws.acquire(ctx()).await.unwrap();
        assert!(
            h1.path.join("README.md").exists(),
            "slot is a checkout of the repo"
        );
        std::fs::write(h1.path.join("dirty.txt"), "x").unwrap();
        ws.release(h1).await.unwrap();

        let h2 = ws.acquire(ctx()).await.unwrap();
        assert!(
            !h2.path.join("dirty.txt").exists(),
            "reset cleaned the untracked file"
        );
        assert!(h2.path.join("README.md").exists());
        ws.release(h2).await.unwrap();
    }

    /// A prior lease's COMMITS (and branch moves) must be undone on reset, not just its
    /// uncommitted edits — otherwise the next run inherits the prior run's HEAD as its
    /// `base_commit` and its committed files, silently corrupting the DIFF and what gets pushed.
    #[tokio::test]
    async fn a_committed_change_is_undone_on_reset_between_uses() {
        let repo = init_repo().await;
        let pool = tempfile::tempdir().unwrap();
        let ws = SlotPoolWorkspace::new(repo.path(), pool.path(), 1, ResetMode::GitClean, None);

        let h1 = ws.acquire(ctx()).await.unwrap();
        let slot = h1.path.clone();
        let g = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(&slot)
                .args(args)
                .output()
                .unwrap();
        };
        std::fs::write(slot.join("leaked.txt"), "x").unwrap();
        g(&["checkout", "-q", "-b", "prior-work"]);
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "prior lease commit"]);
        ws.release(h1).await.unwrap();

        let h2 = ws.acquire(ctx()).await.unwrap();
        assert!(
            !h2.path.join("leaked.txt").exists(),
            "the prior lease's committed file must be gone after reset"
        );
        let log = std::process::Command::new("git")
            .current_dir(&h2.path)
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(
            !log.contains("prior lease commit"),
            "the prior lease's commit must be undone on reset: {log}"
        );
        let branch = std::process::Command::new("git")
            .current_dir(&h2.path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_ne!(
            String::from_utf8_lossy(&branch.stdout).trim(),
            "prior-work",
            "the slot must be restored to its pristine branch, not the prior lease's"
        );
        ws.release(h2).await.unwrap();
    }

    #[tokio::test]
    async fn exhausted_pool_blocks_until_release() {
        let repo = init_repo().await;
        let pool = tempfile::tempdir().unwrap();
        let ws = Arc::new(SlotPoolWorkspace::new(
            repo.path(),
            pool.path(),
            1,
            ResetMode::GitClean,
            None,
        ));

        let h1 = ws.acquire(ctx()).await.unwrap();

        // A second acquire must block while the single slot is held.
        let ws2 = Arc::clone(&ws);
        let pending = tokio::spawn(async move { ws2.acquire(ctx()).await });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !pending.is_finished(),
            "acquire should block on an exhausted pool"
        );

        // Releasing the slot unblocks the waiter.
        ws.release(h1).await.unwrap();
        let h2 = pending.await.unwrap().unwrap();
        assert!(h2.path.exists());
        ws.release(h2).await.unwrap();
    }

    #[tokio::test]
    async fn stale_release_does_not_inject_a_phantom_permit() {
        // A release of a slot this instance didn't lease (a double-release, or a resume after
        // a restart) must NOT raise the pool's concurrency cap.
        let repo = init_repo().await;
        let pool = tempfile::tempdir().unwrap();
        let ws = Arc::new(SlotPoolWorkspace::new(
            repo.path(),
            pool.path(),
            1,
            ResetMode::GitClean,
            None,
        ));

        let h1 = ws.acquire(ctx()).await.unwrap();
        let token = h1.token.clone();
        ws.release(h1).await.unwrap();
        // Re-release the same (now-unleased) token — must be a no-op.
        let stale = WorkspaceHandle::new(RunId::new(), pool.path().join("slot-0"), None, token);
        ws.release(stale).await.unwrap();

        // Hold the single slot; a second acquire must still BLOCK (cap = 1). If the stale
        // release had added a phantom permit, this would wrongly succeed immediately.
        let h2 = ws.acquire(ctx()).await.unwrap();
        let ws2 = Arc::clone(&ws);
        let pending = tokio::spawn(async move { ws2.acquire(ctx()).await });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !pending.is_finished(),
            "a stale release must not raise the pool cap"
        );

        ws.release(h2).await.unwrap();
        let h3 = pending.await.unwrap().unwrap();
        ws.release(h3).await.unwrap();
    }

    #[tokio::test]
    async fn a_corrupt_slot_is_recloned_on_reset() {
        // A crash can leave a slot whose `.git` is gone/corrupt. The next acquire's reset must
        // re-clone it, not fail forever and cycle the dead slot back to the free set.
        let repo = init_repo().await;
        let pool = tempfile::tempdir().unwrap();
        let ws = SlotPoolWorkspace::new(repo.path(), pool.path(), 1, ResetMode::GitClean, None);
        let h1 = ws.acquire(ctx()).await.unwrap();
        let slot = h1.path.clone();
        ws.release(h1).await.unwrap();
        std::fs::remove_dir_all(slot.join(".git")).unwrap(); // corrupt it

        let h2 = ws.acquire(ctx()).await.unwrap();
        assert!(
            h2.path.join("README.md").exists() && h2.path.join(".git").exists(),
            "a corrupt slot must be re-cloned on reset"
        );
        ws.release(h2).await.unwrap();
    }

    #[tokio::test]
    async fn a_half_cloned_slot_is_recloned_on_init() {
        // A slot left existing-but-not-a-repo by a crash mid-`git clone` must be re-cloned at init,
        // not trusted as present (which would poison the pool when every reset failed on it).
        let repo = init_repo().await;
        let pool = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(pool.path().join("slot-0")).unwrap();
        std::fs::write(pool.path().join("slot-0").join("junk"), "x").unwrap();
        let ws = SlotPoolWorkspace::new(repo.path(), pool.path(), 1, ResetMode::GitClean, None);

        let h1 = ws.acquire(ctx()).await.unwrap();
        assert!(
            h1.path.join("README.md").exists() && h1.path.join(".git").exists(),
            "a half-cloned slot must be re-cloned on init"
        );
        ws.release(h1).await.unwrap();
    }

    #[tokio::test]
    async fn slots_are_checked_out_at_base() {
        // `base:` must be honored (parity with the worktree workspace), not silently dropped.
        let repo = init_repo().await;
        let g = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .output()
                .unwrap();
        };
        g(&["checkout", "-q", "-b", "other"]);
        std::fs::write(repo.path().join("only-other.txt"), "x").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "on other"]);
        g(&["checkout", "-q", "main"]); // leave the repo on main, so base must do the work

        let pool = tempfile::tempdir().unwrap();
        let ws = SlotPoolWorkspace::new(
            repo.path(),
            pool.path(),
            1,
            ResetMode::GitClean,
            Some("other".to_owned()),
        );
        let h = ws.acquire(ctx()).await.unwrap();
        assert!(
            h.path.join("only-other.txt").exists(),
            "slot should be checked out at base=other, not the default HEAD"
        );
        ws.release(h).await.unwrap();
    }
}
