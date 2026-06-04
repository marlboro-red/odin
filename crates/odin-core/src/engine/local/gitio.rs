//! Git/diff/snapshot/ref primitives for the executor.
//!
//! Low-level, workdir-scoped git operations carved out of `local.rs`: the working-tree diff
//! capture, HEAD read, off-branch snapshot commits and their anchoring refs, the worktree
//! restore, and ref cleanup. A second `impl LocalEngine` block (these are inherent methods that
//! take `&self` but touch no engine state — they're grouped on the type for `self.method()`
//! ergonomics), plus the shared [`git_opts`] builder.

use std::path::Path;

use super::LocalEngine;
use crate::ids::RunId;
use crate::provider::process::{ProcessOptions, run_process};
use crate::traits::CancelToken;

impl LocalEngine {
    /// Captures the working-tree diff in `workdir`, including agent-created files.
    ///
    /// Coding agents most often *create* files, which a plain `git diff` (tracked
    /// changes only) would miss — so we first mark everything intent-to-add
    /// (`git add -N .`), then diff. Best-effort: a non-git workdir yields `None`.
    pub(crate) async fn capture_diff(
        &self,
        workdir: &Path,
        base: Option<&str>,
        cancel: &CancelToken,
    ) -> Option<String> {
        let opts = git_opts(workdir);
        let intent_to_add = ["add", "-N", "."].map(str::to_owned);
        let _ = run_process("git", &intent_to_add, &opts, cancel).await;
        // Diff against the run's base commit when known (snapshots may have advanced the
        // index without moving it), else the working tree's HEAD. `--` disambiguates the
        // revision from any path.
        let args = match base {
            Some(base) => vec!["diff".to_owned(), base.to_owned(), "--".to_owned()],
            None => vec!["diff".to_owned()],
        };
        // Gate on a clean exit: an unresolvable base (e.g. a reaped snapshot) makes
        // `git diff <base>` exit non-zero with EMPTY stdout, which must NOT be mistaken for a
        // real empty diff and clobber the carried-forward DIFF — return `None` instead.
        run_process("git", &args, &opts, cancel)
            .await
            .ok()
            .filter(|o| o.exit_code == 0)
            .map(|o| o.stdout)
    }

    /// The workspace's current `HEAD` commit, or `None` if it cannot be read (non-git dir).
    pub(crate) async fn git_head(&self, workdir: &Path, cancel: &CancelToken) -> Option<String> {
        let opts = git_opts(workdir);
        let out = run_process(
            "git",
            &["rev-parse".to_owned(), "HEAD".to_owned()],
            &opts,
            cancel,
        )
        .await
        .ok()?;
        let head = out.stdout.trim();
        (out.exit_code == 0 && !head.is_empty()).then(|| head.to_owned())
    }

    /// Snapshots the current workspace tree as an off-branch commit parented on `base`,
    /// kept alive by the per-run ref `refs/odin/run/<run_id>` so [`Self::restore_workdir`] can
    /// reach it on resume — this is the **durable** snapshot the persisted `RunState.snapshot`
    /// points at. Returns the commit SHA, or `None` on any git failure.
    pub(crate) async fn snapshot_workdir(
        &self,
        workdir: &Path,
        base: &str,
        run_id: RunId,
        cancel: &CancelToken,
    ) -> Option<String> {
        self.snapshot_to_ref(
            workdir,
            base,
            run_id,
            &format!("refs/odin/run/{run_id}"),
            cancel,
        )
        .await
    }

    /// Snapshots the workspace tree as an off-branch commit parented on `base`, anchored by
    /// `snapshot_ref` so [`Self::restore_workdir`] can reach it. Returns the commit SHA, or `None`
    /// on any git failure (snapshots are best-effort — resume idempotency degrades gracefully
    /// without one). Does not touch the branch, HEAD, or the working index (it stages into a
    /// throwaway index file). The caller chooses the ref so a transient snapshot (e.g. the
    /// per-step retry rewind point) can use a *separate* ref and not disturb the durable
    /// per-run ref that resume relies on.
    pub(crate) async fn snapshot_to_ref(
        &self,
        workdir: &Path,
        base: &str,
        run_id: RunId,
        snapshot_ref: &str,
        cancel: &CancelToken,
    ) -> Option<String> {
        let index_path =
            std::env::temp_dir().join(format!("odin-index-{run_id}-{}", uuid::Uuid::new_v4()));
        let index_str = index_path.to_string_lossy().into_owned();
        // Stage into a throwaway index, and pin an explicit identity so `commit-tree`
        // succeeds even in a repo with no configured user (or `user.useConfigOnly`). The base
        // git env (raw-bytes / no CRLF normalization) carries through.
        let mut staged = git_opts(workdir);
        staged.env.extend([
            ("GIT_INDEX_FILE".to_owned(), index_str),
            ("GIT_AUTHOR_NAME".to_owned(), "odin".to_owned()),
            ("GIT_AUTHOR_EMAIL".to_owned(), "odin@localhost".to_owned()),
            ("GIT_COMMITTER_NAME".to_owned(), "odin".to_owned()),
            (
                "GIT_COMMITTER_EMAIL".to_owned(),
                "odin@localhost".to_owned(),
            ),
        ]);
        let run_git = |args: Vec<String>| {
            let opts = staged.clone();
            async move { run_process("git", &args, &opts, cancel).await.ok() }
        };
        // Seed a throwaway index from HEAD, stage every change (tracked + untracked), write
        // the tree, and commit it parented on the run's base — no branch is moved. Compute
        // the SHA, then remove the temp index on EVERY path (a `?` here returns from the
        // block, not the function, so the cleanup below always runs).
        let sha = async {
            let read = run_git(vec!["read-tree".to_owned(), "HEAD".to_owned()]).await?;
            if read.exit_code != 0 {
                return None;
            }
            let _ = run_git(vec!["add".to_owned(), "-A".to_owned()]).await;
            let tree_out = run_git(vec!["write-tree".to_owned()]).await?;
            let tree = tree_out.stdout.trim().to_owned();
            if tree_out.exit_code != 0 || tree.is_empty() {
                return None;
            }
            let commit = run_git(vec![
                "commit-tree".to_owned(),
                tree,
                "-p".to_owned(),
                base.to_owned(),
                "-m".to_owned(),
                "odin snapshot".to_owned(),
            ])
            .await?;
            let sha = commit.stdout.trim().to_owned();
            (commit.exit_code == 0 && !sha.is_empty()).then_some(sha)
        }
        .await;
        let _ = std::fs::remove_file(&index_path);
        let sha = sha?;
        // Anchor the dangling commit so it survives until the run completes.
        let opts = git_opts(workdir);
        let _ = run_process(
            "git",
            &[
                "update-ref".to_owned(),
                snapshot_ref.to_owned(),
                sha.clone(),
            ],
            &opts,
            cancel,
        )
        .await;
        Some(sha)
    }

