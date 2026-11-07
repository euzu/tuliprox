//! `run-process`: a plugin runs a local program.
//!
//! This is the one host function that leaves the sandbox for the machine itself, so it is
//! gated by its own capability (`process`) and says so loudly in the admin's review. The
//! program is started directly, never through a shell, so there are no quoting rules and
//! no shell-injection surface from what a plugin passes.
//!
//! Containment is what a plugin cannot get around:
//!
//! * The environment is empty except `PATH` and what the plugin passes, so a plugin cannot
//!   read the process's own environment through a child.
//! * Input is at most 1 MiB, there are at most 256 arguments, and stdout and stderr are each
//!   cut to their first 64 KiB (the rest is drained so the child never blocks on a full pipe).
//! * The wait is 1 to 300 seconds. After that the child is killed and the call fails. A
//!   host-side deadline that drops the call kills the child too (`kill_on_drop`).
//! * Every run is logged by program name, never by arguments, which may carry a credential.

use log::info;
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Most bytes written to the program's stdin.
pub(crate) const MAX_STDIN_BYTES: usize = 1024 * 1024;
/// Most bytes kept of each of stdout and stderr.
pub(crate) const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
/// Most arguments one call passes.
pub(crate) const MAX_ARGS: usize = 256;
/// The longest a call may wait.
pub(crate) const MAX_TIMEOUT_SECS: u32 = 300;

/// What a plugin asks to run.
pub(crate) struct Request {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) stdin: Vec<u8>,
    pub(crate) timeout_secs: u32,
}

/// What a finished program left behind.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Output {
    /// `None` when a signal ended it.
    pub(crate) status: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// A request that cannot be run, with the reason a plugin author can act on.
fn check(request: &Request) -> Result<(), String> {
    if request.program.trim().is_empty() || request.program.contains('\0') {
        return Err("run-process needs a program".to_string());
    }
    if request.args.len() > MAX_ARGS {
        return Err(format!("run-process takes at most {MAX_ARGS} arguments"));
    }
    if request.args.iter().any(|arg| arg.contains('\0')) {
        return Err("a run-process argument cannot contain a NUL byte".to_string());
    }
    if request.stdin.len() > MAX_STDIN_BYTES {
        return Err(format!("run-process input is over {MAX_STDIN_BYTES} bytes"));
    }
    for (name, value) in &request.env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err("a run-process environment entry is not a valid NAME=value pair".to_string());
        }
    }
    Ok(())
}

/// Reads at most [`MAX_OUTPUT_BYTES`] and drains the rest, so the child never blocks on a full pipe.
async fn read_capped<R: AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut kept = Vec::new();
    let _ = (&mut reader).take(MAX_OUTPUT_BYTES).read_to_end(&mut kept).await;
    let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    kept
}

#[cfg(unix)]
struct ProcessGroupGuard {
    pgid: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn new(child_id: Option<u32>) -> Self {
        let pgid = child_id.and_then(|id| libc::pid_t::try_from(id).ok()).filter(|&id| id > 1);
        Self { pgid }
    }

