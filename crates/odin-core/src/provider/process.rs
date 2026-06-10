//! Async subprocess management for provider CLIs.
//!
//! [`run_process`] spawns a command, streams and captures its stdout/stderr
//! concurrently (so a chatty agent never deadlocks on a full pipe), and races the
//! child against an optional timeout and a [`CancelToken`]. On timeout or cancel the
//! child is killed and reaped; partial output captured so far is still returned.
//!
//! On Unix the child is spawned in its own process group, so termination SIGKILLs the whole
//! group (the agent CLI plus any tool/`sh -c` grandchildren) via the `kill` binary — the crate is
//! `unsafe`-forbidden, so it can't call `libc::killpg` directly (see `kill_tree`). A graceful
//! SIGINT-then-SIGKILL escalation remains a deliberate later refinement.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use crate::error::ProviderError;
use crate::traits::CancelToken;

/// The POSIX shell Odin runs `run:` / gate / `shell.exec` command strings through, resolved once
/// and cached for the process.
///
/// Workflow command strings are POSIX-flavored by design (`&&`, `||`, `$(…)`, `[ … ]`, `>>`,
/// `2>&1`), so Odin always runs them through a POSIX shell rather than translating to `cmd`/
/// PowerShell. On Unix that shell is plain `sh` (always on `PATH`). On Windows — where `sh` is
/// usually *not* on `PATH` even though Git for Windows ships `sh.exe` — it is resolved by probing
/// `ODIN_SHELL`, then `sh`/`bash` on `PATH` (**skipping the WSL launchers** in `System32` and the
/// Store-app `…\Microsoft\WindowsApps` dir, which are not POSIX shells — invoking one with no
/// installed distro fails with "Windows Subsystem for Linux has no installed distributions"), then
/// Git-for-Windows install locations. A missing shell yields a [`ProviderError::ShellNotFound`]
/// with an actionable message, so the first shell step fails clearly instead of with a cryptic
/// `sh: not found`.
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

/// Directory names that hold only the **WSL launcher** (`bash.exe`/`wsl.exe`), never a POSIX
/// shell: the Windows system dirs (`System32`/`SysWOW64`/`Sysnative`) ship the optional-feature
/// WSL, and `WindowsApps` holds the Microsoft Store WSL app's execution-alias stub.
#[cfg(any(windows, test))]
const WSL_LAUNCHER_DIRS: [&str; 4] = ["system32", "syswow64", "sysnative", "windowsapps"];

/// True if `exe` lives in a [`WSL_LAUNCHER_DIRS`] directory — so a `sh`/`bash` resolved there must
/// be rejected (invoking it with no installed distro fails with "Windows Subsystem for Linux has
/// no installed distributions"). Matches on directory *name* anywhere in the path, so it is
/// independent of the Windows install drive **and of the `SystemRoot` env var** — which a process
/// launched from Git Bash / a service / a stripped shell may not see, and which an earlier
/// `SystemRoot`-based check wrongly relied on. No real POSIX shell ships under any of these names
/// (Git for Windows puts `sh.exe` under `…\Git\bin` / `…\Git\usr\bin`). Pure + testable
/// cross-platform (like [`sh_beside_git`]); tests pass forward-slash paths so `Path::components`
/// splits on every host.
#[cfg(any(windows, test))]
fn is_wsl_launcher_dir(exe: &std::path::Path) -> bool {
    let Some(parent) = exe.parent() else {
        return false;
    };
    parent.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        WSL_LAUNCHER_DIRS
            .iter()
            .any(|d| name.eq_ignore_ascii_case(d))
    })
}

// Always `Some` on Unix, but the signature mirrors the Windows version (which can be `None`).
#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)]
fn resolve_shell_platform() -> Option<String> {
    Some("sh".to_owned())
}

#[cfg(windows)]
fn resolve_shell_platform() -> Option<String> {
    // A real `sh`/`bash` already on PATH wins — but `which_on_path` skips the WSL launcher
    // `System32\bash.exe` (not a POSIX shell). Return the resolved FULL path, not the bare name:
    // `Command::new("bash")` would let Windows re-search PATH and land back on that launcher.
    for name in ["sh", "bash"] {
        if let Some(exe) = which_on_path(name) {
            return Some(exe.to_string_lossy().into_owned());
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

/// Finds `name.exe` on `PATH`, returning the first match — but never a WSL launcher location
/// (see [`is_wsl_launcher_dir`]), so a probe for `bash` can't resolve to `System32\bash.exe` or
/// the Store-app `WindowsApps\bash.exe`.
#[cfg(windows)]
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(format!("{name}.exe")))
        .find(|exe| exe.is_file() && !is_wsl_launcher_dir(exe))
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

/// A writer shared by every [`StreamSink`] minted from one [`StreamMux`].
type SharedWriter = Arc<AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// The origin of per-step [`StreamSink`]s: one shared, lock-guarded writer (typically the
/// terminal's stderr) that every sink locks before emitting a line — so the live output of
/// concurrently-running steps never interleaves mid-line. Cheap to clone (shared `Arc`); all
/// clones target the same underlying writer.
#[derive(Clone)]
pub struct StreamMux {
    writer: SharedWriter,
}

impl std::fmt::Debug for StreamMux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamMux").finish_non_exhaustive()
    }
}

