//! The Workflow intermediate representation: serde-deserializable types mirroring YAML.
//!
//! Parsing is intentionally *separate* from validation. [`Workflow::from_yaml_str`] is
//! fail-fast and only catches structural problems (bad YAML, an unknown nested field, a
//! step with two kinds). Semantic checks — unknown providers, dependency cycles, bad
//! template references — are collected all-at-once by [`crate::validate::validate`].

pub mod duration;
pub mod params;
pub mod step;
pub mod trigger;
pub mod workflow;
pub mod workspace;

pub use duration::HumanDuration;
pub use params::{ParamSpec, ParamType};
pub use step::{
    ActionStep, ApprovalStep, Artifacts, Backoff, CaseBranch, CaseStep, FeedbackMode, JudgeSpec,
    ProviderStep, RetrySpec, RunStep, Step, StepKind,
};
pub use trigger::{CronDecl, GithubWebhookDecl, TriggerDecl};
pub use workflow::{
    CURRENT_SCHEMA_MAJOR, CURRENT_SCHEMA_MINOR, SchemaVersion, Workflow, WorkflowDefaults,
};
pub use workspace::{ResetMode, SlotPoolConfig, WorkspaceConfig, WorktreeConfig};

use std::path::Path;

use crate::error::{Error, Result};

impl Workflow {
    /// Parses a workflow from a YAML string.
    ///
    /// **Parse only** — call [`crate::validate::validate`] afterward for semantic
    /// checks. Rejects an unsupported schema **major** before returning, so a v2 file
    /// fails loudly rather than silently mis-binding to v1 fields.
    ///
    /// A newer schema **minor** is *not* flagged here — it loads silently. That warning
    /// (`ODIN026`) is emitted by [`crate::validate::validate`], so an embedder that wants
    /// the signal must run validation (the CLI and engine entry points always do).
    ///
    /// # Errors
    /// Returns [`Error::Parse`] on malformed YAML or a structural violation, and
    /// [`Error::SchemaVersion`] if the declared major is not understood.
    pub fn from_yaml_str(src: &str) -> Result<Self> {
        let wf: Workflow = serde_yaml_ng::from_str(src)?;
        if wf.schema_version.major != CURRENT_SCHEMA_MAJOR {
            return Err(Error::SchemaVersion {
                found_major: wf.schema_version.major,
                supported_major: CURRENT_SCHEMA_MAJOR,
            });
        }
        Ok(wf)
    }

    /// Parses a workflow from a file path.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the file cannot be read, plus any error from
    /// [`Workflow::from_yaml_str`].
    pub fn from_yaml_path(path: impl AsRef<Path>) -> Result<Self> {
        let src = std::fs::read_to_string(path)?;
        Self::from_yaml_str(&src)
    }
}

#[cfg(test)]
mod tests {
    use super::Workflow;
    use crate::error::Error;

    #[test]
    fn minimal_workflow_parses() {
        let y = "name: hello\nsteps:\n  - id: a\n    run: ./x.sh\n";
        let wf = Workflow::from_yaml_str(y).unwrap();
        assert_eq!(wf.name.as_str(), "hello");
        assert_eq!(wf.steps.len(), 1);
        assert!(wf.durable, "durable defaults to true");
    }

    #[test]
    fn unknown_major_is_rejected() {
        let y = "schema_version: \"2.0\"\nname: x\nsteps: []\n";
        let err = Workflow::from_yaml_str(y).unwrap_err();
        assert!(matches!(err, Error::SchemaVersion { found_major: 2, .. }));
    }

    #[test]
    fn unknown_root_key_is_tolerated_at_parse_time() {
        // Root tolerance (warned later as ODIN025), unlike nested deny_unknown_fields.
        let y = "name: x\nfuture_field: 1\nsteps:\n  - id: a\n    run: ./x.sh\n";
        assert!(Workflow::from_yaml_str(y).is_ok());
    }
}
