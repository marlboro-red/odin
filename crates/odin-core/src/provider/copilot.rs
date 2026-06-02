//! The built-in [`Provider`] for the GitHub Copilot CLI (`copilot`).

use async_trait::async_trait;
use indexmap::IndexMap;

use super::process::{ProcessOptions, run_process};
use crate::error::ProviderError;
use crate::ids::ProviderRef;
use crate::traits::{CancelToken, InvocationCtx, InvocationOutcome, Provider};

/// Invokes the standalone GitHub Copilot CLI non-interactively (`copilot -p`).
///
/// Defaults to `--allow-all` (tools/paths/urls), required for non-interactive autonomy;
/// the run executes in an isolated worktree, so that blast radius is contained. The
/// binary name and flags are configurable.
///
/// Unlike the claude/codex adapters (which isolate the agent's final message), copilot's
/// captured `stdout` is best-effort: `--no-color`/`--log-level none` strip ANSI and the
/// log stream, but the CLI may still print session chrome around the result.
pub struct CopilotProvider {
    program: String,
    extra_args: Vec<String>,
}

impl CopilotProvider {
    /// A provider invoking the `copilot` binary on `PATH` with full permissions.
    #[must_use]
    pub fn new() -> Self {
        Self {
            program: "copilot".to_owned(),
            extra_args: vec!["--allow-all".to_owned(), "--no-color".to_owned()],
        }
    }

    /// Overrides the binary name/path.
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Replaces the extra CLI flags applied to every invocation.
    #[must_use]
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
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
        ProviderRef::new("copilot")
    }

    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError> {
        let prompt = ctx.prompt.clone().unwrap_or_default();
        let workdir = ctx.workdir.to_string_lossy().into_owned();

        let mut args = vec![
            "-p".to_owned(),
            prompt,
            "--add-dir".to_owned(),
            workdir,
            "--log-level".to_owned(),
            "none".to_owned(),
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

        Ok(InvocationOutcome {
            exit_code: out.exit_code,
            stdout: out.stdout,
            stderr: out.stderr,
            outputs: IndexMap::new(),
            usage: None,
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

#[cfg(test)]
mod tests {
    use super::CopilotProvider;
    use crate::traits::Provider;

    #[test]
    fn id_is_copilot() {
        assert_eq!(CopilotProvider::new().id().as_str(), "copilot");
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
        eprintln!("copilot replied: {:?}", out.stdout);
    }
}
