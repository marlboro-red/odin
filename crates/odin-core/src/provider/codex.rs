//! The built-in [`Provider`] for OpenAI's Codex CLI (`codex`).

use async_trait::async_trait;
use indexmap::IndexMap;

use super::process::{ProcessOptions, run_process};
use crate::error::ProviderError;
use crate::ids::ProviderRef;
use crate::traits::{CancelToken, InvocationCtx, InvocationOutcome, Provider};

/// Invokes the `codex` CLI non-interactively (`codex exec`).
///
/// The agent's final message is captured via `-o <file>` (clean text, no event parsing).
/// Defaults to the `workspace-write` sandbox so the agent can edit the run's worktree;
/// the binary name and flags are configurable.
pub struct CodexProvider {
    program: String,
    extra_args: Vec<String>,
}

impl CodexProvider {
    /// A provider invoking the `codex` binary on `PATH` with the default sandbox flags.
    #[must_use]
    pub fn new() -> Self {
        Self {
            program: "codex".to_owned(),
            extra_args: vec![
                "--sandbox".to_owned(),
                "workspace-write".to_owned(),
                "--skip-git-repo-check".to_owned(),
            ],
        }
    }

    /// Overrides the binary name/path.
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Replaces the extra CLI flags applied to every `codex exec` invocation.
    #[must_use]
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
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
        ProviderRef::new("codex")
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

        let mut args = vec![
            "exec".to_owned(),
            "--cd".to_owned(),
            workdir,
            "-o".to_owned(),
            last_message_arg,
        ];
        args.extend(self.extra_args.iter().cloned());
        args.push(prompt);

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

        let text = std::fs::read_to_string(&last_message).unwrap_or_else(|_| out.stdout.clone());
        let _ = std::fs::remove_file(&last_message);

        Ok(InvocationOutcome {
            exit_code: out.exit_code,
            stdout: text,
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
    use super::CodexProvider;
    use crate::traits::Provider;

    #[test]
    fn id_is_codex() {
        assert_eq!(CodexProvider::new().id().as_str(), "codex");
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
        eprintln!("codex replied: {:?}", out.stdout);
    }
}
