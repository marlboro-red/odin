//! Validation diagnostics: severities, stable `ODIN###` codes, and the collected report.
//!
//! Validation never fails fast — it runs every rule and collects every [`Diagnostic`]
//! into a [`ValidationReport`], so an author sees all problems in one pass.

use std::fmt;

use serde::Serialize;

/// How serious a [`Diagnostic`] is. `Warning < Error` for sorting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The workflow is still runnable, but something is suspicious or inert.
    Warning,
    /// The workflow is invalid and must not run.
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Warning => "warning",
            Severity::Error => "error",
        })
    }
}

/// A stable, documentable diagnostic code. Each maps to exactly one validation rule.
///
/// Codes are append-only: a variant's numeric code never changes once shipped, so
/// downstream tooling (CI filters, `#[allow]`-style suppressions) can rely on them.
///
/// Serializes as its stable `ODIN###` string (e.g. `"ODIN023"`), not the variant name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagCode {
    /// ODIN001 — the workflow declares no steps.
    NoSteps,
    /// ODIN002 — a step id is empty.
    EmptyStepId,
    /// ODIN003 — two steps share an id.
    DuplicateStepId,
    /// ODIN004 — a step id is not a valid template path segment.
    InvalidStepId,
    /// ODIN005 — a `provider:` / judge / fallback ref names no known provider.
    UnknownProvider,
    /// ODIN006 — a provider step has no prompt source.
    MissingPrompt,
    /// ODIN007 — a step lists the same produced artifact twice.
    DuplicateProduces,
    /// ODIN008 — a required artifact is produced by no step.
    UnsatisfiedRequires,
    /// ODIN009 — a provider step sets both `prompt` and `prompt_file`.
    BothPromptAndFile,
    /// ODIN010 — an `action:` names no known action.
    UnknownAction,
    /// ODIN011 — a judge threshold is outside `0.0..=1.0`.
    JudgeThresholdRange,
    /// ODIN012 — a `depends_on` target names no known step.
    UnknownDependency,
    /// ODIN013 — a step depends on itself.
    SelfDependency,
    /// ODIN014 — the `depends_on` graph contains a cycle.
    DependencyCycle,
    /// ODIN015 — a required artifact's producer is not an upstream dependency.
    ArtifactOrdering,
    /// ODIN016 — a slot pool size is less than 1.
    InvalidPoolSize,
    /// ODIN017 — a template references an unknown variable.
    UnknownTemplateRef,
    /// ODIN018 — a template has a syntax error.
    TemplateSyntax,
    /// ODIN019 — a step produces the engine-reserved `DIFF` artifact.
    ReservedArtifactDiff,
    /// ODIN020 — a cron trigger schedule is not a valid 5-field expression.
    InvalidCron,
    /// ODIN021 — a step is judged by the same provider that produced it (warning).
    SameProviderJudge,
    /// ODIN022 — a param is `required` yet also has a `default` (warning).
    RequiredWithDefault,
    /// ODIN023 — `on_fallback_provider` is set but inert in v1 (warning).
    InertFallbackProvider,
    /// ODIN024 — a declared param is never referenced (warning).
    UnusedParam,
    /// ODIN025 — an unknown field at the workflow root (warning, forward-compat).
    ///
    /// (Setting a `prompt` on a non-provider step is *not* a diagnostic — it is a
    /// parse-time error, alongside the other exactly-one-of-kind violations.)
    UnknownRootField,
    /// ODIN026 — the schema minor is newer than this engine (warning).
    NewerSchemaMinor,
    /// ODIN027 — a `github_webhook` trigger maps a param that the workflow does not
    /// declare; the mapping is inert (warning).
    WebhookParamUndeclared,
}

impl DiagCode {
    /// The canonical `ODIN###` string for this code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DiagCode::NoSteps => "ODIN001",
            DiagCode::EmptyStepId => "ODIN002",
            DiagCode::DuplicateStepId => "ODIN003",
            DiagCode::InvalidStepId => "ODIN004",
            DiagCode::UnknownProvider => "ODIN005",
            DiagCode::MissingPrompt => "ODIN006",
            DiagCode::DuplicateProduces => "ODIN007",
            DiagCode::UnsatisfiedRequires => "ODIN008",
            DiagCode::BothPromptAndFile => "ODIN009",
            DiagCode::UnknownAction => "ODIN010",
            DiagCode::JudgeThresholdRange => "ODIN011",
            DiagCode::UnknownDependency => "ODIN012",
            DiagCode::SelfDependency => "ODIN013",
            DiagCode::DependencyCycle => "ODIN014",
            DiagCode::ArtifactOrdering => "ODIN015",
            DiagCode::InvalidPoolSize => "ODIN016",
            DiagCode::UnknownTemplateRef => "ODIN017",
            DiagCode::TemplateSyntax => "ODIN018",
            DiagCode::ReservedArtifactDiff => "ODIN019",
            DiagCode::InvalidCron => "ODIN020",
            DiagCode::SameProviderJudge => "ODIN021",
            DiagCode::RequiredWithDefault => "ODIN022",
            DiagCode::InertFallbackProvider => "ODIN023",
            DiagCode::UnusedParam => "ODIN024",
            DiagCode::UnknownRootField => "ODIN025",
            DiagCode::NewerSchemaMinor => "ODIN026",
            DiagCode::WebhookParamUndeclared => "ODIN027",
        }
    }

    /// The severity this rule conventionally emits at.
    #[must_use]
    pub fn severity(self) -> Severity {
        match self {
            DiagCode::SameProviderJudge
            | DiagCode::RequiredWithDefault
            | DiagCode::InertFallbackProvider
            | DiagCode::UnusedParam
            | DiagCode::UnknownRootField
            | DiagCode::NewerSchemaMinor
            | DiagCode::WebhookParamUndeclared => Severity::Warning,
            _ => Severity::Error,
        }
    }
}

