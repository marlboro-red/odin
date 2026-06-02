//! Built-in [`crate::traits::Provider`] adapters and the subprocess runner they share.
//!
//! v1 ships [`ClaudeProvider`]; the OpenAI Codex and GitHub Copilot CLI adapters follow
//! the same shape (build args → [`process::run_process`] → map to an
//! [`crate::traits::InvocationOutcome`]) and land in later milestones.

pub mod claude;
pub mod process;

pub use claude::ClaudeProvider;
pub use process::{ProcessOptions, ProcessOutput, run_process};
