//! Workspace acquisition and scratch-worktree lifecycle for the executor.
//!
//! Carved out of `local.rs`: building/caching the per-(kind, config) [`Workspace`] instance,
//! acquiring/releasing a run's workspace, and provisioning/reclaiming the throwaway scratch
//! worktrees that isolate concurrent `scratch:` steps. All of these serialize the `worktree`
//! kind under the engine's `worktree_lock` — `git worktree add/remove/prune` mutate the repo's
//! shared `.git/worktrees/` metadata and corrupt it if run concurrently. A second `impl
//! LocalEngine` block (a child module of `local`, so it reads the engine's private fields
//! directly).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::LocalEngine;
use super::gitio::git_opts;
use crate::ids::{RunId, StepId};
use crate::ir::WorkspaceConfig;
use crate::provider::process::run_process;
use crate::traits::{AcquireCtx, CancelToken, Workspace, WorkspaceHandle};
use crate::workspace::{SlotPoolWorkspace, WorktreeWorkspace};

impl LocalEngine {
    pub(crate) fn make_workspace(&self, cfg: &WorkspaceConfig) -> Arc<dyn Workspace> {
        // The workspace `type` the workflow declares, as a registry key.
        let kind = match cfg {
            WorkspaceConfig::Worktree(_) => "worktree",
            WorkspaceConfig::SlotPool(_) => "slot_pool",
        };
        // A custom workspace registered under this kind overrides the built-in (the same
        // last-writer-wins override `register_provider` gives providers). This is the live
        // path for `Registry::register_workspace`. NB: the workflow IR's `workspace.type` is
        // still a closed set (worktree / slot_pool), so an embedder can *replace* a built-in
        // kind but cannot yet introduce a brand-new `type:` string from YAML.
        if let Some(workspace) = self.registry.workspace(kind) {
            return Arc::clone(workspace);
        }
        // Cache one built-in instance per (kind, config) for the engine's lifetime, so all
        // runs and resumes share it. `slot_pool`'s concurrency cap and lease bookkeeping are
        // in-memory and per-instance — a fresh instance per run would defeat them entirely.
        let cfg_json = serde_json::to_string(cfg).unwrap_or_default();
        let key = format!("{kind}|{cfg_json}");
        // Hold the cache lock across the (synchronous) construction so a cold-cache miss can't
        // race: two concurrent first runs of a slot-pool workflow must NOT each build their own
        // `SlotPoolWorkspace` over the same on-disk slot dirs — each would get a full set of
        // permits and hand the same slot to a different run (two agents in one checkout, the
        // ODIN016 failure the cache exists to prevent). The closure runs only on a miss.
        let mut cache = self.workspaces.lock().unwrap();
        let workspace = cache.entry(key).or_insert_with(|| match cfg {
            WorkspaceConfig::Worktree(_) => {
                Arc::new(WorktreeWorkspace::new(self.repo_root.clone()))
            }
            WorkspaceConfig::SlotPool(c) => {
                // The on-disk pool dir is keyed by the config (a hash of the same JSON the
                // instance cache uses), so two DISTINCT slot-pool configs never share physical
                // slot dirs. Otherwise their independent in-memory lease state would hand the
                // SAME slot-N to two concurrent runs — two agents in one checkout — and defeat
                // the per-pool concurrency cap (ODIN016).
                let mut hasher = DefaultHasher::new();
                cfg_json.hash(&mut hasher);
                let pool_dir = self
                    .repo_root
                    .join(".odin")
                    .join("slots")
                    .join(format!("{:016x}", hasher.finish()));
                Arc::new(SlotPoolWorkspace::new(
                    self.repo_root.clone(),
                    pool_dir,
                    c.pool as usize,
                    c.reset,
                    c.base.clone(),
                ))
            }
        });
        Arc::clone(workspace)
    }

    /// Acquires a workspace, serializing `worktree` acquisition under the same lock as scratch
    /// worktrees. `git worktree add`/`remove` mutate the repo's shared `.git/worktrees/`
    /// metadata, which is NOT safe for concurrent runs to touch at once — a concurrent add and
    /// remove corrupt it (`fatal: failed to read .../commondir`), failing a run. `slot_pool`
    /// (and custom kinds) acquire without the lock; they don't touch worktree metadata.
    pub(crate) async fn acquire_workspace(
        &self,
        workspace: &Arc<dyn Workspace>,
        ctx: AcquireCtx,
    ) -> std::result::Result<WorkspaceHandle, crate::error::WorkspaceError> {
        if workspace.kind() == "worktree" {
            let _guard = self.worktree_lock.lock().await;
            workspace.acquire(ctx).await
        } else {
            workspace.acquire(ctx).await
        }
    }