    fn kill(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: `pgid` is guaranteed to be a valid PID > 1 returned by `spawn()` and converted to `pid_t`.
            // Sending `SIGKILL` to this process group cleans up the child and any grandchildren spawned in its group.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) { self.kill(); }
}

/// Run `request` for plugin `plugin`, if it holds the `process` capability.
pub(crate) async fn run(plugin: &str, capable: bool, request: Request) -> Result<Output, String> {
    if !capable {
        return Err("run-process needs the process capability".to_string());
    }
    check(&request)?;
    info!("plugin `{plugin}` runs the program `{}`", request.program);

    let mut command = tokio::process::Command::new(&request.program);
    command
        .args(&request.args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .envs(request.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|err| format!("could not run the program: {err}"))?;

    let (Some(mut stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("could not attach to the program's pipes".to_string());
    };
    let input = request.stdin;
    // Written on its own, so a program that answers before it has read everything cannot deadlock the call.
    let write = async move {
        // A program that stops reading early is not an error of ours: its exit status says how it went.
        let _ = stdin.write_all(&input).await;
        let _ = stdin.shutdown().await;
        drop(stdin);
    };

    #[cfg(unix)]
    let mut guard = ProcessGroupGuard::new(child.id());

    let wait = Duration::from_secs(u64::from(request.timeout_secs.clamp(1, MAX_TIMEOUT_SECS)));
    let finished = tokio::time::timeout(wait, async {
        let write_task = tokio::spawn(write);
        let stdout_task = tokio::spawn(read_capped(stdout));
        let stderr_task = tokio::spawn(read_capped(stderr));
        let status = child.wait().await;
        #[cfg(unix)]
        guard.kill();
        let (stdout, stderr, _) = tokio::join!(stdout_task, stderr_task, write_task);
        (stdout.unwrap_or_default(), stderr.unwrap_or_default(), status)
    })
    .await;
    match finished {
        Ok((stdout, stderr, Ok(status))) => Ok(Output { status: status.code(), stdout, stderr }),
        Ok((_, _, Err(err))) => Err(format!("waiting for the program failed: {err}")),
        // The future is dropped here, and `kill_on_drop` ends the child.
        Err(_) => Err(format!("the program was still running after {} s and was stopped", wait.as_secs())),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn request(program: &str, args: &[&str]) -> Request {
        Request {
            program: program.to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            env: Vec::new(),
            stdin: Vec::new(),
            timeout_secs: 10,
        }
    }

    async fn run_as_capable(request: Request) -> Result<Output, String> { run("test", true, request).await }

    #[tokio::test]
    async fn without_the_capability_nothing_runs() {
        let err = run("test", false, request("/bin/echo", &["x"])).await.unwrap_err();
        assert!(err.contains("process capability"), "{err}");
    }

    #[tokio::test]
    async fn stdin_reaches_the_program_and_its_output_comes_back() {
        let mut cat = request("/bin/cat", &[]);
        cat.stdin = b"hello".to_vec();
        let output = run_as_capable(cat).await.unwrap();
        assert_eq!(output, Output { status: Some(0), stdout: b"hello".to_vec(), stderr: Vec::new() });
    }

    #[tokio::test]
    async fn the_exit_status_and_stderr_are_reported() {
        let output = run_as_capable(request("/bin/sh", &["-c", "echo oops >&2; exit 3"])).await.unwrap();
        assert_eq!(output.status, Some(3));
        assert_eq!(output.stderr, b"oops\n");
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error() {
        let err = run_as_capable(request("/definitely/not/here", &[])).await.unwrap_err();
        assert!(err.starts_with("could not run the program"), "{err}");
    }

    #[tokio::test]
    async fn the_environment_is_empty_but_for_path_and_what_was_passed() {
        let mut env = request("/usr/bin/env", &[]);
        env.env = vec![("TULIPROX_EVENT_ID".to_string(), "system.info".to_string())];
        let output = run_as_capable(env).await.unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let mut names: Vec<&str> = text.lines().filter_map(|line| line.split('=').next()).collect();
        names.sort_unstable();
        assert!(names.contains(&"TULIPROX_EVENT_ID") && names.contains(&"PATH"), "{names:?}");
        assert!(
            names.iter().all(|name| ["PATH", "TULIPROX_EVENT_ID", "PWD", "SHLVL", "_", "OLDPWD"].contains(name)),
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn a_program_that_runs_too_long_is_stopped() {
        let mut sleeper = request("/bin/sleep", &["30"]);
        sleeper.timeout_secs = 1;
        let started = std::time::Instant::now();
        let err = run_as_capable(sleeper).await.unwrap_err();
        assert!(err.contains("still running"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn output_is_cut_but_the_program_is_not_blocked() {
        let output = run_as_capable(request("/bin/sh", &["-c", "head -c 300000 /dev/zero"])).await.unwrap();
        assert_eq!(output.status, Some(0));
        assert_eq!(output.stdout.len() as u64, MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn input_the_program_ignores_does_not_hang_the_call() {
        let mut quick = request("/bin/true", &[]);
        quick.stdin = vec![b'x'; MAX_STDIN_BYTES];
        assert_eq!(run_as_capable(quick).await.unwrap().status, Some(0));
    }

    #[tokio::test]
    async fn malformed_requests_are_refused_before_anything_starts() {
        let mut bad = request("", &[]);
        assert!(run_as_capable(bad).await.is_err());
        bad = request("/bin/echo", &["a\0b"]);
        assert!(run_as_capable(bad).await.is_err());
        bad = request("/bin/echo", &[]);
        bad.env = vec![("A=B".to_string(), "x".to_string())];
        assert!(run_as_capable(bad).await.is_err());
        bad = request("/bin/echo", &[]);
        bad.stdin = vec![0; MAX_STDIN_BYTES + 1];
        assert!(run_as_capable(bad).await.is_err());
        let many: Vec<&str> = vec!["x"; MAX_ARGS + 1];
        assert!(run_as_capable(request("/bin/echo", &many)).await.is_err());
    }

    #[tokio::test]
    async fn grandchild_does_not_hang_call() {
        let mut req = request("/bin/sh", &["-c", "sleep 30 & echo done"]);
        req.timeout_secs = 20;
        let started = std::time::Instant::now();
        let output = run_as_capable(req).await.unwrap();
        assert_eq!(output.status, Some(0));
        assert_eq!(output.stdout, b"done\n");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn grandchild_is_killed_when_call_ends() {
        let mut req = request("/bin/sh", &["-c", "sleep 30 & echo $!; echo done"]);
        req.timeout_secs = 20;
        let output = run_as_capable(req).await.unwrap();
        assert_eq!(output.status, Some(0));
        let text = String::from_utf8(output.stdout).unwrap();
        let pid: i32 = text.lines().next().unwrap().trim().parse().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // SAFETY: `pid` was reported by the test child via `echo $!`. Signal 0 performs error checking
        // without sending a signal, probing whether the process is still alive.
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        assert!(!alive, "grandchild process {pid} should have been killed");
    }
}
