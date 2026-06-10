//! # odin-daemon
//!
//! The long-running side of **Odin**. Where [`odin_core`] defines the [`Trigger`]
//! *trait* and the [`Engine`] that executes a workflow once, this crate supplies the
//! concrete, long-lived triggers and the supervisor loop that turns a stream of
//! trigger events into runs.
//!
//! ## What's here
//!
//! - [`CronTrigger`] — a [`Trigger`] that fires on a standard 5-field cron schedule.
//! - [`WebhookServer`] / [`GithubWebhookTrigger`] — a shared HTTP server that turns signed
//!   GitHub webhook POSTs into trigger events (the event-driven slice).
//! - [`Daemon`] — owns an [`Engine`] and a set of workflows, resumes any incomplete
//!   runs on startup, then drives every registered trigger concurrently, dispatching a
//!   run per event.
//!
//! The `odind` binary is a thin runner over these: it loads a directory of workflow
//! files, builds an engine, derives triggers from each workflow's `triggers:` block,
//! and serves.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # async fn demo(engine: Arc<dyn odin_core::Engine>, workflows: Vec<odin_core::Workflow>) -> anyhow::Result<()> {
//! use odin_daemon::Daemon;
//!
//! // Build cron triggers from each workflow's declared schedule, then serve forever.
//! let daemon = Daemon::from_workflows(engine, workflows)?;
//! daemon.run().await?;
//! # Ok(())
//! # }
//! ```
//!
//! [`Trigger`]: odin_core::Trigger
//! [`Engine`]: odin_core::Engine

mod daemon;
mod dashboard;
mod metrics;
mod trigger;
mod webhook;

pub use daemon::Daemon;
pub use trigger::CronTrigger;
pub use webhook::{BoundWebhookServer, GithubWebhookTrigger, WebhookServer};
