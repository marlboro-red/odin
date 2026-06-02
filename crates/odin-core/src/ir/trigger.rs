//! Declared triggers. v1 *executes* only `Manual`; others parse & validate now so a
//! workflow file is forward-compatible with the daemon milestone.

use serde::{Deserialize, Serialize};

/// A declared trigger. `#[non_exhaustive]` so new kinds are additive.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TriggerDecl {
    /// Run on explicit user/API request. The only kind wired in v1.
    ///
    /// A `#[non_exhaustive]` struct variant (with no fields yet) rather than a bare unit
    /// variant, so future manual-trigger options (an approval gate, an allowed-actors
    /// list) are additive instead of a breaking change.
    #[non_exhaustive]
    Manual {},
    /// A GitHub webhook event. Declaration parsed now; dispatch in the daemon milestone.
    GithubWebhook(GithubWebhookDecl),
    /// A cron schedule. Declaration parsed now; dispatch in the daemon milestone.
    Cron(CronDecl),
}

/// Declaration of a GitHub webhook trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct GithubWebhookDecl {
    /// Event names, e.g. `["pull_request.opened", "issues.labeled"]`.
    pub events: Vec<String>,
    /// Optional `owner/repo` filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Declaration of a cron trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CronDecl {
    /// Standard 5-field cron expression. Validity is checked at validate-time (`ODIN020`).
    pub schedule: String,
}

#[cfg(test)]
mod tests {
    use super::TriggerDecl;

    #[test]
    fn parses_all_kinds() {
        let y = r#"
- type: manual
- type: github_webhook
  events: ["issues.labeled"]
  repo: marlboro-red/odin
- type: cron
  schedule: "0 3 * * 1"
"#;
        let triggers: Vec<TriggerDecl> = serde_yaml_ng::from_str(y).unwrap();
        assert_eq!(triggers.len(), 3);
        assert!(matches!(triggers[0], TriggerDecl::Manual { .. }));
        assert!(matches!(triggers[1], TriggerDecl::GithubWebhook(_)));
        assert!(matches!(triggers[2], TriggerDecl::Cron(_)));

        // `type: manual` round-trips through the struct variant.
        let back = serde_yaml_ng::to_string(&triggers[0]).unwrap();
        assert!(back.contains("type: manual"), "got: {back}");
    }
}
