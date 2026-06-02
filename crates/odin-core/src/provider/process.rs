//! Async subprocess management for provider CLIs.
//!
//! [`run_process`] spawns a command, streams and captures its stdout/stderr
//! concurrently (so a chatty agent never deadlocks on a full pipe), and races the
//! child against an optional timeout and a [`CancelToken`]. On timeout or cancel the
//! child is killed and reaped; partial output captured so far is still returned.
//!
//! Termination currently uses `start_kill` (SIGKILL on Unix). A graceful
//! SIGINT-then-SIGKILL escalation is a deliberate later refinement — it needs a signal
//! crate, and the crate is `unsafe`-forbidden, so it cannot use raw `libc::kill`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use crate::error::ProviderError;
use crate::traits::CancelToken;

/// Knobs for a single [`run_process`] invocation.
#[derive(Clone, Debug, Default)]
pub struct ProcessOptions {
    /// Working directory to run in. Defaults to the current directory.
    pub workdir: Option<PathBuf>,
    /// Wall-clock timeout. After it elapses the child is killed and
    /// [`ProcessOutput::timed_out`] is set.
    pub timeout: Option<Duration>,
    /// Extra environment variables to set for the child.
    pub env: Vec<(String, String)>,
    /// Optional stdin to feed the child (then close).
    pub stdin: Option<String>,
}

/// The captured result of a finished (or killed) subprocess.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ProcessOutput {
    /// Exit code (`-1` if the process was killed or carried no code).
    pub exit_code: i32,
    /// Captured stdout (lossy UTF-8).
    pub stdout: String,
    /// Captured stderr (lossy UTF-8).
    pub stderr: String,
    /// True if the child was killed because it exceeded the timeout.
    pub timed_out: bool,
    /// True if the child was killed because the [`CancelToken`] fired.
    pub cancelled: bool,
}

/// Outcome of the wait race, internal to [`run_process`]. None of these handlers touch
/// the child, so the `child.wait()` borrow is released before we kill it.
enum Wait {
    Done(std::process::ExitStatus),
    Timeout,
    Cancel,
}

/// Runs `program args...` to completion, racing it against the timeout and cancel token.
///
/// # Errors
/// Returns [`ProviderError::NotFound`] if the program is not on `PATH`, or
/// [`ProviderError::Other`] for other spawn/wait I/O failures. A non-zero exit, a
/// timeout, or a cancel are reported in the returned [`ProcessOutput`], not as errors.
pub async fn run_process(
    program: &str,
    args: &[String],
    opts: &ProcessOptions,
    cancel: &CancelToken,
) -> Result<ProcessOutput, ProviderError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if opts.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = &opts.workdir {
        cmd.current_dir(dir);
    }
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProviderError::NotFound(program.to_owned()));
        }
        Err(e) => {
            return Err(ProviderError::Other(
                anyhow::Error::new(e).context(format!("spawning {program}")),
            ));
        }
    };

    // Feed stdin on its own task so a large prompt can't deadlock against output.
    if let (Some(input), Some(mut sink)) = (opts.stdin.clone(), child.stdin.take()) {
        tokio::spawn(async move {
            let _ = sink.write_all(input.as_bytes()).await;
            // `sink` drops here, closing the child's stdin.
        });
    }

    // Drain stdout/stderr concurrently with the wait, so pipes never fill and block.
    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf).await;
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf).await;
        buf
    });

    let waited = tokio::select! {
        status = child.wait() => match status {
            Ok(s) => Wait::Done(s),
            Err(e) => return Err(ProviderError::Other(
                anyhow::Error::new(e).context("waiting for child"),
            )),
        },
        () = cancel.cancelled() => Wait::Cancel,
        () = sleep_opt(opts.timeout) => Wait::Timeout,
    };

    let (timed_out, cancelled, status) = match waited {
        Wait::Done(s) => (false, false, Some(s)),
        Wait::Timeout => {
            let _ = child.start_kill();
            (true, false, child.wait().await.ok())
        }
        Wait::Cancel => {
            let _ = child.start_kill();
            (false, true, child.wait().await.ok())
        }
    };

    // After a kill, bound how long we wait to drain the pipes. A surviving grandchild
    // (e.g. a backgrounded process in `sh -c`, or an agent's tool subprocess) can keep
    // the stdout/stderr fd open, and `read_to_end` would otherwise block forever —
    // defeating the very timeout/cancel that just fired.
    let killed = timed_out || cancelled;
    let stdout = collect(out_task, killed).await;
    let stderr = collect(err_task, killed).await;
    let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);

    Ok(ProcessOutput {
        exit_code,
        stdout,
        stderr,
        timed_out,
        cancelled,
    })
}

/// Sleeps for `d`, or never resolves if `d` is `None`.
async fn sleep_opt(d: Option<Duration>) {
    match d {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending::<()>().await,
    }
}

/// How long to keep draining a killed child's pipes before abandoning partial output.
const KILL_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Joins a reader task. On the normal path it awaits fully; after a kill it bounds the
/// wait by a grace period and then aborts the task, so a surviving grandchild holding the
/// pipe open cannot hang the call.
async fn collect(task: tokio::task::JoinHandle<Vec<u8>>, killed: bool) -> String {
    let bytes = if killed {
        let abort = task.abort_handle();
        if let Ok(Ok(b)) = tokio::time::timeout(KILL_DRAIN_GRACE, task).await {
            b
        } else {
            abort.abort();
            Vec::new()
        }
    } else {
        task.await.unwrap_or_default()
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use super::{ProcessOptions, run_process};
    use crate::error::ProviderError;
    use crate::traits::CancelToken;
    use std::time::Duration;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let out = run_process(
            "sh",
            &args(&["-c", "echo out; echo err 1>&2; exit 3"]),
            &ProcessOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 3);
        assert!(out.stdout.contains("out"));
        assert!(out.stderr.contains("err"));
        assert!(!out.timed_out && !out.cancelled);
    }

    #[tokio::test]
    async fn times_out_and_kills() {
        let opts = ProcessOptions {
            timeout: Some(Duration::from_millis(150)),
            ..Default::default()
        };
        let out = run_process("sh", &args(&["-c", "sleep 5"]), &opts, &CancelToken::new())
            .await
            .unwrap();
        assert!(out.timed_out, "expected timeout");
        assert!(!out.cancelled);
    }

    #[tokio::test]
    async fn cancels_promptly() {
        let cancel = CancelToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let out = run_process(
            "sh",
            &args(&["-c", "sleep 5"]),
            &ProcessOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert!(out.cancelled, "expected cancel");
    }

    #[tokio::test]
    async fn missing_binary_is_not_found() {
        let err = run_process(
            "odin-definitely-not-a-real-binary-xyz",
            &[],
            &ProcessOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProviderError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn timeout_returns_even_if_a_grandchild_holds_the_pipe() {
        // The backgrounded `sleep 10` inherits the stdout pipe and outlives its parent
        // `sh`, which is killed at the timeout. Without bounding the reader drain, this
        // would block until the grandchild exits (~10s); with the fix it returns promptly.
        let opts = ProcessOptions {
            timeout: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let out = run_process(
            "sh",
            &args(&["-c", "sleep 10 & echo go; sleep 10"]),
            &opts,
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert!(out.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "run_process must return promptly after timeout, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn feeds_stdin() {
        let opts = ProcessOptions {
            stdin: Some("hello".to_owned()),
            ..Default::default()
        };
        let out = run_process("cat", &[], &opts, &CancelToken::new())
            .await
            .unwrap();
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.exit_code, 0);
    }
}
