//! The five integration traits — the pluggable surface Odin is built on.
//!
//! Each is object-safe (`Arc<dyn Trait>`, via `async-trait`), has one required method
//! plus defaulted optionals, exchanges owned/serializable context-outcome structs so
//! implementations can live in other crates, and returns its own focused error type.
//!
//! | Trait | Responsibility | Mock (with the `mock` feature) |
//! |-------|----------------|------|
//! | [`Provider`] | invoke a coding-agent CLI | `EchoProvider` |
//! | [`Workspace`] | provision a per-run workdir | `TmpWorkspace` |
//! | [`Store`] | durably persist run state | `MemStore` |
//! | [`Action`] | perform a named side-effect | `NoopAction` |
//! | [`Trigger`] | emit run-starting events | `ScriptedTrigger` |

pub mod action;
pub mod provider;
pub mod store;
pub mod trigger;
pub mod workspace;

pub use action::{Action, ActionCtx, ActionOutcome};
pub use provider::{CancelToken, InvocationCtx, InvocationOutcome, Provider};
pub use store::{
    LoopProgress, PrunePolicy, PruneReport, PrunedCount, RunEvent, RunState, RunStatusCount,
    StepState, Store, StoreMetrics,
};
pub use trigger::{Trigger, TriggerEvent};
pub use workspace::{AcquireCtx, Workspace, WorkspaceHandle};
