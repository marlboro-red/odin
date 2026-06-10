//! The built-in [`Provider`] for Anthropic's Claude Code CLI (`claude`).

use async_trait::async_trait;
use indexmap::IndexMap;

use super::process::{ProcessOptions, run_process};
use crate::error::ProviderError;
use crate::ids::ProviderRef;
use crate::traits::{CancelToken, InvocationCtx, InvocationOutcome, Provider};
use crate::usage::Usage;

/// Invokes the `claude` CLI in headless mode (`claude -p <prompt> --output-format json`).
///
/// The binary name, the registry id, a pinned model, and any extra flags (permission mode,
/// allowed tools) are configurable so the executor can tune autonomy per workflow without
/// changing this adapter.
///
/// To run several models in one workflow, register one instance per model under a distinct
/// id and target each from a step's `provider:`:
/// ```no_run
/// # use std::sync::Arc;
/// # use odin_core::{EngineBuilder, ClaudeProvider};
/// # fn wire(builder: &mut EngineBuilder) {
/// builder
///     .registry_mut()
///     .register_provider(Arc::new(
///         ClaudeProvider::new().with_id("planner").with_model("claude-opus-4-8"),
///     ))
///     .register_provider(Arc::new(
///         ClaudeProvider::new().with_id("reviewer").with_model("claude-sonnet-4-6"),
///     ));
/// # }
/// ```
pub struct ClaudeProvider {
    id: String,
    program: String,
    model: Option<String>,
    extra_args: Vec<String>,
}

