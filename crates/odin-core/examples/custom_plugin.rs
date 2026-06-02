//! Implementing & registering **custom plugins** against odin-core's public API.
//!
//! Writes a custom [`Provider`] and a custom [`Action`] (recording a [`SideEffect`]), registers
//! them with [`EngineBuilder`], and runs a two-step workflow that uses them — then reads the
//! typed [`RunStatus`]/side-effects off the summary. Everything here is `odin_core::` public
//! API, imported from the crate root (including the [`async_trait`] re-export, so you don't add
//! or version-match `async-trait` yourself).
//!
//! Because a Cargo example compiles as its own crate, `#[non_exhaustive]` applies exactly as it
//! would to any downstream integrator — so this doubles as proof that the public construction
//! surface (`InvocationOutcome::success`, `ActionOutcome::success`/`with_*`, `SideEffect::*`) is
//! sufficient to author a plugin without reaching into the crate.
//!
//! ```text
//! cargo run -p odin-core --example custom_plugin
//! ```
#![allow(clippy::doc_markdown)]

use std::process::Command;
use std::sync::Arc;

use odin_core::{
    Action, ActionCtx, ActionError, ActionOutcome, EngineBuilder, InvocationCtx, InvocationOutcome,
    Provider, ProviderError, ProviderRef, RunInput, RunStatus, SideEffect, Workflow, async_trait,
};

/// A custom provider: uppercases its prompt and returns it as the step's stdout. A real
/// provider would shell out to a coding-agent CLI here (see the built-in `ClaudeProvider`).
struct ShoutProvider;

#[async_trait]
impl Provider for ShoutProvider {
    fn id(&self) -> ProviderRef {
        ProviderRef::new("shout") // the key a step's `provider:` matches
    }

    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError> {
        let said = ctx.prompt.unwrap_or_default().to_uppercase();
        Ok(InvocationOutcome::success(said))
    }
}

/// A custom action: records a comment side-effect (surfaced in the run summary) and one output.
struct CommentAction;

#[async_trait]
impl Action for CommentAction {
    // The trait fixes the return type to `&str`, so the literal can't be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "demo.comment" // the key a step's `action:` matches
    }

    async fn run(&self, _ctx: ActionCtx) -> Result<ActionOutcome, ActionError> {
        Ok(ActionOutcome::success()
            .with_side_effect(SideEffect::comment("https://example.test/c/1"))
            .with_output("commented", true))
    }
}

const WORKFLOW: &str = r#"
name: custom-plugin-demo
workspace: { type: worktree }
steps:
  - id: shout
    provider: shout
    prompt: "hello from a custom provider"
  - id: comment
    action: demo.comment
    depends_on: [shout]
"#;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A throwaway git repo with one commit, so the worktree workspace has a HEAD to cut from.
    let repo = tempfile::tempdir()?;
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .env("GIT_AUTHOR_NAME", "odin")
            .env("GIT_AUTHOR_EMAIL", "odin@example.test")
            .env("GIT_COMMITTER_NAME", "odin")
            .env("GIT_COMMITTER_EMAIL", "odin@example.test")
            .output()
    };
    git(&["init", "-q", "-b", "main"])?;
    git(&["commit", "-q", "--allow-empty", "-m", "init"])?;

    // Register the custom plugins alongside the built-ins, then build the engine.
    let mut builder = EngineBuilder::new().repo(repo.path());
    builder
        .registry_mut()
        .register_provider(Arc::new(ShoutProvider))
        .register_action(Arc::new(CommentAction));
    let engine = builder.build()?;

    // The engine validates `provider: shout` / `action: demo.comment` against the live
    // registry, so a workflow naming custom plugins runs (it would, however, trip the
    // standalone `odin validate`, which only knows the built-in names).
    let summary = engine
        .run(&Workflow::from_yaml_str(WORKFLOW)?, RunInput::manual())
        .await?;

    for step in &summary.steps {
        let stdout = step
            .outputs
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        println!("{} [{:?}] {stdout}", step.id, step.status);
    }
    println!("side-effects: {:?}", summary.side_effects);
    println!("run: {:?}", summary.status);
    if summary.status != RunStatus::Succeeded {
        return Err("custom-plugin run did not succeed".into());
    }
    Ok(())
}
