//! Minimal **codex-only** smoke through Odin's engine — no Claude/Opus calls, cheap to run.
//!
//! Validates the codex provider end-to-end: `with_id`/`with_model`/read-only registration,
//! plus the engine's absolute-workdir handling (a relative workdir made `codex --cd`/`-o`
//! resolve to a nonexistent doubled path and fail with "No such file or directory"). Use this
//! to iterate on the codex integration without burning tokens on the full cross-model eval.
//!
//! Invokes the real `codex` CLI (must be installed, on `PATH`, authenticated); small API cost.
//!
//! ```text
//! cargo run -p odin-core --example codex_smoke
//! ```
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use odin_core::{CodexProvider, EngineBuilder, RunInput, RunStatus, Workflow};

const WORKFLOW: &str = r#"
name: codex-smoke
durable: false
workspace: { type: worktree }
steps:
  - id: ask
    provider: codex-gpt55
    prompt: "Reply with exactly the word: ok"
"#;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `.repo(".")` is deliberately relative — this exercises the engine's absolutization.
    let mut builder = EngineBuilder::new().repo(".");
    builder.registry_mut().register_provider(Arc::new(
        CodexProvider::new()
            .with_id("codex-gpt55")
            .with_model("gpt-5.5")
            .with_extra_args(vec![
                "--sandbox".to_owned(),
                "read-only".to_owned(),
                "--skip-git-repo-check".to_owned(),
            ]),
    ));
    let engine = builder.build()?;

    let summary = engine
        .run(&Workflow::from_yaml_str(WORKFLOW)?, RunInput::manual())
        .await?;

    for step in &summary.steps {
        println!(
            "{} [{:?}] exit={:?} stdout={:?}",
            step.id,
            step.status,
            step.exit_code,
            step.outputs
                .get("stdout")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        );
    }
    println!("run {:?}; ${:.4}", summary.status, summary.usage.cost_usd());
    if summary.status != RunStatus::Succeeded {
        return Err("codex smoke did not succeed".into());
    }
    Ok(())
}
