//! The [`Workspace`] trait: provision an isolated working directory per run.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::WorkspaceError;
use crate::ids::RunId;
use crate::ir::WorkspaceConfig;

/// Provides each run an isolated working directory. v1 implementations: a per-run git
/// worktree, and a fixed-size pool of pre-cloned slots.
///
/// Lifecycle: `acquire` → steps run against `handle.path` → `release`.
#[async_trait]
pub trait Workspace: Send + Sync {
    /// Registry key (e.g. `"worktree"` | `"slot_pool"`).
    fn kind(&self) -> &str;

    /// Claims a workdir for a run. **Blocks/queues** if a finite pool is exhausted (it waits for a
    /// free slot rather than erroring).
    ///
    /// # Errors
    /// Returns a [`WorkspaceError`] if provisioning fails (a git or I/O error).
    async fn acquire(&self, ctx: AcquireCtx) -> Result<WorkspaceHandle, WorkspaceError>;

    /// Releases/resets a previously acquired workspace. Idempotent.
    ///
    /// # Errors
    /// Returns a [`WorkspaceError`] if cleanup fails.
    async fn release(&self, handle: WorkspaceHandle) -> Result<(), WorkspaceError>;
}

/// What the engine knows when acquiring a workspace.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AcquireCtx {
    /// The run requesting a workspace.
    pub run_id: RunId,
    /// The workflow's declared workspace config.
    pub config: WorkspaceConfig,
}

/// A claimed workspace lease.
///
/// It is `Clone + Serialize` because [`crate::traits::RunState`] must persist it for
/// crash-resume; single-ownership of the lease is enforced by the engine's
/// acquire→release lifecycle, not by the type. The `token` is an impl-private reclaim
/// handle (slot index, worktree name) opaque to the engine.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceHandle {
    /// The run this lease belongs to.
    pub run_id: RunId,
    /// Absolute path steps execute in.
    pub path: PathBuf,
    /// Branch/ref created for this run, if any (folded into the run summary).
    pub branch: Option<String>,
    /// Impl-private reclaim token, opaque to the engine.
    pub token: String,
}

impl WorkspaceHandle {
    /// Builds a workspace lease. A [`Workspace`] implementation returns this from `acquire`;
    /// because the struct is `#[non_exhaustive]`, out-of-crate implementors must use this
    /// constructor (a struct literal won't compile in another crate).
    ///
    /// `path` should be **absolute** — the engine sets each step's working directory to it,
    /// and tools resolve their own path arguments against it. `branch` is the ref created for
    /// the run, if any; `token` is an impl-private reclaim handle (slot index, worktree name)
    /// the engine treats as opaque and hands back to `release`.
    #[must_use]
    pub fn new(
        run_id: RunId,
        path: PathBuf,
        branch: Option<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            path,
            branch,
            token: token.into(),
        }
    }
}
