//! The [`Trigger`] runtime trait (distinct from [`crate::ir::TriggerDecl`], which is the
//! declarative config).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::api::RunInput;
use crate::error::TriggerError;
use crate::ids::WorkflowId;

/// A source of run-starting events. v1 ships only a manual trigger; the daemon hosts
/// long-lived ones (webhook, cron) later. Pull-based, so manual (one event then end),
/// cron (timer), and webhook (server-pushed) all fit one shape.
#[async_trait]
pub trait Trigger: Send + Sync {
    /// Stable name (`"manual"`, `"github_webhook"`, `"cron"`).
    fn kind(&self) -> &str;

    /// Blocks until the next event, or returns `Ok(None)` when the source is exhausted
    /// (manual = one event then `None`; cron/webhook never return `None`). Cancel-safe.
    ///
    /// # Errors
    /// Returns a [`TriggerError`] if the underlying source fails.
    async fn next_event(&mut self) -> Result<Option<TriggerEvent>, TriggerError>;
}

/// A fired trigger, ready to become a run. Carries a [`RunInput`] so the same run path
/// serves manual and triggered runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TriggerEvent {
    /// Which declared trigger fired.
    pub source: String,
    /// Which workflow to start.
    pub workflow: WorkflowId,
    /// The assembled run input (trigger payload + params).
    pub input: RunInput,
}
