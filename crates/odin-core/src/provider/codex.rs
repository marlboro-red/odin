//! The built-in [`Provider`] for OpenAI's Codex CLI (`codex`).

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use super::process::{ProcessOptions, run_process};
use crate::error::ProviderError;
use crate::ids::ProviderRef;
use crate::traits::{CancelToken, InvocationCtx, InvocationOutcome, Provider};
use crate::usage::Usage;

/// Invokes the `codex` CLI non-interactively (`codex exec`).
///
/// The agent's final message is captured via `-o <file>` (clean text, no event parsing).
/// Defaults to the `workspace-write` sandbox so the agent can edit the run's worktree;
/// the binary name, registry id, pinned model, and sandbox flags are configurable.
///
/// Pin a model with [`with_model`](Self::with_model) (emits `--model <model>`); to mix
/// models in one workflow, register several instances under distinct ids via
/// [`with_id`](Self::with_id) and target each from a step's `provider:`.
pub struct CodexProvider {
    id: String,
    program: String,
    model: Option<String>,
    extra_args: Vec<String>,
}

impl CodexProvider {
    /// A provider invoking the `codex` binary on `PATH` with the default sandbox flags,
    /// registered under the id `"codex"`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: "codex".to_owned(),
            program: "codex".to_owned(),
            model: None,
            extra_args: vec![
                "--sandbox".to_owned(),
                "workspace-write".to_owned(),
                "--skip-git-repo-check".to_owned(),
            ],
        }
    }

    /// Overrides the registry id (the key a step's `provider:` matches). Use a distinct id
    /// per instance to register several model-pinned providers; reusing `"codex"` replaces
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
    /// (which replaces the sandbox defaults).
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Replaces the extra CLI flags applied to every `codex exec` invocation.
    #[must_use]
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    /// Builds the full argument vector for one `codex exec`: the fixed flags, `extra_args`,
    /// `--model` when pinned, then the prompt as the trailing positional argument.
    fn build_args(&self, prompt: String, workdir: String, last_message_arg: String) -> Vec<String> {
        let mut args = vec![
            "exec".to_owned(),
            // `--json` streams JSONL events to stdout (we parse token usage from them);
            // `-o` still captures the clean final message to a file.
            "--json".to_owned(),
            "--cd".to_owned(),
            workdir,
            "-o".to_owned(),
            last_message_arg,
        ];
        args.extend(self.extra_args.iter().cloned());
        if let Some(model) = &self.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        // The prompt is the trailing positional arg, so it must be pushed last.
        args.push(prompt);
        args
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn id(&self) -> ProviderRef {
        ProviderRef::new(self.id.as_str())
    }

    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError> {
        let prompt = ctx.prompt.clone().unwrap_or_default();
        let workdir = ctx.workdir.to_string_lossy().into_owned();
        // The final agent message is written here (clean text); kept inside the workdir so
        // a write-sandboxed agent can create it, and removed before the engine captures DIFF.
        let last_message = ctx
            .workdir
            .join(format!(".odin-codex-{}.txt", uuid::Uuid::new_v4()));
        let last_message_arg = last_message.to_string_lossy().into_owned();

        let args = self.build_args(prompt, workdir, last_message_arg);

        let opts = ProcessOptions {
            workdir: Some(ctx.workdir.clone()),
            timeout: ctx.timeout,
            env: Vec::new(),
            stdin: None,
        };
        let out = run_process(&self.program, &args, &opts, &ctx.cancel).await?;

        // Parse the JSONL event stream for the agent message (fallback) and token usage.
        let (event_text, usage) = parse_events(&out.stdout);
        // Read the captured final message and remove the temp file on EVERY path (incl.
        // timeout), so it never leaks into the worktree or the auto-captured DIFF.
        let text = std::fs::read_to_string(&last_message)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or(event_text)
            .unwrap_or_else(|| out.stdout.clone());
        let _ = std::fs::remove_file(&last_message);

        if out.timed_out {
            return Err(ProviderError::Timeout(ctx.timeout.unwrap_or_default()));
        }

        Ok(InvocationOutcome {
            exit_code: out.exit_code,
            stdout: text,
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
        (!v.is_empty()).then(|| v.to_owned())
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

/// Parses `codex exec --json` JSONL events into the agent's final message (fallback for the
/// `-o` file) and aggregate token usage. Tolerant of unknown lines/schema drift: unparseable
/// lines are skipped, and usage is `None` if no `turn.completed` event carried it. Codex does
/// not report a dollar cost, so `cost_micros` stays 0.
fn parse_events(stdout: &str) -> (Option<String>, Option<Usage>) {
    let mut text: Option<String> = None;
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut saw_usage = false;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("item.completed")
                if v.pointer("/item/type").and_then(Value::as_str) == Some("agent_message") =>
            {
                if let Some(t) = v.pointer("/item/text").and_then(Value::as_str) {
                    text = Some(t.to_owned());
                }
            }
            Some("turn.completed") => {
                if let Some(u) = v.get("usage") {
                    input_tokens += u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    output_tokens += u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                    saw_usage = true;
                }
            }
            _ => {}
        }
    }
    let usage = saw_usage.then_some(Usage {
        input_tokens,
        output_tokens,
        cost_micros: 0,
    });
    (text, usage)
}

#[cfg(test)]
mod tests {
    use super::{CodexProvider, parse_events};
    use crate::traits::Provider;

    #[test]
    fn id_is_codex() {
        assert_eq!(CodexProvider::new().id().as_str(), "codex");
    }

    #[test]
    fn with_id_overrides_the_registry_key() {
        assert_eq!(
            CodexProvider::new().with_id("impl-codex").id().as_str(),
            "impl-codex"
        );
    }

    #[test]
    fn with_model_inserts_model_before_the_trailing_prompt() {
        let args = CodexProvider::new().with_model("gpt-5.2-codex").build_args(
            "do it".to_owned(),
            "/wd".to_owned(),
            "/wd/.odin-codex-x.txt".to_owned(),
        );
        // The prompt stays the final positional argument...
        assert_eq!(args.last().map(String::as_str), Some("do it"));
        // ...and `--model <name>` precedes it (codex would treat a trailing flag as part of
        // the positional prompt otherwise).
        let pos = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[pos + 1], "gpt-5.2-codex");
        assert!(
            pos + 1 < args.len() - 1,
            "model must come before the prompt"
        );
        // The sandbox defaults still survive, and `--model` is appended *after* them (the
        // documented compose contract: a later `with_model` pin wins over extra args).
        let sandbox = args
            .iter()
            .position(|a| a == "--skip-git-repo-check")
            .unwrap();
        assert!(pos > sandbox, "--model must follow extra_args: {args:?}");
    }

    #[test]
    fn parse_events_extracts_message_and_summed_usage() {
        // Two turns + an agent message, mirroring real `codex exec --json` output.
        let jsonl = concat!(
            r#"{"type":"thread.started","thread_id":"x"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"ok"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":5}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}"#,
            "\n",
        );
        let (text, usage) = parse_events(jsonl);
        assert_eq!(text.as_deref(), Some("ok"));
        let usage = usage.expect("usage present");
        assert_eq!(usage.input_tokens, 110);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cost_micros, 0);
    }

    #[test]
    fn parse_events_tolerates_no_usage_and_junk() {
        let (text, usage) = parse_events("not json\n{\"type\":\"turn.started\"}\n");
        assert!(text.is_none());
        assert!(usage.is_none());
    }

    /// Live smoke test against the real `codex` CLI. Double-gated (`#[ignore]` +
    /// `ODIN_LIVE_PROVIDER_TESTS=1`) so default/CI runs never incur cost.
    /// `ODIN_LIVE_PROVIDER_TESTS=1 cargo test -p odin-core live_codex_smoke -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "live: invokes the real `codex` CLI; set ODIN_LIVE_PROVIDER_TESTS=1"]
    async fn live_codex_smoke() {
        use crate::ids::StepId;
        use crate::traits::{CancelToken, InvocationCtx};
        use indexmap::IndexMap;

        if std::env::var("ODIN_LIVE_PROVIDER_TESTS").ok().as_deref() != Some("1") {
            eprintln!("skipping live codex test; set ODIN_LIVE_PROVIDER_TESTS=1 to run");
            return;
        }
        let provider = CodexProvider::new();
        provider
            .health_check()
            .await
            .expect("codex should be installed and on PATH");

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
        let usage = out.usage.expect("codex --json should report token usage");
        assert!(usage.input_tokens > 0, "expected non-zero input tokens");
        eprintln!("codex replied: {:?}; usage: {usage:?}", out.stdout);
    }
}
