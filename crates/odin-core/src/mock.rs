//! Minimal `Noop`/`Mock` implementations of the five integration traits.
//!
//! Available with the `mock` feature (and in this crate's own tests). They exist to
//! prove the trait surface is object-safe and one-file-implementable, and to let
//! downstream crates test the engine without real CLIs, git, or a database.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::api::SideEffect;
use crate::error::{ActionError, ProviderError, StoreError, TriggerError, WorkspaceError};
use crate::ids::{ProviderRef, RunId};
use crate::traits::{
    AcquireCtx, Action, ActionCtx, ActionOutcome, InvocationCtx, InvocationOutcome, Provider,
    RunEvent, RunState, Store, Trigger, TriggerEvent, Workspace, WorkspaceHandle,
};

/// A provider that echoes its rendered prompt back as stdout.
pub struct EchoProvider(ProviderRef);

impl EchoProvider {
    /// Creates an echo provider answering to `id`.
    pub fn new(id: impl Into<ProviderRef>) -> Self {
        Self(id.into())
    }
}

#[async_trait]
impl Provider for EchoProvider {
    fn id(&self) -> ProviderRef {
        self.0.clone()
    }

    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError> {
        Ok(InvocationOutcome::success(ctx.prompt.unwrap_or_default()))
    }
}

/// A workspace that hands back a fixed path and never cleans up.
pub struct TmpWorkspace(pub std::path::PathBuf);

#[async_trait]
impl Workspace for TmpWorkspace {
    // The trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn kind(&self) -> &str {
        "tmp"
    }

    async fn acquire(&self, ctx: AcquireCtx) -> Result<WorkspaceHandle, WorkspaceError> {
        Ok(WorkspaceHandle::new(ctx.run_id, self.0.clone(), None, ""))
    }

    async fn release(&self, _handle: WorkspaceHandle) -> Result<(), WorkspaceError> {
        Ok(())
    }
}

/// An in-memory store. Proves the durability contract without SQLite.
#[derive(Default)]
pub struct MemStore {
    runs: Mutex<HashMap<RunId, RunState>>,
}

impl MemStore {
    /// Creates an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for MemStore {
    async fn checkpoint(&self, state: &RunState) -> Result<(), StoreError> {
        self.runs
            .lock()
            .unwrap()
            .insert(state.run_id, state.clone());
        Ok(())
    }

    async fn append_event(&self, _run_id: RunId, _event: &RunEvent) -> Result<(), StoreError> {
        Ok(())
    }

    async fn load_incomplete(&self) -> Result<Vec<RunState>, StoreError> {
        Ok(self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|s| !s.status.is_terminal())
            .cloned()
            .collect())
    }

    async fn load_run(&self, run_id: RunId) -> Result<Option<RunState>, StoreError> {
        Ok(self.runs.lock().unwrap().get(&run_id).cloned())
    }
}

/// An action that echoes its `with:` args into its outputs.
pub struct NoopAction(pub String);

#[async_trait]
impl Action for NoopAction {
    fn name(&self) -> &str {
        &self.0
    }

    async fn run(&self, ctx: ActionCtx) -> Result<ActionOutcome, ActionError> {
        Ok(ActionOutcome {
            exit_code: 0,
            outputs: ctx.args,
            stderr: String::new(),
            side_effects: Vec::<SideEffect>::new(),
        })
    }
}

/// A trigger that replays a fixed script of events, then returns `None`.
pub struct ScriptedTrigger(Mutex<VecDeque<TriggerEvent>>);

impl ScriptedTrigger {
    /// Creates a trigger that will emit `events` in order.
    pub fn new(events: impl IntoIterator<Item = TriggerEvent>) -> Self {
        Self(Mutex::new(events.into_iter().collect()))
    }
}

#[async_trait]
impl Trigger for ScriptedTrigger {
    // The trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn kind(&self) -> &str {
        "scripted"
    }

    async fn next_event(&mut self) -> Result<Option<TriggerEvent>, TriggerError> {
        Ok(self.0.get_mut().unwrap().pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::{EchoProvider, MemStore};
    use crate::api::{RunInput, RunStatus};
    use crate::ids::{RunId, StepId, WorkflowId};
    use crate::traits::{CancelToken, InvocationCtx, Provider, RunState, Store};
    use indexmap::IndexMap;
    use std::sync::Arc;

    fn ctx() -> InvocationCtx {
        InvocationCtx {
            step_id: StepId::new("s"),
            workdir: std::path::PathBuf::from("/tmp"),
            prompt: Some("hello".to_owned()),
            inputs: IndexMap::new(),
            timeout: None,
            cancel: CancelToken::new(),
        }
    }

    #[tokio::test]
    async fn provider_is_object_safe_and_echoes() {
        let p: Arc<dyn Provider> = Arc::new(EchoProvider::new("echo"));
        assert_eq!(p.id().as_str(), "echo");
        let out = p.invoke(ctx()).await.unwrap();
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.exit_code, 0);
    }

    fn run_state(status: RunStatus) -> RunState {
        RunState {
            run_id: RunId::new(),
            workflow: WorkflowId::new("w"),
            schema_major: 1,
            status,
            error: None,
            steps: IndexMap::new(),
            artifacts: IndexMap::new(),
            provider_versions: IndexMap::new(),
            approvals: IndexMap::new(),
            input: RunInput::manual(),
            workspace: None,
            base_commit: None,
            snapshot: None,
            loop_state: IndexMap::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn store_round_trips_and_filters_incomplete() {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let running = run_state(RunStatus::Running);
        let done = run_state(RunStatus::Succeeded);
        let running_id = running.run_id;
        store.checkpoint(&running).await.unwrap();
        store.checkpoint(&done).await.unwrap();

        let incomplete = store.load_incomplete().await.unwrap();
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0].run_id, running_id);
        assert!(store.load_run(running_id).await.unwrap().is_some());
    }
}
