//! The root workflow type and its metadata.

use std::num::NonZeroUsize;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::duration::HumanDuration;
use super::params::ParamSpec;
use super::step::{RetrySpec, Step};
use super::trigger::TriggerDecl;
use super::workspace::WorkspaceConfig;
use crate::ids::{ParamName, WorkflowId};

/// The schema major this engine speaks. Bump only on a breaking IR change.
pub const CURRENT_SCHEMA_MAJOR: u16 = 1;

/// The schema minor this engine speaks. A workflow declaring a newer minor still loads
/// but warns (`ODIN026`), since it may use additive fields this engine ignores.
pub const CURRENT_SCHEMA_MINOR: u16 = 0;

/// A parsed, **not-yet-validated** workflow definition. Mirrors the YAML 1:1.
///
/// Unknown keys at the root are tolerated (and surfaced as a warning, `ODIN025`) so a
/// file authored for a newer schema minor still loads; unknown keys in nested config
/// are hard parse errors.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Workflow {
    /// Schema version of the workflow *file format*. Defaults to the current major.minor.
    #[serde(default)]
    pub schema_version: SchemaVersion,

    /// Stable identity and display name of this workflow.
    pub name: WorkflowId,

    /// Author's semantic version of the workflow content. Opaque to the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Free-form labels for categorizing/filtering this workflow (e.g. in the recipe catalog).
    /// Normalized on parse: each tag is trimmed and lowercased, empties are dropped, and
    /// duplicates are collapsed (first occurrence wins, author order otherwise preserved).
    /// Malformed tags are surfaced by validation (`ODIN045`) but never block a run.
    #[serde(
        default,
        deserialize_with = "de_tags",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub tags: Vec<String>,

    /// Whether runs of this workflow are checkpointed to the [`crate::traits::Store`].
    #[serde(default = "default_true")]
    pub durable: bool,

    /// How per-run workspaces are provisioned. Defaults to a per-run git worktree.
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    /// Declared triggers. Empty = manual-only. Non-manual triggers (webhook/cron) are evaluated
    /// by the `odind` daemon; the core engine runs manual invocations directly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<TriggerDecl>,

    /// Input parameter schema, keyed by name. Insertion order preserved.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub params: IndexMap<ParamName, ParamSpec>,

    /// The steps. A DAG via each step's `depends_on`; the first executor walks a
    /// topological order. Non-empty is enforced by validation (`ODIN001`), not parsing.
    pub steps: Vec<Step>,

    /// Default retry/timeout applied to steps that omit their own.
    #[serde(default, skip_serializing_if = "WorkflowDefaults::is_empty")]
    pub defaults: WorkflowDefaults,

    /// Maximum steps executing at once within a run. Omitted / `1` = sequential (the
    /// default). When `> 1`, independent steps run concurrently up to this many; steps in
    /// the shared workdir run exclusively, while `scratch: true` steps (isolated worktrees)
    /// run in parallel. The user asserts that concurrent shared-workdir steps don't conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<NonZeroUsize>,
}

fn default_true() -> bool {
    true
}

/// Deserializes and **normalizes** workflow tags: trim, lowercase (ASCII), drop empties, and
/// collapse duplicates keeping first occurrence (so author order survives). The raw tokens are
/// re-read from source by validation (`ODIN045`) to warn about anything normalization had to fix;
/// `to_ascii_lowercase` (not `to_lowercase`) is deliberate so a non-ASCII tag isn't Unicode-folded
/// into something the `[a-z0-9._-]` charset check then flags as its own normalization.
fn de_tags<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(d)?;
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for tag in raw {
        let norm = tag.trim().to_ascii_lowercase();
        if !norm.is_empty() && !out.contains(&norm) {
            out.push(norm);
        }
    }
    Ok(out)
}

/// Workflow-level defaults applied to steps that don't override them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WorkflowDefaults {
    /// Default per-step timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<HumanDuration>,
    /// Default retry policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySpec>,
}

impl WorkflowDefaults {
    /// True if no defaults are set (used to skip serialization).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.timeout.is_none() && self.retry.is_none()
    }
}

/// `MAJOR.MINOR` schema version of the file format. Only `major` gates compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SchemaVersion {
    /// Breaking version. The engine refuses majors it does not know.
    pub major: u16,
    /// Additive version. A newer minor loads with a warning (`ODIN026`).
    pub minor: u16,
}

impl Default for SchemaVersion {
    fn default() -> Self {
        Self {
            major: CURRENT_SCHEMA_MAJOR,
            minor: 0,
        }
    }
}

impl TryFrom<String> for SchemaVersion {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let (maj, min) = s
            .split_once('.')
            .ok_or_else(|| format!("expected MAJOR.MINOR, got {s:?}"))?;
        Ok(Self {
            major: maj
                .parse()
                .map_err(|_| format!("invalid major in schema_version {s:?}"))?,
            minor: min
                .parse()
                .map_err(|_| format!("invalid minor in schema_version {s:?}"))?,
        })
    }
}

impl From<SchemaVersion> for String {
    fn from(v: SchemaVersion) -> String {
        format!("{}.{}", v.major, v.minor)
    }
}

#[cfg(test)]
mod tests {
    use super::{CURRENT_SCHEMA_MAJOR, SchemaVersion};

    #[test]
    fn schema_version_round_trips() {
        let v = SchemaVersion::try_from("1.3".to_owned()).unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 3);
        assert_eq!(String::from(v), "1.3");
    }

    #[test]
    fn schema_version_default_is_current() {
        assert_eq!(SchemaVersion::default().major, CURRENT_SCHEMA_MAJOR);
    }

    #[test]
    fn schema_version_rejects_garbage() {
        assert!(SchemaVersion::try_from("x".to_owned()).is_err());
        assert!(SchemaVersion::try_from("1.x".to_owned()).is_err());
    }
}