    /// Releases a workspace (best effort), serialized for `worktree` kinds — see
    /// [`acquire_workspace`](Self::acquire_workspace).
    pub(crate) async fn release_workspace(
        &self,
        workspace: &Arc<dyn Workspace>,
        handle: WorkspaceHandle,
    ) {
        if workspace.kind() == "worktree" {
            let _guard = self.worktree_lock.lock().await;
            let _ = workspace.release(handle).await;
        } else {
            let _ = workspace.release(handle).await;
        }
    }

    /// Adds a detached git worktree at the run's base commit (`HEAD` of `base_workdir`) as a
    /// throwaway scratch dir, outside the run workdir so it never pollutes the shared DIFF.
    /// Named by `run_id` so [`cleanup_scratch`](Self::cleanup_scratch) can reclaim leftovers.
    pub(crate) async fn acquire_scratch(
        &self,
        run_id: RunId,
        base_workdir: &Path,
        step_id: &StepId,
        cancel: &CancelToken,
    ) -> std::result::Result<PathBuf, String> {
        let scratch = std::env::temp_dir().join(format!(
            "odin-scratch-{run_id}-{}-{}",
            step_id.as_str(),
            uuid::Uuid::new_v4()
        ));
        let scratch_str = scratch.to_string_lossy();
        let opts = git_opts(base_workdir);
        let args = ["worktree", "add", "--detach", &scratch_str, "HEAD"].map(str::to_owned);
        // Serialized: concurrent scratch steps must not race on git's worktree metadata.
        let out = {
            let _guard = self.worktree_lock.lock().await;
            run_process("git", &args, &opts, cancel).await
        }
        .map_err(|e| e.to_string())?;
        if out.exit_code == 0 {
            Ok(scratch)
        } else {
            Err(format!("git worktree add failed: {}", out.stderr.trim()))
        }
    }

    /// Removes a scratch worktree (best effort — failure only leaks a temp dir).
    pub(crate) async fn release_scratch(
        &self,
        base_workdir: &Path,
        scratch: &Path,
        cancel: &CancelToken,
    ) {
        let scratch_str = scratch.to_string_lossy();
        let opts = git_opts(base_workdir);
        let args = ["worktree", "remove", "--force", &scratch_str].map(str::to_owned);
        let _guard = self.worktree_lock.lock().await;
        let _ = run_process("git", &args, &opts, cancel).await;
    }

    /// Reclaims scratch worktrees left over from a previous attempt of this run (a crash or
    /// kill mid-scratch-step leaks the temp dir). Called once at the start of a run that has
    /// scratch steps, so resumes don't accumulate orphaned worktrees.
    pub(crate) async fn cleanup_scratch(
        &self,
        run_id: RunId,
        base_workdir: &Path,
        cancel: &CancelToken,
    ) {
        let opts = git_opts(base_workdir);
        let prefix = format!("odin-scratch-{run_id}-");
        // Serialize against acquire/release_scratch: `git worktree remove` and especially
        // `git worktree prune` (a *global* operation on `.git/worktrees/`) race with a
        // concurrent run's `git worktree add`, corrupting git's worktree metadata. Hold the
        // same lock those paths take for the whole cleanup (it is rare — once per resuming
        // run that has scratch steps). The filesystem reads/removes are cheap to keep inside.
        let _guard = self.worktree_lock.lock().await;
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().starts_with(&prefix) {
                    continue;
                }
                let p = entry.path();
                let p = p.to_string_lossy();
                let args = ["worktree", "remove", "--force", &p].map(str::to_owned);
                let _ = run_process("git", &args, &opts, cancel).await;
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        // Drop any now-dangling worktree registrations from the repo metadata.
        let _ = run_process(
            "git",
            &["worktree".to_owned(), "prune".to_owned()],
            &opts,
            cancel,
        )
        .await;
    }
}
