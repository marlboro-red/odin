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
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use crate::error::ProviderError;
use crate::traits::CancelToken;

/// The POSIX shell Odin runs `run:` / gate / `shell.exec` command strings through, resolved once
/// and cached for the process.
///
/// Workflow command strings are POSIX-flavored by design (`&&`, `||`, `$(…)`, `[ … ]`, `>>`,
/// `2>&1`), so Odin always runs them through a POSIX shell rather than translating to `cmd`/
/// PowerShell. On Unix that shell is plain `sh` (always on `PATH`). On Windows — where `sh` is
/// usually *not* on `PATH` even though Git for Windows ships `sh.exe` — it is resolved by probing
/// `ODIN_SHELL`, then `sh`/`bash` on `PATH`, then Git-for-Windows install locations. A missing
/// shell yields a [`ProviderError::ShellNotFound`] with an actionable
/// message, so the first shell step fails clearly instead of with a cryptic `sh: not found`.
///
/// # Errors
/// [`ProviderError::ShellNotFound`] if no POSIX shell can be resolved (Windows without Git for
/// Windows and without `ODIN_SHELL` set).
pub fn posix_shell() -> Result<&'static str, ProviderError> {
    static SHELL: OnceLock<Option<String>> = OnceLock::new();
    SHELL.get_or_init(resolve_shell).as_deref().ok_or_else(|| {
        ProviderError::ShellNotFound(
            "no POSIX shell for `run:`/gate/`shell.exec` commands — install Git for Windows (it \
             ships sh.exe) or set ODIN_SHELL to a shell path"
                .to_owned(),
        )
    })
}

/// Resolves the shell: an explicit `ODIN_SHELL` override wins, else the platform default.
fn resolve_shell() -> Option<String> {
    shell_from_override(std::env::var("ODIN_SHELL").ok().as_deref()).or_else(resolve_shell_platform)
}

/// Normalizes an `ODIN_SHELL` value: trimmed, with a blank treated as unset.
fn shell_from_override(val: Option<&str>) -> Option<String> {
    val.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The `sh.exe` locations Git for Windows ships alongside `git.exe`. `git.exe` lives in
/// `<root>\cmd` (the copy on `PATH`) or `<root>\bin`, and `sh.exe` is in `<root>\bin` and
/// `<root>\usr\bin` — so going up two levels from `git.exe` and appending those is robust.
/// (Compiled on Windows, where it's used, and in test builds, which cover it cross-platform.)
#[cfg(any(windows, test))]
fn sh_beside_git(git_exe: &std::path::Path) -> Vec<PathBuf> {
    git_exe
        .parent()
        .and_then(std::path::Path::parent)
        .map(|root| {
            vec![
                root.join("bin").join("sh.exe"),
                root.join("usr").join("bin").join("sh.exe"),
            ]
        })
        .unwrap_or_default()
}

// Always `Some` on Unix, but the signature mirrors the Windows version (which can be `None`).
#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)]
fn resolve_shell_platform() -> Option<String> {
    Some("sh".to_owned())
}

#[cfg(windows)]
fn resolve_shell_platform() -> Option<String> {
    // A shell already on PATH (Git Bash put on PATH, or any other sh/bash) wins.
    for name in ["sh", "bash"] {
        if which_on_path(name).is_some() {
            return Some(name.to_owned());
        }
    }
    // Otherwise probe the sh.exe Git for Windows ships — derived from `git` on PATH (the user is
    // assumed to have git), then well-known install roots.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(git) = which_on_path("git") {
        candidates.extend(sh_beside_git(&git));
    }
    for root in well_known_git_roots() {
        candidates.push(root.join("bin").join("sh.exe"));
        candidates.push(root.join("usr").join("bin").join("sh.exe"));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Finds `name.exe` on `PATH`, returning the first match.
#[cfg(windows)]
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(format!("{name}.exe")))
        .find(|exe| exe.is_file())
}

/// Well-known Git-for-Windows install roots (per-machine and per-user).
#[cfg(windows)]
fn well_known_git_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            roots.push(PathBuf::from(base).join("Git"));
        }
    }
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(base).join("Programs").join("Git"));
    }
    roots
}

