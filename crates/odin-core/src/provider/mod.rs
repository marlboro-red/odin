//! Built-in [`crate::traits::Provider`] adapters and the subprocess runner they share.
//!
//! v1 ships [`ClaudeProvider`], [`CodexProvider`], and [`CopilotProvider`] — each builds
//! args → [`process::run_process`] → maps to an [`crate::traits::InvocationOutcome`].

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod process;

pub use claude::ClaudeProvider;
pub use codex::CodexProvider;
pub use copilot::CopilotProvider;
pub use process::{ProcessOptions, ProcessOutput, StreamMux, StreamSink, posix_shell, run_process};
