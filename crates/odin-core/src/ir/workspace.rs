//! Pluggable workspace provisioning configuration.

use serde::{Deserialize, Serialize};

/// How a run gets its working directory. Internally tagged on `type`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkspaceConfig {
    /// One throwaway `git worktree` per run. The default.
    Worktree(WorktreeConfig),
    /// A pool of N pre-cloned slots; claim / release / reset between runs.
    SlotPool(SlotPoolConfig),
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self::Worktree(WorktreeConfig::default())
    }
}

/// Configuration for the per-run worktree workspace.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WorktreeConfig {
    /// Base branch/ref the worktree is cut from. Defaults to repo `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// Configuration for the slot-pool workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SlotPoolConfig {
    /// Number of clones in the pool. Must be `>= 1` (rule `ODIN016`).
    pub pool: u16,
    /// How a slot is reset before reuse.
    #[serde(default)]
    pub reset: ResetMode,
    /// Base branch/ref slots are cut from. Defaults to repo `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// How a slot pool cleans a slot before reuse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResetMode {
    /// `git reset --hard && git clean -fdx`. Fast; the default.
    #[default]
    GitClean,
    /// Re-clone from origin. Slow; pristine.
    Reclone,
}

#[cfg(test)]
mod tests {
    use super::{ResetMode, WorkspaceConfig};

    #[test]
    fn default_is_worktree() {
        assert!(matches!(
            WorkspaceConfig::default(),
            WorkspaceConfig::Worktree(_)
        ));
    }

    #[test]
    fn slot_pool_parses() {
        let y = "type: slot_pool\npool: 4\nreset: reclone\n";
        let cfg: WorkspaceConfig = serde_yaml_ng::from_str(y).unwrap();
        match cfg {
            WorkspaceConfig::SlotPool(s) => {
                assert_eq!(s.pool, 4);
                assert_eq!(s.reset, ResetMode::Reclone);
            }
            WorkspaceConfig::Worktree(_) => panic!("expected slot_pool"),
        }
    }

    #[test]
    fn unknown_field_in_leaf_is_rejected() {
        let y = "type: worktree\nbranch: main\n"; // `branch` is not a field (it's `base`)
        assert!(serde_yaml_ng::from_str::<WorkspaceConfig>(y).is_err());
    }
}