impl StreamMux {
    /// A mux that tees live output to the process's **stderr** — stdout stays a clean data
    /// channel (the run summary / `--json`).
    #[must_use]
    pub fn to_stderr() -> Self {
        Self::to_writer(tokio::io::stderr())
    }

    /// A mux that tees to an arbitrary async writer. Lets an embedder redirect live step output
    /// (e.g. to a log file or a UI pane) instead of the terminal.
    #[must_use]
    pub fn to_writer(writer: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            writer: Arc::new(AsyncMutex::new(Box::new(writer))),
        }
    }

    /// Derives a [`StreamSink`] that prefixes each emitted line with `label`. All sinks from one
    /// mux share its writer, so their lines serialize.
    #[must_use]
    pub fn sink(&self, label: impl Into<Arc<str>>) -> StreamSink {
        StreamSink {
            label: label.into(),
            writer: Arc::clone(&self.writer),
        }
    }
}

/// A per-step live-output sink. When set on [`ProcessOptions::stream`], [`run_process`] tees each
/// completed line of the child's stdout/stderr to it as the bytes arrive (in addition to capturing
/// the full output in [`ProcessOutput`]), prefixed with the step's label so concurrent steps stay
/// legible. Cloning shares the underlying writer.
#[derive(Clone)]
pub struct StreamSink {
    label: Arc<str>,
    writer: SharedWriter,
}

impl std::fmt::Debug for StreamSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamSink")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// Cap on a single un-terminated streamed line. A pathological newline-free stream — a huge
/// one-line JSON blob, or a `\r`-only progress bar — would otherwise let `pending` grow until
/// EOF; past this we flush it as a partial line so the live buffer stays bounded and the output
/// still appears as it arrives. (The full output is still captured intact in [`ProcessOutput`].)
const MAX_STREAM_LINE: usize = 64 * 1024;

impl StreamSink {
    /// Feeds a chunk of child output: splits on `\n`, emits each completed line, and buffers any
    /// trailing partial line in `pending` for the next chunk — flushing early if it grows past
    /// [`MAX_STREAM_LINE`] so a newline-free stream can't buffer unbounded.
    async fn feed(&self, bytes: &[u8], pending: &mut Vec<u8>) {
        let mut start = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                pending.extend_from_slice(&bytes[start..i]);
                self.emit_line(pending).await;
                pending.clear();
                start = i + 1;
            }
        }
        pending.extend_from_slice(&bytes[start..]);
        if pending.len() >= MAX_STREAM_LINE {
            self.emit_line(pending).await;
            pending.clear();
        }
    }

    /// Emits any buffered trailing partial line — called once at EOF.
    async fn flush(&self, pending: &mut Vec<u8>) {
        if !pending.is_empty() {
            self.emit_line(pending).await;
            pending.clear();
        }
    }

    /// Writes one framed line — `<label> │ <text>` — under the shared writer lock, so it can't
    /// interleave with another sink's line.
    async fn emit_line(&self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        // Drop a trailing CR so CRLF output isn't double-spaced on the terminal.
        let text = text.strip_suffix('\r').unwrap_or(&text);
        let framed = format!("{} │ {text}\n", self.label);
        let mut w = self.writer.lock().await;
        let _ = w.write_all(framed.as_bytes()).await;
        let _ = w.flush().await;
    }
}

