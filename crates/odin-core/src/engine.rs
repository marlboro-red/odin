//! The [`Engine`] façade: the API embedders drive.
//!
//! The concrete implementation lands at the execution milestone; this trait and its
//! builder are fixed now so the public driving API does not churn when the executor
//! arrives. An external tool depends on this surface, not on engine internals.

use std::sync::Arc;

use async_trait::async_trait;

use crate::api::{RunInput, RunSummary};
use crate::error::{Error, Result};
use crate::ids::RunId;
use crate::ir::Workflow;
use crate::registry::Registry;
use crate::traits::Store;

/// The thing embedders drive to run workflows.
#[async_trait]
pub trait Engine: Send + Sync {
    /// Runs a workflow to completion, returning the structured summary.
    ///
    /// Validates the input against the workflow's params first, and checkpoints to the
    /// [`Store`] if the workflow is durable.
    ///
    /// # Errors
    /// Returns an [`crate::error::Error`] if validation, a plugin, or persistence fails.
    async fn run(&self, workflow: &Workflow, input: RunInput) -> Result<RunSummary>;

    /// Resumes any incomplete runs found in the [`Store`] (crash recovery).
    ///
    /// # Errors
    /// Returns an [`crate::error::Error`] if recovery fails.
    async fn resume_all(&self) -> Result<Vec<RunSummary>>;

    /// Fetches the summary of a known run id.
    ///
    /// # Errors
    /// Returns an [`crate::error::Error`] if the store read fails.
    async fn summary(&self, run_id: RunId) -> Result<Option<RunSummary>>;
}

/// Wires a [`Registry`] of plugins and a [`Store`] into a concrete engine.
#[derive(Default)]
pub struct EngineBuilder {
    registry: Registry,
    store: Option<Arc<dyn Store>>,
}

impl EngineBuilder {
    /// A new builder seeded with the built-in providers/workspaces/actions.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Registry::with_builtins(),
            store: None,
        }
    }

    /// Provides the durable store.
    #[must_use]
    pub fn store(mut self, store: Arc<dyn Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Accesses the registry to register custom plugins.
    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    /// Finalizes into a runnable engine.
    ///
    /// # Errors
    /// Until the execution milestone lands, this returns [`Error::Unimplemented`] — a
    /// *recoverable* error, never a panic, so integration code written against this frozen
    /// API can be exercised end-to-end today. Later it will also error if required plugins
    /// are missing.
    pub fn build(self) -> Result<Arc<dyn Engine>> {
        // Touch the fields so they are not dead code before the executor consumes them.
        let _ = (&self.registry, &self.store);
        Err(Error::Unimplemented {
            feature: "engine executor",
        })
    }
}
