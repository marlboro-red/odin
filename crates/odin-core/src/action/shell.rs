//! The `shell.exec` action: run an arbitrary shell command in the workspace.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use crate::error::ActionError;
use crate::traits::{Action, ActionCtx, ActionOutcome};

/// Runs a shell command (`with.command`) via the resolved POSIX shell (`sh -c`) in the run's
/// workspace. See [`crate::provider::posix_shell`].
pub struct ShellExec;

#[async_trait]
impl Action for ShellExec {
    // The trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "shell.exec"
    }

    async fn run(&self, ctx: ActionCtx) -> Result<ActionOutcome, ActionError> {
        let command = super::arg_str(&ctx.args, "command")?;
        let shell = crate::provider::posix_shell()
            .map_err(|e| ActionError::Other(anyhow::anyhow!("{e}")))?;
        let out = super::exec(shell, &["-c", command], &ctx.workdir).await?;
        let mut outputs = IndexMap::new();
        outputs.insert("stdout".to_owned(), Value::String(out.stdout));
        Ok(ActionOutcome {
            exit_code: out.exit_code,
            outputs,
            side_effects: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ShellExec;
    use crate::ids::StepId;
    use crate::traits::{Action, ActionCtx};
    use indexmap::IndexMap;
    use serde_json::json;

    fn ctx(args: IndexMap<String, serde_json::Value>) -> ActionCtx {
        ActionCtx {
            step_id: StepId::new("s"),
            workdir: std::env::temp_dir(),
            args,
        }
    }

    #[tokio::test]
    async fn runs_a_command_and_captures_stdout() {
        let mut args = IndexMap::new();
        args.insert("command".to_owned(), json!("echo hello-action"));
        let out = ShellExec.run(ctx(args)).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(
            out.outputs["stdout"]
                .as_str()
                .unwrap()
                .contains("hello-action")
        );
    }

    #[tokio::test]
    async fn missing_command_arg_errors() {
        assert!(ShellExec.run(ctx(IndexMap::new())).await.is_err());
    }
}