/// Knobs for a single [`run_process`] invocation.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
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
    /// Live-stream the child's stdout/stderr to this sink as it arrives (in addition to capturing
    /// it in [`ProcessOutput`]). `None` (the default) = capture only.
    pub stream: Option<StreamSink>,
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
    // On Unix, put the child in its OWN process group so a kill reaps the whole tree — the agent
    // CLI plus the tool subprocesses / `sh -c` grandchildren it forks. Without this, SIGKILL hits
    // only the direct child and the grandchildren survive as orphans that keep mutating the
    // workspace (and a slot pool would then reset + re-lease a dir still being written). See
    // `kill_tree`.
    #[cfg(unix)]
    cmd.process_group(0);
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

    // Drain stdout/stderr concurrently with the wait, so pipes never fill and block. With a
    // stream sink set, each pipe is also teed line-by-line to it as the bytes arrive.
    let out_pipe = child.stdout.take().expect("stdout piped");
    let err_pipe = child.stderr.take().expect("stderr piped");
    let out_task = tokio::spawn(drain_pipe(out_pipe, opts.stream.clone()));
    let err_task = tokio::spawn(drain_pipe(err_pipe, opts.stream.clone()));

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
            kill_tree(&mut child).await;
            (true, false, child.wait().await.ok())
        }
        Wait::Cancel => {
            kill_tree(&mut child).await;
            (false, true, child.wait().await.ok())
        }
    };

    // Drain both pipes concurrently, each bounded by `KILL_DRAIN_GRACE` (see `collect`). The
    // bound applies on EVERY path, not just after a kill: a child that exits *cleanly* but leaks a
    // backgrounded grandchild (`./server & echo done; exit 0`, or an agent's lingering tool
    // subprocess) leaves that grandchild holding the stdout/stderr fd open, so an unbounded
    // `read_to_end` would wedge the run forever even though `child.wait()` already returned.
    // Draining concurrently also avoids paying the grace twice (~4s) when both fds are held.
    let (stdout, stderr) = tokio::join!(collect(out_task), collect(err_task));
    let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);

    Ok(ProcessOutput {
        exit_code,
        stdout,
        stderr,
        timed_out,
        cancelled,
    })
}

/// Reads `pipe` to EOF and returns all of its bytes. With a [`StreamSink`] it also tees each
/// completed line to the sink as it arrives (the live path); without one it does a single
/// `read_to_end` (the capture-only fast path, byte-identical to the pre-streaming behavior).
async fn drain_pipe<R>(mut pipe: R, sink: Option<StreamSink>) -> Vec<u8>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buf = Vec::new();
    let Some(sink) = sink else {
        let _ = pipe.read_to_end(&mut buf).await;
        return buf;
    };
    let mut chunk = [0_u8; 8192];
    let mut pending = Vec::new();
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                sink.feed(&chunk[..n], &mut pending).await;
            }
        }
    }
    sink.flush(&mut pending).await;
    buf
}

/// Kills a timed-out/cancelled child **and its descendants**. On Unix the child leads its own
/// process group (`process_group(0)` at spawn), so one SIGKILL to the negated group id reaps the
/// agent CLI's tool subprocesses and `sh -c` grandchildren too — which would otherwise survive the
/// direct-child kill and keep running (and writing into the workspace). We shell out to `kill`
/// because the crate forbids `unsafe`, so it can't call `libc::killpg` directly; `start_kill` is a
/// belt-and-suspenders fallback (and the only step on non-Unix).
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // `-<pid>` targets the process group (pgid == leader pid). Best-effort: ignore failures.
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.start_kill();
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