    /// Resets the workspace (index + worktree) to `target`, then drops leftover untracked
    /// files — so a step interrupted mid-edit re-runs from a clean, known state. HEAD is not
    /// moved; callers only restore while HEAD is still at the run's base (no commits), so the
    /// worktree and HEAD stay consistent.
    ///
    /// `git clean -fd` discards every *non-ignored* untracked file in the workspace created
    /// since `target`. That is the intended blast radius — the run's worktree is a throwaway
    /// per-run checkout, not the user's repo — but ignored files (`.gitignore`d build caches,
    /// local `.env`, etc.) are deliberately left untouched and so are NOT rewound; a step's
    /// side effects on ignored paths are outside snapshot/restore.
    ///
    /// If `read-tree` fails (e.g. the snapshot commit was reaped), the restore is abandoned
    /// WITHOUT running `clean`, leaving the workspace untouched rather than half-reset.
    ///
    /// Returns `true` iff the worktree was actually reset to `target`. A caller that advances
    /// resume state on the assumption the restore happened (the loop re-entering at a later
    /// iteration) MUST gate on this — otherwise a reaped snapshot would skip iterations whose work
    /// is no longer present, re-entering against a tree that lacks it.
    pub(crate) async fn restore_workdir(
        &self,
        workdir: &Path,
        target: &str,
        cancel: &CancelToken,
    ) -> bool {
        let opts = git_opts(workdir);
        let read = ["read-tree", "-u", "--reset", target].map(str::to_owned);
        let reset_ok = run_process("git", &read, &opts, cancel)
            .await
            .is_ok_and(|o| o.exit_code == 0);
        if reset_ok {
            let _ = run_process(
                "git",
                &["clean".to_owned(), "-fd".to_owned()],
                &opts,
                cancel,
            )
            .await;
        }
        reset_ok
    }

    /// Drops the run's snapshot refs so their dangling commits become collectable: the durable
    /// per-run ref, plus any per-step retry rewind refs (`refs/odin/retry/<id>/*`) and per-loop
    /// iteration refs (`refs/odin/loop/<id>/*`) a crash or cancel left behind (the happy path drops
    /// each one when its step settles). Best effort.
    pub(crate) async fn delete_snapshot_ref(
        &self,
        workdir: &Path,
        run_id: RunId,
        cancel: &CancelToken,
    ) {
        self.delete_ref(workdir, &format!("refs/odin/run/{run_id}"), cancel)
            .await;
        let opts = git_opts(workdir);
        // List the run's transient refs (a trailing `/` matches the whole hierarchy) and drop each.
        for prefix in [
            format!("refs/odin/retry/{run_id}/"),
            format!("refs/odin/loop/{run_id}/"),
        ] {
            let list = [
                "for-each-ref".to_owned(),
                "--format=%(refname)".to_owned(),
                prefix,
            ];
            if let Ok(out) = run_process("git", &list, &opts, cancel).await {
                for refname in out.stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    self.delete_ref(workdir, refname, cancel).await;
                }
            }
        }
    }

    /// Deletes a git ref (best effort), letting any commit it anchored become collectable.
    pub(crate) async fn delete_ref(&self, workdir: &Path, ref_name: &str, cancel: &CancelToken) {
        let opts = git_opts(workdir);
        let args = [
            "update-ref".to_owned(),
            "-d".to_owned(),
            ref_name.to_owned(),
        ];
        let _ = run_process("git", &args, &opts, cancel).await;
    }
}

/// `ProcessOptions` for an Odin-driven `git` call: the workdir plus
/// [`crate::provider::process::GIT_PORTABLE_ENV`] so git treats content as raw bytes (byte-stable
/// snapshots/diffs/checkouts across platforms — see that constant). Every engine git call uses it.
pub(crate) fn git_opts(workdir: &Path) -> ProcessOptions {
    ProcessOptions {
        workdir: Some(workdir.to_path_buf()),
        env: crate::provider::process::GIT_PORTABLE_ENV
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        ..ProcessOptions::default()
    }
}
