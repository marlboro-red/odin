//! Cross-model adversarial PR review — dogfooding Odin's own engine.
//!
//! Registers two model-pinned providers via the `with_id`/`with_model` builders this PR adds —
//! `claude-opus` (Claude Code @ `claude-opus-4-8`) and `codex-gpt55` (Codex @ `gpt-5.5`) — then
//! runs [`cross_model_review.yaml`](./cross_model_review.yaml): each model reviews the PR diff,
//! each adversarially scrutinizes the OTHER model's review, and a final step synthesizes the
//! confirmed findings.
//!
//! This invokes the real `claude` and `codex` CLIs (must be installed, on `PATH`, and
//! authenticated) and incurs API cost, so it is a runnable example — not a test.
//!
//! ```text
//! cargo run -p odin-core --example cross_model_review -- [BASE_REF] [PATHSPEC...]
//! ```
//!
//! `BASE_REF` defaults to `main`; the reviewed diff is `git diff <BASE_REF> [-- PATHSPEC...]`.
//! The diff is capped (~14 KB) because `codex exec` stalls on very large single prompts —
//! scope with pathspecs (e.g. `main crates/odin-core/src/provider/claude.rs`) to review a
//! specific file in full.
#![allow(clippy::doc_markdown)]

use std::process::Command;
use std::sync::Arc;

use odin_core::{ClaudeProvider, CodexProvider, EngineBuilder, RunInput, RunStatus, Workflow};

const REVIEW_WORKFLOW: &str = include_str!("cross_model_review.yaml");

/// Cap on the diff handed to the reviewers: `codex exec` stalls on very large single prompts
/// (~30 KB+), while `claude -p` handles them. Kept well under that so even a refute step's
/// `diff + the other model's review` stays responsive.
const MAX_DIFF_BYTES: usize = 14_000;

type BoxError = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    // args: [BASE_REF] [PATHSPEC...] — base defaults to `main`; pathspecs scope the diff.
    let mut argv = std::env::args().skip(1);
    let base = argv.next().unwrap_or_else(|| "main".to_owned());
    let paths: Vec<String> = argv.collect();

    // The PR diff to review (committed + working-tree changes vs the base ref).
    let mut git = vec!["diff".to_owned(), base.clone()];
    if !paths.is_empty() {
        git.push("--".to_owned());
        git.extend(paths.iter().cloned());
    }
    let out = Command::new("git").args(&git).output()?;
    if !out.status.success() {
        return Err(format!(
            "`git diff {base}` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let mut diff = String::from_utf8(out.stdout)?;
    if diff.trim().is_empty() {
        return Err(format!("empty diff vs {base} — nothing to review").into());
    }

    // Bound the prompt (see MAX_DIFF_BYTES) — and say so loudly rather than silently
    // truncating. Scope with pathspecs to review specific files in full.
    let full_len = diff.len();
    if full_len > MAX_DIFF_BYTES {
        let mut cut = MAX_DIFF_BYTES;
        while cut > 0 && !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        diff.push_str("\n... [diff truncated for reviewer compatibility]\n");
        eprintln!(
            "WARNING: diff vs {base} is {full_len} bytes; truncated to ~{MAX_DIFF_BYTES} so the \
             codex reviewer stays responsive. Pass pathspecs to scope the review instead."
        );
    }
    eprintln!("reviewing {} bytes of diff vs {base} …", diff.len());

    // Two model-pinned providers, each registered under a distinct id — the with_id/with_model
    // feature in action. A step's `provider:` targets these names.
    let mut builder = EngineBuilder::new().repo(".");
    builder
        .registry_mut()
        .register_provider(Arc::new(
            ClaudeProvider::new()
                .with_id("claude-opus")
                .with_model("claude-opus-4-8"),
        ))
        .register_provider(Arc::new(
            CodexProvider::new()
                .with_id("codex-gpt55")
                .with_model("gpt-5.5")
                // A reviewer must not edit the tree. Pin a read-only sandbox: `with_extra_args`
                // REPLACES codex's default `workspace-write` flags (so re-supply
                // `--skip-git-repo-check`), while the `--model` pin from `with_model` is a
                // separate field that survives. Read-only also avoids codex's agentic edit
                // loop, keeping each review a fast single-shot answer.
                .with_extra_args(vec![
                    "--sandbox".to_owned(),
                    "read-only".to_owned(),
                    "--skip-git-repo-check".to_owned(),
                ]),
        ));
    let engine = builder.build()?;

    let workflow = Workflow::from_yaml_str(REVIEW_WORKFLOW)?;
    let summary = engine
        .run(&workflow, RunInput::manual().param("diff", diff))
        .await?;

    for step in &summary.steps {
        let text = step
            .outputs
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("<no text output>");
        println!(
            "\n========== {} [{:?}] ==========\n{text}",
            step.id, step.status
        );
    }
    println!(
        "\n=== run {} — {:?}; {} step(s); ${:.4} ===",
        summary.run_id,
        summary.status,
        summary.steps.len(),
        summary.usage.cost_usd()
    );
    if let Some(err) = &summary.error {
        eprintln!("error: {err}");
    }
    if summary.status != RunStatus::Succeeded {
        return Err("cross-model review run did not succeed".into());
    }
    Ok(())
}