impl fmt::Display for DiagCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for DiagCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// A single validation finding: a coded message anchored at a structural location.
#[derive(Clone, Debug, Serialize)]
pub struct Diagnostic {
    /// How serious this finding is.
    pub severity: Severity,
    /// The stable rule code.
    pub code: DiagCode,
    /// Human-readable message naming the offender and how to fix it.
    pub message: String,
    /// Structural pointer into the workflow, e.g. `steps[2].depends_on[0]`.
    pub pointer: String,
    /// Optional extra help (e.g. a "did you mean" or a list of valid names).
    pub help: Option<String>,
}

impl Diagnostic {
    /// Builds a diagnostic at the code's conventional severity.
    pub fn new(code: DiagCode, pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: code.severity(),
            code,
            message: message.into(),
            pointer: pointer.into(),
            help: None,
        }
    }

    /// Attaches a help string (builder style).
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{}]: {}", self.severity, self.code, self.message)?;
        if !self.pointer.is_empty() {
            write!(f, "\n  --> {}", self.pointer)?;
        }
        if let Some(help) = &self.help {
            write!(f, "\n  help: {help}")?;
        }
        Ok(())
    }
}

/// The collected output of validation: every [`Diagnostic`] from a single pass.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ValidationReport {
    /// All findings, in the order the rules emitted them.
    pub diagnostics: Vec<Diagnostic>,
}

impl ValidationReport {
    /// True if there are no diagnostics at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// True if any diagnostic is an [`Severity::Error`].
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// Number of error-severity diagnostics.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    /// Number of warning-severity diagnostics.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count()
    }

    /// Iterates diagnostics matching a given code (handy in tests).
    pub fn by_code(&self, code: DiagCode) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(move |d| d.code == code)
    }

    /// True if any diagnostic carries the given code.
    #[must_use]
    pub fn contains(&self, code: DiagCode) -> bool {
        self.diagnostics.iter().any(|d| d.code == code)
    }

    /// Converts to a `Result`: `Err(Error::Validation(self))` if any errors are present.
    ///
    /// Warnings alone do not fail the conversion.
    pub fn into_result(self) -> crate::error::Result<()> {
        if self.has_errors() {
            Err(crate::error::Error::Validation(self))
        } else {
            Ok(())
        }
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for d in &self.diagnostics {
            writeln!(f, "{d}")?;
        }
        write!(
            f,
            "{} error(s), {} warning(s)",
            self.error_count(),
            self.warning_count()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{DiagCode, Diagnostic, Severity, ValidationReport};

    #[test]
    fn codes_are_unique_and_well_formed() {
        // Exhaustive list mirrors the public enum; if a variant is added without a
        // code, `as_str` won't compile, so this also guards the mapping.
        let all = [
            DiagCode::NoSteps,
            DiagCode::EmptyStepId,
            DiagCode::DuplicateStepId,
            DiagCode::InvalidStepId,
            DiagCode::UnknownProvider,
            DiagCode::MissingPrompt,
            DiagCode::DuplicateProduces,
            DiagCode::UnsatisfiedRequires,
            DiagCode::BothPromptAndFile,
            DiagCode::UnknownAction,
            DiagCode::JudgeThresholdRange,
            DiagCode::UnknownDependency,
            DiagCode::SelfDependency,
            DiagCode::DependencyCycle,
            DiagCode::ArtifactOrdering,
            DiagCode::InvalidPoolSize,
            DiagCode::UnknownTemplateRef,
            DiagCode::TemplateSyntax,
            DiagCode::ReservedArtifactDiff,
            DiagCode::InvalidCron,
            DiagCode::SameProviderJudge,
            DiagCode::RequiredWithDefault,
            DiagCode::InertFallbackProvider,
            DiagCode::UnusedParam,
            DiagCode::UnknownRootField,
            DiagCode::NewerSchemaMinor,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for c in all {
            assert!(c.as_str().starts_with("ODIN"));
            assert!(seen.insert(c.as_str()), "duplicate code {}", c.as_str());
        }
        assert_eq!(seen.len(), 26);
    }

    #[test]
    fn report_separates_errors_and_warnings() {
        let mut r = ValidationReport::default();
        r.diagnostics
            .push(Diagnostic::new(DiagCode::NoSteps, "", "no steps"));
        r.diagnostics
            .push(Diagnostic::new(DiagCode::UnusedParam, "params.x", "unused"));
        assert!(r.has_errors());
        assert_eq!(r.error_count(), 1);
        assert_eq!(r.warning_count(), 1);
        assert_eq!(DiagCode::UnusedParam.severity(), Severity::Warning);
        assert!(r.contains(DiagCode::NoSteps));
        assert!(r.into_result().is_err());
    }

    #[test]
    fn warnings_only_pass_into_result() {
        let mut r = ValidationReport::default();
        r.diagnostics
            .push(Diagnostic::new(DiagCode::UnusedParam, "params.x", "unused"));
        assert!(!r.has_errors());
        assert!(r.into_result().is_ok());
    }
}