/// Joins a pipe-reader task, bounding the wait by [`KILL_DRAIN_GRACE`] and then aborting it.
/// The bound applies even when the child exited cleanly: at that point the child has closed its
/// own pipe ends so EOF is imminent (at most a pipe buffer remains), UNLESS a surviving grandchild
/// inherited the fd and holds it open — in which case an unbounded join would hang the run. The
/// grace is generous enough to drain any real buffered tail and short enough to keep a leaked
/// grandchild from wedging the call.
async fn collect(task: tokio::task::JoinHandle<Vec<u8>>) -> String {
    let abort = task.abort_handle();
    let bytes = if let Ok(Ok(b)) = tokio::time::timeout(KILL_DRAIN_GRACE, task).await {
        b
    } else {
        abort.abort();
        Vec::new()
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
impl StreamMux {
    /// A mux that captures everything teed through it into a shared buffer, for assertions.
    /// Crate-visible so engine tests (not just this module) can assert on streamed output.
    pub(crate) fn capturing() -> (Self, Arc<std::sync::Mutex<Vec<u8>>>) {
        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        (Self::to_writer(CaptureWriter(Arc::clone(&buf))), buf)
    }
}

/// A test [`AsyncWrite`] that appends everything written into a shared `Vec` for inspection.
#[cfg(test)]
struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl AsyncWrite for CaptureWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
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

    #[test]
    fn the_wsl_launcher_is_not_a_shell() {
        use super::is_wsl_launcher_dir;
        use std::path::Path;
        // Forward slashes so `Path::components` splits on every host (see the helper's doc). No
        // `SystemRoot` is consulted — the match is purely on directory name, in any case and on
        // any Windows install drive.
        for launcher in [
            "C:/Windows/System32/bash.exe", // optional-feature WSL (the user's exact path)
            "c:/windows/system32/bash.exe", // case-insensitive
            "C:/Windows/SysWOW64/wsl.exe",
            "C:/Windows/Sysnative/bash.exe",
            "D:/Windows/System32/bash.exe", // any drive, not just C:
            // Microsoft Store WSL app's execution-alias stub — not under System32.
            "C:/Users/me/AppData/Local/Microsoft/WindowsApps/bash.exe",
            "c:/users/me/appdata/local/microsoft/windowsapps/wsl.exe",
        ] {
            assert!(
                is_wsl_launcher_dir(Path::new(launcher)),
                "must reject the WSL launcher: {launcher}"
            );
        }
        // A real Git-for-Windows shell (or any non-launcher bash) is accepted.
        for shell in [
            "C:/Program Files/Git/bin/sh.exe",
            "C:/Program Files/Git/usr/bin/sh.exe",
            "C:/tools/bash.exe",
        ] {
            assert!(
                !is_wsl_launcher_dir(Path::new(shell)),
                "must accept a real shell: {shell}"
            );
        }
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

    // The inverse of the test above: the child exits CLEANLY (exit 0, no timeout, no cancel) but
    // leaks a backgrounded grandchild that keeps the stdout pipe open. The drain must still be
    // bounded — an unbounded `read_to_end` here wedged the run until the grandchild died.
    #[cfg(unix)]
    #[tokio::test]
    async fn clean_exit_returns_even_if_a_grandchild_holds_the_pipe() {
        let started = std::time::Instant::now();
        let out = run_process(
            "sh",
            &args(&["-c", "sleep 10 & echo done; exit 0"]),
            &ProcessOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 0, "the child exited cleanly");
        assert!(!out.timed_out && !out.cancelled);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a cleanly-exited step with a leaked grandchild must not wedge the run, took {:?}",
            started.elapsed()
        );
    }

    // The process-group kill must reap a BACKGROUNDED grandchild, not just the direct child. The
    // grandchild here would `touch` a marker after 1s; the group is killed at 200ms, so if it were
    // orphaned (only the direct child killed) the marker would appear — and must not.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_reaps_backgrounded_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let script = format!("(sleep 1; touch '{}') & sleep 30", marker.display());
        let opts = ProcessOptions {
            timeout: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let out = run_process("sh", &args(&["-c", &script]), &opts, &CancelToken::new())
            .await
            .unwrap();
        assert!(out.timed_out);
        // Wait past when the grandchild WOULD have touched the marker, had it survived.
        tokio::time::sleep(Duration::from_millis(1400)).await;
        assert!(
            !marker.exists(),
            "the backgrounded grandchild must be reaped by the process-group kill, not orphaned"
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
    async fn streams_lines_live_while_capturing() {
        let sh = shell_or_skip!();
        let (mux, captured) = super::StreamMux::capturing();
        let opts = ProcessOptions {
            stream: Some(mux.sink("step1")),
            ..Default::default()
        };
        let out = run_process(
            sh,
            // Two complete lines (one per stream) plus a trailing line with no newline.
            &args(&["-c", "echo line-a; echo line-b 1>&2; printf 'no-newline'"]),
            &opts,
            &CancelToken::new(),
        )
        .await
        .unwrap();
        // The full output is still captured, exactly as without streaming.
        assert!(out.stdout.contains("line-a"));
        assert!(out.stderr.contains("line-b"));
        assert!(out.stdout.contains("no-newline"));
        // ...and each line was teed live to the mux, framed with the step label.
        let teed = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(teed.contains("step1 │ line-a"), "teed: {teed:?}");
        assert!(teed.contains("step1 │ line-b"), "teed: {teed:?}");
        // The trailing partial line (no newline) is flushed at EOF, not dropped.
        assert!(teed.contains("step1 │ no-newline"), "teed: {teed:?}");
    }

    #[tokio::test]
    async fn streaming_survives_a_timeout_kill() {
        // Stream a line, then hang past the timeout: the kill must still return promptly (the
        // drain task — possibly mid-emit, holding the writer lock — is aborted without deadlock),
        // and the line emitted before the kill must have been teed live.
        let sh = shell_or_skip!();
        let (mux, captured) = super::StreamMux::capturing();
        let opts = ProcessOptions {
            timeout: Some(Duration::from_millis(200)),
            stream: Some(mux.sink("slow")),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let out = run_process(
            sh,
            &args(&["-c", "echo early-line; sleep 5"]),
            &opts,
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert!(out.timed_out, "expected a timeout");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must return promptly after the kill, took {:?}",
            started.elapsed()
        );
        let teed = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(teed.contains("slow │ early-line"), "teed: {teed:?}");
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
