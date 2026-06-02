//! The built-in [`Provider`] for the GitHub Copilot CLI (`copilot`).

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use super::process::{ProcessOptions, run_process};
use crate::error::ProviderError;
use crate::ids::ProviderRef;
use crate::traits::{CancelToken, InvocationCtx, InvocationOutcome, Provider};
use crate::usage::Usage;

/// Invokes the standalone GitHub Copilot CLI non-interactively (`copilot -p`).
///
/// Defaults to `--allow-all` (tools/paths/urls), required for non-interactive autonomy;
/// the run executes in an isolated worktree, so that blast radius is contained. The
/// binary name and flags are configurable.
///
/// Runs with `--output-format json` (JSONL), so the agent's final answer is read cleanly
/// from `assistant.message` events rather than scraped from chrome-laden text. Copilot
/// reports per-message *output* tokens (not input), so [`Usage`] carries output tokens with
/// `input_tokens`/`cost_micros` left 0 (Copilot bills in "premium requests", not dollars).
///
/// Pin a model with [`with_model`](Self::with_model) (emits `--model <model>`); to mix
/// models in one workflow, register several instances under distinct ids via
/// [`with_id`](Self::with_id) and target each from a step's `provider:`.
pub struct CopilotProvider {
    id: String,
    program: String,
    model: Option<String>,
    extra_args: Vec<String>,
}

impl CopilotProvider {
    /// A provider invoking the `copilot` binary on `PATH` with full permissions, registered
    /// under the id `"copilot"`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: "copilot".to_owned(),
            program: "copilot".to_owned(),
            model: None,
            extra_args: vec!["--allow-all".to_owned(), "--no-color".to_owned()],
        }
    }

    /// Overrides the registry id (the key a step's `provider:` matches). Use a distinct id
    /// per instance to register several model-pinned providers; reusing `"copilot"` replaces
    /// the built-in.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Overrides the binary name/path.
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Pins the model, passed to the CLI as `--model <model>`. Appended after the base and
    /// extra args, so it composes with — and survives — [`with_extra_args`](Self::with_extra_args)
    /// (which replaces the permission defaults).
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Replaces the extra CLI flags applied to every invocation.
    #[must_use]
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    /// Builds the full argument vector for one invocation: the fixed flags + prompt, any
    /// `extra_args`, then `--model` when pinned.
    fn build_args(&self, prompt: String, workdir: String) -> Vec<String> {
        let mut args = vec![
            "-p".to_owned(),
            prompt,
            "--add-dir".to_owned(),
            workdir,
            "--log-level".to_owned(),
            "none".to_owned(),
            // JSONL events: lets us read a clean final answer + token usage.
            "--output-format".to_owned(),
            "json".to_owned(),
        ];
        args.extend(self.extra_args.iter().cloned());
        if let Some(model) = &self.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        args
    }

    /// Formats this provider's version string, folding in the pinned model when set. Surfaced
    /// via `Provider::version`; once provider-version capture is wired into run state, it makes
    /// two runs that differ only by model distinguishable for reproducibility.
    fn version_string(&self, cli_version: &str) -> String {
        match &self.model {
            Some(model) => format!("{cli_version} (model={model})"),
            None => cli_version.to_owned(),
        }
    }
}

impl Default for CopilotProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn id(&self) -> ProviderRef {
        ProviderRef::new(self.id.as_str())
    }

    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError> {
        let prompt = ctx.prompt.clone().unwrap_or_default();
        let workdir = ctx.workdir.to_string_lossy().into_owned();

        let args = self.build_args(prompt, workdir);

        let opts = ProcessOptions {
            workdir: Some(ctx.workdir.clone()),
            timeout: ctx.timeout,
            env: Vec::new(),
            stdin: None,
        };
        let out = run_process(&self.program, &args, &opts, &ctx.cancel).await?;
        if out.timed_out {
            return Err(ProviderError::Timeout(ctx.timeout.unwrap_or_default()));
        }

        let (event_text, usage) = parse_events(&out.stdout);
        Ok(InvocationOutcome {
            exit_code: out.exit_code,
            stdout: event_text.unwrap_or_else(|| out.stdout.clone()),
            stderr: out.stderr,
            outputs: IndexMap::new(),
            usage,
            produced: IndexMap::new(),
        })
    }

    async fn version(&self) -> Option<String> {
        let out = run_process(
            &self.program,
            &["--version".to_owned()],
            &ProcessOptions::default(),
            &CancelToken::new(),
        )
        .await
        .ok()?;
        let v = out.stdout.trim();
        (!v.is_empty()).then(|| self.version_string(v))
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        let out = run_process(
            &self.program,
            &["--version".to_owned()],
            &ProcessOptions::default(),
            &CancelToken::new(),
        )
        .await?;
        if out.exit_code == 0 {
            Ok(())
        } else {
            Err(ProviderError::Exited {
                code: out.exit_code,
                stderr: out.stderr,
            })
        }
    }
}