/// Odin-internal secrets scrubbed from every spawned child's environment. Providers, actions,
/// gates, and `run:` steps all spawn through [`run_process`], and an agent CLI (or any shell a
/// `run:`/gate executes) inherits the launching process's environment — so without this the
/// daemon's own webhook HMAC secret would be readable by every agent subprocess it starts.
/// (Trusted internal `git` invocations spawn outside this path and run no untrusted code.)
const SHIELDED_ENV: &[&str] = &["ODIN_WEBHOOK_SECRET"];

/// Environment that makes every `git` Odin drives treat content as **raw bytes** — no CRLF↔LF
/// normalization — so snapshots, diffs, and worktree checkouts are byte-stable across platforms.
/// On Windows the Git-for-Windows installer defaults to `core.autocrlf=true`, which would
/// otherwise lay CRLF on checkout while `HEAD` holds LF, making `git diff` report every line
/// changed and snapshots round-trip differently. Injected via `GIT_CONFIG_*` (git ≥ 2.31) so it
/// overrides global/repo config **for Odin's invocations only**, never writing the user's repo
/// config. Applied to all of Odin's git calls (workspace + engine); harmless on Unix, where
/// `autocrlf` is already off.
pub(crate) const GIT_PORTABLE_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_COUNT", "2"),
    ("GIT_CONFIG_KEY_0", "core.autocrlf"),
    ("GIT_CONFIG_VALUE_0", "false"),
    ("GIT_CONFIG_KEY_1", "core.safecrlf"),
    ("GIT_CONFIG_VALUE_1", "false"),
];

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
    // Strip Odin's own secrets last, so neither the inherited environment nor an explicit
    // `opts.env` entry can leak them into the child (defense in depth).
    for key in SHIELDED_ENV {
        cmd.env_remove(key);
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

#[cfg(test)]
mod shell_resolution_tests {
    #[test]
    fn override_is_trimmed_and_blank_is_unset() {
        use super::shell_from_override;
        assert_eq!(shell_from_override(Some(" bash ")).as_deref(), Some("bash"));
        assert_eq!(shell_from_override(Some("")), None);
        assert_eq!(shell_from_override(Some("   ")), None);
        assert_eq!(shell_from_override(None), None);
    }

    #[test]
    fn sh_is_derived_beside_git() {
        use super::sh_beside_git;
        // `git.exe` on PATH lives in `<root>\cmd`; `sh.exe` ships in `<root>\bin` and
        // `<root>\usr\bin`.
        let cands = sh_beside_git(std::path::Path::new("C:/Program Files/Git/cmd/git.exe"));
        let strs: Vec<String> = cands
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert!(
            strs.iter().any(|s| s.ends_with("Git/bin/sh.exe")),
            "{strs:?}"
        );
        assert!(
            strs.iter().any(|s| s.ends_with("Git/usr/bin/sh.exe")),
            "{strs:?}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn the_unix_shell_is_plain_sh() {
        assert_eq!(super::resolve_shell_platform().as_deref(), Some("sh"));
    }
}

#[cfg(test)]
mod git_env_tests {
    use super::{GIT_PORTABLE_ENV, ProcessOptions, run_process};
    use crate::traits::CancelToken;

    async fn git(dir: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> String {
        let opts = ProcessOptions {
            workdir: Some(dir.to_path_buf()),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..ProcessOptions::default()
        };
        let owned: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        run_process("git", &owned, &opts, &CancelToken::new())
            .await
            .unwrap()
            .stdout
    }

    /// `GIT_CONFIG_COUNT` must equal the number of key/value pairs, or git ignores the extras.
    #[test]
    fn portable_env_count_matches_keys() {
        let count: usize = GIT_PORTABLE_ENV
            .iter()
            .find(|(k, _)| *k == "GIT_CONFIG_COUNT")
            .unwrap()
            .1
            .parse()
            .unwrap();
        let keys = GIT_PORTABLE_ENV
            .iter()
            .filter(|(k, _)| k.starts_with("GIT_CONFIG_KEY_"))
            .count();
        let vals = GIT_PORTABLE_ENV
            .iter()
            .filter(|(k, _)| k.starts_with("GIT_CONFIG_VALUE_"))
            .count();
        assert_eq!(count, keys, "GIT_CONFIG_COUNT must match the key count");
        assert_eq!(keys, vals);
    }

    /// Proves the mechanism cross-platform: in a repo configured `core.autocrlf=true`, adding a
    /// CRLF file normally strips the CR, but adding it with `GIT_PORTABLE_ENV` preserves the bytes
    /// — so Odin's snapshots/diffs are stable even where autocrlf is the default (Windows).
    #[tokio::test]
    async fn portable_env_disables_autocrlf_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"], &[]).await;
        git(p, &["config", "core.autocrlf", "true"], &[]).await;

        // A separate file per config so git's stat cache can't reuse a prior staging.
        std::fs::write(p.join("norm.txt"), "alpha\r\nbeta\r\n").unwrap();
        git(p, &["add", "norm.txt"], &[]).await;
        let normalized = git(p, &["cat-file", "-p", ":norm.txt"], &[]).await;
        assert!(
            !normalized.contains('\r'),
            "autocrlf=true should strip CR: {normalized:?}"
        );

        std::fs::write(p.join("raw.txt"), "alpha\r\nbeta\r\n").unwrap();
        git(p, &["add", "raw.txt"], GIT_PORTABLE_ENV).await;
        let raw = git(p, &["cat-file", "-p", ":raw.txt"], &[]).await;
        assert!(
            raw.contains('\r'),
            "GIT_PORTABLE_ENV must preserve CR (autocrlf forced off): {raw:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{ProcessOptions, posix_shell, run_process};
    use crate::error::ProviderError;
    use crate::traits::CancelToken;
    use std::time::Duration;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Resolves the shell or returns `None` to skip — these tests use POSIX command strings, which
    /// run on any platform with a shell (Unix `sh`, or Git Bash's `sh.exe` on Windows). The skip
    /// only fires on a Windows box with no shell, where shell behavior can't be exercised anyway.
    macro_rules! shell_or_skip {
        () => {
            match posix_shell() {
                Ok(s) => s,
                Err(_) => return,
            }
        };
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let sh = shell_or_skip!();
        let out = run_process(
            sh,
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
        let sh = shell_or_skip!();
        let opts = ProcessOptions {
            timeout: Some(Duration::from_millis(150)),
            ..Default::default()
        };
        let out = run_process(sh, &args(&["-c", "sleep 5"]), &opts, &CancelToken::new())
            .await
            .unwrap();
        assert!(out.timed_out, "expected timeout");
        assert!(!out.cancelled);
    }

    #[tokio::test]
    async fn cancels_promptly() {
        let sh = shell_or_skip!();
        let cancel = CancelToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let out = run_process(
            sh,
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
        // No shell needed — spawns a bogus program directly, portable across platforms.
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

    // Genuinely Unix-coupled: relies on pipe inheritance + `&` backgrounding and on a process
    // group surviving the parent's kill — Windows has no process-group teardown.
    #[cfg(unix)]
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
    async fn shields_odin_secret_from_the_child() {
        let sh = shell_or_skip!();
        // ODIN_WEBHOOK_SECRET must never reach a spawned child, even when present in its
        // environment; an unrelated var must survive. (We seed it via `opts.env` rather than
        // the parent process env because `std::env::set_var` is `unsafe`, which the crate
        // forbids — and `env_remove` strips inherited and explicit entries alike.)
        let opts = ProcessOptions {
            env: vec![
                ("ODIN_WEBHOOK_SECRET".to_owned(), "leaked".to_owned()),
                ("ODIN_KEEP_ME".to_owned(), "kept".to_owned()),
            ],
            ..Default::default()
        };
        let out = run_process(
            sh,
            &args(&[
                "-c",
                "echo \"${ODIN_WEBHOOK_SECRET:-CLEAN}:${ODIN_KEEP_ME:-MISSING}\"",
            ]),
            &opts,
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.stdout.trim(), "CLEAN:kept");
    }

    #[tokio::test]
    async fn feeds_stdin() {
        // Route `cat` through the shell so it resolves on Windows (Git Bash ships cat, but it may
        // not be on PATH for a bare `Command::new("cat")`).
        let sh = shell_or_skip!();
        let opts = ProcessOptions {
            stdin: Some("hello".to_owned()),
            ..Default::default()
        };
        let out = run_process(sh, &args(&["-c", "cat"]), &opts, &CancelToken::new())
            .await
            .unwrap();
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.exit_code, 0);
    }
}
