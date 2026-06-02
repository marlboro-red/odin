//! The [`SlotPoolWorkspace`]: a fixed pool of pre-cloned slots claimed per run.
//!
//! Unlike worktrees (which share one `.git`), each slot is an independent local clone,
//! so N runs get N genuinely independent checkouts. The pool size caps concurrency:
//! `acquire` **blocks** when every slot is in use until one is released.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::sync::{Mutex, Semaphore};

use super::git::git;
use crate::error::WorkspaceError;
use crate::ir::ResetMode;
use crate::traits::{AcquireCtx, Workspace, WorkspaceHandle};

/// A pool of `size` pre-cloned slots under `pool_dir`, each reset before reuse.
pub struct SlotPoolWorkspace {
    repo_root: PathBuf,
    pool_dir: PathBuf,
    reset: ResetMode,
    size: usize,
    /// Indices of currently-free slots.
    free: Mutex<VecDeque<usize>>,
    /// One permit per free slot; `acquire` waits here when the pool is exhausted.
    slots: Semaphore,
    /// Guards one-time lazy cloning of the slots.
    initialized: Mutex<bool>,
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
    ) -> Self {
        Self {
            repo_root: repo_root.into(),
            pool_dir: pool_dir.into(),
            reset,
            size,
            free: Mutex::new(VecDeque::new()),
            slots: Semaphore::new(size),
            initialized: Mutex::new(false),
        }
    }

    fn slot_path(&self, index: usize) -> PathBuf {
        self.pool_dir.join(format!("slot-{index}"))
    }

    /// Clones the slots once, on first use.
    async fn ensure_initialized(&self) -> Result<(), WorkspaceError> {
        let mut done = self.initialized.lock().await;
        if *done {
            return Ok(());
        }
        tokio::fs::create_dir_all(&self.pool_dir).await?;
        let repo = self.repo_root.to_string_lossy().into_owned();
        let mut free = self.free.lock().await;
        for i in 0..self.size {
            let slot = self.slot_path(i);
            if !slot.exists() {
                let slot_str = slot.to_string_lossy().into_owned();
                git(
                    &self.pool_dir,
                    &["clone", "--local", repo.as_str(), slot_str.as_str()],
                )
                .await?;
            }
            free.push_back(i);
        }
        *done = true;
        Ok(())
    }

    /// Restores a slot to a pristine state before it is handed out.
    async fn reset_slot(&self, slot: &Path) -> Result<(), WorkspaceError> {
        match self.reset {
            ResetMode::GitClean => {
                git(slot, &["reset", "--hard"]).await?;
                git(slot, &["clean", "-fdx"]).await?;
            }
            ResetMode::Reclone => {
                let _ = tokio::fs::remove_dir_all(slot).await;
                let repo = self.repo_root.to_string_lossy().into_owned();
                let slot_str = slot.to_string_lossy().into_owned();
                git(
                    &self.pool_dir,
                    &["clone", "--local", repo.as_str(), slot_str.as_str()],
                )
                .await?;
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

        Ok(WorkspaceHandle {
            run_id: ctx.run_id,
            path: slot,
            branch: None,
            token: index.to_string(),
        })
    }

    async fn release(&self, handle: WorkspaceHandle) -> Result<(), WorkspaceError> {
        let index: usize = handle
            .token
            .parse()
            .map_err(|_| WorkspaceError::Git(format!("invalid slot token {:?}", handle.token)))?;
        self.return_slot(index).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SlotPoolWorkspace;
    use crate::ids::RunId;
    use crate::ir::{ResetMode, SlotPoolConfig, WorkspaceConfig};
    use crate::traits::{AcquireCtx, Workspace};
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
        let ws = SlotPoolWorkspace::new(repo.path(), pool.path(), 1, ResetMode::GitClean);

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

    #[tokio::test]
    async fn exhausted_pool_blocks_until_release() {
        let repo = init_repo().await;
        let pool = tempfile::tempdir().unwrap();
        let ws = Arc::new(SlotPoolWorkspace::new(
            repo.path(),
            pool.path(),
            1,
            ResetMode::GitClean,
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
}