impl ClaudeProvider {
    /// A provider invoking the `claude` binary on `PATH` with no extra flags, registered
    /// under the id `"claude"`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: "claude".to_owned(),
            program: "claude".to_owned(),
            model: None,
            extra_args: Vec::new(),
        }
    }

    /// Overrides the registry id (the key a step's `provider:` matches). Use a distinct id
    /// per instance to register several model-pinned providers; reusing `"claude"` replaces
    /// the built-in.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Overrides the binary name/path (useful for tests or a pinned install).
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Pins the model, passed to the CLI as `--model <model>` on every invocation
    /// (appended after [`with_extra_args`](Self::with_extra_args), so the two compose).
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Appends extra CLI flags to every invocation (e.g. a permission mode, allowed tools).
    #[must_use]
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    /// Builds the full argument vector for one invocation: the fixed headless flags, the
    /// prompt, any `extra_args`, then `--model` when pinned.
    fn build_args(&self, prompt: String) -> Vec<String> {
        let mut args = vec![
            "-p".to_owned(),
            prompt,
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
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for ClaudeProvider {
    fn id(&self) -> ProviderRef {
        ProviderRef::new(self.id.as_str())
    }

    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError> {
        let prompt = ctx.prompt.clone().unwrap_or_default();
        let args = self.build_args(prompt);

        let opts = ProcessOptions {
            workdir: Some(ctx.workdir.clone()),
            timeout: ctx.timeout,
            env: Vec::new(),
            stdin: None,
            stream: ctx.stream.clone(),
        };
        let out = run_process(&self.program, &args, &opts, &ctx.cancel).await?;
        if out.timed_out {
            return Err(ProviderError::Timeout(ctx.timeout.unwrap_or_default()));
        }

        let parsed = parse_result(&out.stdout);
        let mut exit_code = out.exit_code;
        let mut stderr = out.stderr;
        // Claude can exit 0 while reporting `is_error: true` (max-turns, execution error). Normalize
        // that to a non-zero exit so the engine fails the step instead of recording the error text
        // as a successful result that flows downstream.
        if let Some(reason) = parsed.error {
            if exit_code == 0 {
                exit_code = 1;
            }
            if !stderr.is_empty() {
                stderr.push('\n');
            }
            stderr.push_str(&reason);
        }
        Ok(InvocationOutcome {
            exit_code,
            stdout: parsed.text,
            stderr,
            outputs: IndexMap::new(),
            usage: parsed.usage,
            produced: IndexMap::new(),
        })
    }

    async fn version(&self) -> Option<String> {
        // Bounded so a hung/auth-prompting CLI can't wedge the run that resolves it (the result is
        // cached, so this runs at most once per provider). A non-zero exit isn't a usable version.
        let opts = ProcessOptions {
            timeout: Some(std::time::Duration::from_secs(5)),
            ..ProcessOptions::default()
        };
        let out = run_process(
            &self.program,
            &["--version".to_owned()],
            &opts,
            &CancelToken::new(),
        )
        .await
        .ok()?;
        if out.exit_code != 0 {
            return None;
        }
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

/// What Odin reads from a `claude --output-format json` document.
struct ParsedResult {
    text: String,
    usage: Option<Usage>,
    /// `Some(reason)` when claude reported `is_error: true` — even at process exit 0 (e.g.
    /// `error_max_turns`, `error_during_execution`). The caller normalizes this to a non-zero exit
    /// so the engine records a **failure**, not a success with a refusal/error as its output.
    error: Option<String>,
}

/// Parses `claude --output-format json` output into [`ParsedResult`]. Tolerant of schema drift:
/// any parse failure or missing field falls back to the raw stdout as the result text, no usage,
/// and no error.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn parse_result(stdout: &str) -> ParsedResult {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return ParsedResult {
            text: stdout.to_owned(),
            usage: None,
            error: None,
        };
    };
    let text = v
        .get("result")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| stdout.to_owned(), str::to_owned);
    let input_tokens = v
        .pointer("/usage/input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output_tokens = v
        .pointer("/usage/output_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    // total_cost_usd is a float in the CLI output; convert to integer micro-dollars.
    let cost_micros = v
        .get("total_cost_usd")
        .and_then(serde_json::Value::as_f64)
        .map_or(0, |d| (d.max(0.0) * 1_000_000.0).round() as u64);
    let error = v
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        .then(|| {
            let subtype = v
                .get("subtype")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("error");
            format!("claude reported an error (is_error, subtype={subtype})")
        });
    ParsedResult {
        text,
        usage: Some(Usage {
            input_tokens,
            output_tokens,
            cost_micros,
        }),
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaudeProvider, parse_result};
    use crate::traits::Provider;

    #[test]
    fn id_is_claude() {
        assert_eq!(ClaudeProvider::new().id().as_str(), "claude");
    }

    #[test]
    fn with_id_overrides_the_registry_key() {
        assert_eq!(
            ClaudeProvider::new().with_id("planner").id().as_str(),
            "planner"
        );
    }

    #[test]
    fn with_model_appends_a_model_flag() {
        let args = ClaudeProvider::new()
            .with_model("claude-opus-4-8")
            .build_args("hi".to_owned());
        let pos = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[pos + 1], "claude-opus-4-8");
        // No model flag is emitted when unset.
        assert!(
            !ClaudeProvider::new()
                .build_args("hi".to_owned())
                .iter()
                .any(|a| a == "--model")
        );
    }

    #[test]
    fn pinned_model_is_appended_after_extra_args() {
        // The documented compose contract: `--model` lands AFTER `with_extra_args`, so a
        // later `with_model` pin wins (last-position) over anything in the extra args.
        let args = ClaudeProvider::new()
            .with_extra_args(vec!["--permission-mode".to_owned(), "plan".to_owned()])
            .with_model("claude-opus-4-8")
            .build_args("hi".to_owned());
        let extra = args.iter().position(|a| a == "--permission-mode").unwrap();
        let model = args.iter().position(|a| a == "--model").unwrap();
        assert!(model > extra, "--model must follow extra_args: {args:?}");
    }

    #[test]
    fn parses_json_result_and_usage() {
        let json = r#"{"type":"result","result":"all done","total_cost_usd":0.0123,"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let p = parse_result(json);
        assert_eq!(p.text, "all done");
        let u = p.usage.unwrap();
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cost_micros, 12_300);
        assert!(p.error.is_none(), "a successful result is not an error");
    }

    #[test]
    fn is_error_result_is_surfaced_as_an_error() {
        // claude exits 0 but reports an error (e.g. hit the turn limit) — Odin must not treat the
        // error text as a successful result.
        let json = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"hit the limit","usage":{"input_tokens":10,"output_tokens":0}}"#;
        let p = parse_result(json);
        let reason = p.error.expect("is_error:true must surface an error reason");
        assert!(
            reason.contains("error_max_turns"),
            "reason names the subtype: {reason}"
        );
        // usage is still captured (the call cost tokens even though it errored).
        assert_eq!(p.usage.unwrap().input_tokens, 10);
    }

    #[test]
    fn falls_back_to_raw_stdout_on_non_json() {
        let p = parse_result("just plain text");
        assert_eq!(p.text, "just plain text");
        assert!(p.usage.is_none());
        assert!(p.error.is_none());
    }

    /// Live smoke test against the real `claude` CLI. Double-gated so it never runs
    /// (or costs anything) by default:
    /// 1. `#[ignore]` — excluded unless you pass `-- --ignored`.
    /// 2. requires `ODIN_LIVE_PROVIDER_TESTS=1` in the environment.
    ///
    /// Run it with:
    /// `ODIN_LIVE_PROVIDER_TESTS=1 cargo test -p odin-core live_claude_smoke -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "live: invokes the real `claude` CLI; set ODIN_LIVE_PROVIDER_TESTS=1"]
    async fn live_claude_smoke() {
        use crate::ids::StepId;
        use crate::traits::{CancelToken, InvocationCtx};
        use indexmap::IndexMap;

        if std::env::var("ODIN_LIVE_PROVIDER_TESTS").ok().as_deref() != Some("1") {
            eprintln!("skipping live claude test; set ODIN_LIVE_PROVIDER_TESTS=1 to run");
            return;
        }

        let provider = ClaudeProvider::new();
        provider
            .health_check()
            .await
            .expect("claude should be installed and on PATH");

        let ctx = InvocationCtx {
            step_id: StepId::new("smoke"),
            workdir: std::env::temp_dir(),
            prompt: Some("Reply with exactly the word: ok".to_owned()),
            inputs: IndexMap::new(),
            timeout: Some(std::time::Duration::from_secs(120)),
            cancel: CancelToken::new(),
            stream: None,
        };
        let out = provider
            .invoke(ctx)
            .await
            .expect("invocation should succeed");
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert!(!out.stdout.is_empty(), "expected a non-empty reply");
        eprintln!("claude replied: {:?}; usage: {:?}", out.stdout, out.usage);
    }
}
