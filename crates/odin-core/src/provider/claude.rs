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
/// The binary name and any extra flags (model, permission mode, allowed tools) are
/// configurable so the executor can tune autonomy per workflow without changing this
/// adapter.
pub struct ClaudeProvider {
    program: String,
    extra_args: Vec<String>,
}

impl ClaudeProvider {
    /// A provider invoking the `claude` binary on `PATH` with no extra flags.
    #[must_use]
    pub fn new() -> Self {
        Self {
            program: "claude".to_owned(),
            extra_args: Vec::new(),
        }
    }

    /// Overrides the binary name/path (useful for tests or a pinned install).
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Appends extra CLI flags to every invocation (e.g. `--model`, a permission mode).
    #[must_use]
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
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
        ProviderRef::new("claude")
    }

    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError> {
        let prompt = ctx.prompt.clone().unwrap_or_default();
        let mut args = vec![
            "-p".to_owned(),
            prompt,
            "--output-format".to_owned(),
            "json".to_owned(),
        ];
        args.extend(self.extra_args.iter().cloned());

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

        let (text, usage) = parse_result(&out.stdout);
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

/// Parses `claude --output-format json` output into `(result_text, usage)`.
///
/// Tolerant of schema drift: any parse failure or missing field falls back to the raw
/// stdout as the result text and no usage.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn parse_result(stdout: &str) -> (String, Option<Usage>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return (stdout.to_owned(), None);
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
    (
        text,
        Some(Usage {
            input_tokens,
            output_tokens,
            cost_micros,
        }),
    )
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
    fn parses_json_result_and_usage() {
        let json = r#"{"type":"result","result":"all done","total_cost_usd":0.0123,"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let (text, usage) = parse_result(json);
        assert_eq!(text, "all done");
        let u = usage.unwrap();
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cost_micros, 12_300);
    }

    #[test]
    fn falls_back_to_raw_stdout_on_non_json() {
        let (text, usage) = parse_result("just plain text");
        assert_eq!(text, "just plain text");
        assert!(usage.is_none());
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
