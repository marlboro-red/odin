//! Newtype identifiers.
//!
//! Stringly-typed fields are the number-one source of silent workflow bugs; these
//! newtypes make a [`StepId`] impossible to confuse with an [`ArtifactName`] at the
//! type level while still deserializing transparently from plain YAML/JSON strings.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Declares a transparent `String` newtype id with the common impls.
macro_rules! string_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wraps a raw string as this id type.
            #[must_use]
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            /// Borrows the underlying string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), &self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

string_id!(
    /// Author-assigned, stable identity of a workflow (its `name`).
    WorkflowId
);
string_id!(
    /// Author-assigned, stable id of a step. Unique within a workflow.
    StepId
);
string_id!(
    /// Name of a named artifact, e.g. `DIFF`. Convention: `UPPER_SNAKE`.
    ArtifactName
);
string_id!(
    /// Name of a workflow input parameter.
    ParamName
);
string_id!(
    /// Name of a per-step gate command.
    GateName
);
string_id!(
    /// Reference to a provider by registry key, e.g. `"claude"`.
    ///
    /// Resolved against the [`crate::registry::Registry`] at run construction and
    /// validated statically (rule `ODIN005`) with a "did you mean" hint.
    ProviderRef
);

/// A run-instance identifier (UUID v4). A distinct type from [`WorkflowId`]: a
/// workflow is a definition, a run is one execution of it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub uuid::Uuid);

impl RunId {
    /// Generates a fresh random run id.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::str::FromStr for RunId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(uuid::Uuid::parse_str(s)?))
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{RunId, StepId};

    #[test]
    fn display_and_as_str_match() {
        let id = StepId::new("plan");
        assert_eq!(id.as_str(), "plan");
        assert_eq!(id.to_string(), "plan");
    }

    #[test]
    fn debug_is_typed() {
        let id = StepId::from("plan");
        assert_eq!(format!("{id:?}"), r#"StepId("plan")"#);
    }

    #[test]
    fn serde_is_transparent() {
        let id = StepId::new("build");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""build""#);
        let back: StepId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn run_ids_are_unique() {
        assert_ne!(RunId::new(), RunId::new());
    }
}