/// Parses `copilot --output-format json` JSONL into the final answer and aggregate token
/// usage. The answer is the last `assistant.message` content; usage sums per-message
/// `outputTokens` (Copilot reports no input tokens or dollar cost, so those stay 0). Tolerant
/// of schema drift — unparseable lines are skipped, and usage is `None` if none was seen.
fn parse_events(stdout: &str) -> (Option<String>, Option<Usage>) {
    let mut text: Option<String> = None;
    let mut output_tokens = 0_u64;
    let mut saw_tokens = false;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("assistant.message") {
            if let Some(content) = v.pointer("/data/content").and_then(Value::as_str) {
                if !content.is_empty() {
                    text = Some(content.to_owned());
                }
            }
            if let Some(tokens) = v.pointer("/data/outputTokens").and_then(Value::as_u64) {
                output_tokens += tokens;
                saw_tokens = true;
            }
        }
    }
    let usage = saw_tokens.then_some(Usage {
        input_tokens: 0,
        output_tokens,
        cost_micros: 0,
    });
    (text, usage)
}

#[cfg(test)]
mod tests {
    use super::{CopilotProvider, parse_events};
    use crate::traits::Provider;

    #[test]
    fn id_is_copilot() {
        assert_eq!(CopilotProvider::new().id().as_str(), "copilot");
    }

    #[test]
    fn with_id_overrides_the_registry_key() {
        assert_eq!(
            CopilotProvider::new().with_id("reviewer").id().as_str(),
            "reviewer"
        );
    }

    #[test]
    fn version_string_folds_in_the_pinned_model() {
        assert_eq!(CopilotProvider::new().version_string("1.0.57"), "1.0.57");
        assert_eq!(
            CopilotProvider::new()
                .with_model("gpt-5.2")
                .version_string("1.0.57"),
            "1.0.57 (model=gpt-5.2)"
        );
    }

    #[test]
    fn with_model_appends_a_model_flag() {
        let args = CopilotProvider::new()
            .with_model("gpt-5.2")
            .build_args("hi".to_owned(), "/wd".to_owned());
        let pos = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[pos + 1], "gpt-5.2");
        // The permission defaults still survive alongside the model.
        assert!(args.iter().any(|a| a == "--allow-all"));
    }

    #[test]
    fn parse_events_extracts_final_answer_and_output_tokens() {
        // Mirrors real `copilot --output-format json`: session chrome, then the answer.
        let jsonl = concat!(
            r#"{"type":"session.mcp_servers_loaded","data":{"servers":[]}}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"ok","outputTokens":37,"phase":"final_answer"}}"#,
            "\n",
            r#"{"type":"result","data":{},"usage":{"premiumRequests":7.5}}"#,
            "\n",
        );
        let (text, usage) = parse_events(jsonl);
        assert_eq!(text.as_deref(), Some("ok"));
        let usage = usage.expect("usage present");
        assert_eq!(usage.output_tokens, 37);
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.cost_micros, 0);
    }

    #[test]
    fn parse_events_tolerates_chrome_only_output() {
        let (text, usage) = parse_events("plain text\n{\"type\":\"session.skills_loaded\"}\n");
        assert!(text.is_none());
        assert!(usage.is_none());
    }

    /// Live smoke test against the real `copilot` CLI. Double-gated (`#[ignore]` +
    /// `ODIN_LIVE_PROVIDER_TESTS=1`) so default/CI runs never incur cost.
    /// `ODIN_LIVE_PROVIDER_TESTS=1 cargo test -p odin-core live_copilot_smoke -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "live: invokes the real `copilot` CLI; set ODIN_LIVE_PROVIDER_TESTS=1"]
    async fn live_copilot_smoke() {
        use crate::ids::StepId;
        use crate::traits::{CancelToken, InvocationCtx};
        use indexmap::IndexMap;

        if std::env::var("ODIN_LIVE_PROVIDER_TESTS").ok().as_deref() != Some("1") {
            eprintln!("skipping live copilot test; set ODIN_LIVE_PROVIDER_TESTS=1 to run");
            return;
        }
        let provider = CopilotProvider::new();
        provider
            .health_check()
            .await
            .expect("copilot should be installed and on PATH");

        let dir = tempfile::tempdir().unwrap();
        let ctx = InvocationCtx {
            step_id: StepId::new("smoke"),
            workdir: dir.path().to_path_buf(),
            prompt: Some("Reply with exactly the word: ok".to_owned()),
            inputs: IndexMap::new(),
            timeout: Some(std::time::Duration::from_secs(180)),
            cancel: CancelToken::new(),
        };
        let out = provider
            .invoke(ctx)
            .await
            .expect("invocation should succeed");
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert!(!out.stdout.trim().is_empty(), "expected a non-empty reply");
        let usage = out
            .usage
            .expect("copilot --output-format json should report usage");
        assert!(usage.output_tokens > 0, "expected non-zero output tokens");
        eprintln!("copilot replied: {:?}; usage: {usage:?}", out.stdout);
    }
}
