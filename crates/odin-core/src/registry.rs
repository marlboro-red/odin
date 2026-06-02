//! The registry: maps string keys to boxed trait objects.
//!
//! Built-ins ship registered; third parties `register_*` with zero core changes. The
//! validator checks workflow references against the live registry via
//! [`Registry::known_names`].

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::traits::{Action, Provider, Trigger, Workspace};
use crate::validate::KnownNames;

/// Resolves provider/workspace/action/trigger names to implementations.
#[derive(Default, Clone)]
pub struct Registry {
    providers: BTreeMap<String, Arc<dyn Provider>>,
    workspaces: BTreeMap<String, Arc<dyn Workspace>>,
    actions: BTreeMap<String, Arc<dyn Action>>,
    triggers: BTreeMap<String, Arc<dyn Trigger>>,
}

impl Registry {
    /// A registry with all built-in providers/workspaces/actions/triggers registered.
    ///
    /// Currently registers the [`ClaudeProvider`](crate::provider::ClaudeProvider); the
    /// codex/copilot providers, the worktree/slot-pool workspaces, and the
    /// github/git/shell actions arrive in later milestones. Parse-only validation (the
    /// `ir` feature) instead uses [`KnownNames::builtin`].
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register_provider(Arc::new(crate::provider::ClaudeProvider::new()));
        registry
            .register_action(Arc::new(crate::action::ShellExec))
            .register_action(Arc::new(crate::action::GitCommit))
            .register_action(Arc::new(crate::action::GitPush))
            .register_action(Arc::new(crate::action::OpenPr));
        registry
    }

    /// Registers a provider under its [`Provider::id`].
    pub fn register_provider(&mut self, p: Arc<dyn Provider>) -> &mut Self {
        self.providers.insert(p.id().as_str().to_owned(), p);
        self
    }

    /// Registers an action under its [`Action::name`].
    pub fn register_action(&mut self, a: Arc<dyn Action>) -> &mut Self {
        self.actions.insert(a.name().to_owned(), a);
        self
    }

    /// Registers a workspace under its [`Workspace::kind`].
    pub fn register_workspace(&mut self, w: Arc<dyn Workspace>) -> &mut Self {
        self.workspaces.insert(w.kind().to_owned(), w);
        self
    }

    /// Registers a trigger under its [`Trigger::kind`].
    pub fn register_trigger(&mut self, t: Arc<dyn Trigger>) -> &mut Self {
        self.triggers.insert(t.kind().to_owned(), t);
        self
    }

    /// Looks up a provider by name.
    #[must_use]
    pub fn provider(&self, name: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(name)
    }

    /// Looks up an action by name.
    #[must_use]
    pub fn action(&self, name: &str) -> Option<&Arc<dyn Action>> {
        self.actions.get(name)
    }

    /// Looks up a workspace by name.
    #[must_use]
    pub fn workspace(&self, name: &str) -> Option<&Arc<dyn Workspace>> {
        self.workspaces.get(name)
    }

    /// Looks up a trigger by name.
    #[must_use]
    pub fn trigger(&self, name: &str) -> Option<&Arc<dyn Trigger>> {
        self.triggers.get(name)
    }

    /// The set of registered provider/action names, for validating a workflow against
    /// this concrete registry (so third-party plugins are recognized).
    ///
    /// Trigger and workspace names are intentionally omitted: a workflow references
    /// providers and actions by name in steps, but its `triggers:` are declared
    /// structurally (`type: github_webhook`) and dispatched by the daemon, not resolved
    /// by string key during step validation.
    #[must_use]
    pub fn known_names(&self) -> KnownNames<'_> {
        KnownNames {
            providers: self.providers.keys().map(String::as_str).collect(),
            actions: self.actions.keys().map(String::as_str).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Registry;

    #[test]
    fn builtins_register_claude_and_actions() {
        let r = Registry::with_builtins();
        assert!(r.provider("claude").is_some());
        assert!(r.known_names().providers.contains(&"claude"));
        // Not yet implemented providers are absent.
        assert!(r.provider("codex").is_none());
        // Built-in actions are registered.
        for name in ["shell.exec", "git.commit", "git.push", "github.open_pr"] {
            assert!(r.action(name).is_some(), "{name} should be registered");
        }
    }

    #[test]
    fn empty_registry_has_no_names() {
        let r = Registry::default();
        assert!(r.known_names().providers.is_empty());
        assert!(r.provider("claude").is_none());
    }
}
