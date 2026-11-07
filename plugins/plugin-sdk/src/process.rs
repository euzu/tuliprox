//! `host::run-process`: run a local program. Needs the `process` capability.
//!
//! The host starts the program directly (never through a shell) as the user tuliprox runs
//! as, with an empty environment except `PATH` and what [`Command::env`] adds, writes
//! [`Command::stdin`] and closes it, and waits at most [`Command::timeout_secs`]. Output is
//! cut to the first 64 KiB of stdout and of stderr.
//!
//! ```
//! use tuliprox_plugin_sdk::{mock, process::{self, Command}};
//!
//! mock::reset();
//! mock::process_respond("/bin/echo", Ok(process::Output { status: Some(0), stdout: b"hi\n".to_vec(), stderr: Vec::new() }));
//! let output = Command::new("/bin/echo").arg("hi").run().unwrap();
//! assert!(output.success());
//! assert_eq!(output.stdout, b"hi\n");
//! ```

use crate::sys;

/// A program to run, built up and then started with [`Command::run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Vec<u8>,
    /// The host waits this long (1 to 300 seconds), then stops the program.
    pub timeout_secs: u32,
}

/// What a finished program left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// `None` when a signal ended the program.
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    /// Exited with status 0.
    #[must_use]
    pub const fn success(&self) -> bool { matches!(self.status, Some(0)) }
}

impl Command {
    /// A program with no arguments, no input and a 30 second wait.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self { program: program.into(), args: Vec::new(), env: Vec::new(), stdin: Vec::new(), timeout_secs: 30 }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    #[must_use]
    pub fn stdin(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.stdin = input.into();
        self
    }

    #[must_use]
    pub const fn timeout_secs(mut self, secs: u32) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Runs the program and waits for it. `Err` says the host refused (no capability, a
    /// malformed request), the program could not be started, or it ran past the wait. A
    /// program that ran and failed is `Ok` with its status.
    pub fn run(&self) -> Result<Output, String> {
        let output = sys::run_process(&self.program, &self.args, &self.env, &self.stdin, self.timeout_secs)?;
        Ok(Output { status: output.status, stdout: output.stdout, stderr: output.stderr })
    }
}
