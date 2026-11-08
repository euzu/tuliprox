//! The program side of the plugin: run the program for one notification and say how the
//! outcome ends the attempt for the host's outbox.
//!
//! # What the program gets
//!
//! * The event as JSON on its standard input (`id`, `severity`, `timestamp`, `instance`,
//!   `title`, `body`, `fields`, `sections`), so a script can pick out what it needs.
//! * `TULIPROX_EVENT_ID`, `TULIPROX_EVENT_SEVERITY` and `TULIPROX_EVENT_TITLE` in its
//!   environment, for a script that only wants to branch on the event.
//! * Nothing else of tuliprox's environment: the host starts it with `PATH` and these.
//!
//! The program is started directly, never through a shell.
//!
//! # Retry contract
//!
//! | The program | Result |
//! | --- | --- |
//! | exits 0 | delivered |
//! | exits with another status, or is ended by a signal | transient: retried with backoff |
//! | runs past the timeout | transient (the host stops it) |
//! | cannot be started (missing, not executable) | permanent: it fails identically forever |
//! | `program` missing, or the plugin was not granted `process` | permanent |
//!
//! A line of the program's standard error (the first one, at most 160 characters) is part of
//! a transient reason, so a failing script says why in the dead-letter list.

use tuliprox_plugin_sdk::{prelude::*, process::Command};

/// The host gives a plugin call 30 s; stay clear of it.
const DEFAULT_TIMEOUT_SECS: u32 = 20;
const MAX_TIMEOUT_SECS: u32 = 25;
const REASON_LIMIT: usize = 160;

/// The program's standard error as part of a reason: its first line, one line, cut short.
fn stderr_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line: String = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .chars()
        .filter(|ch| !ch.is_control())
        .take(REASON_LIMIT)
        .collect();
    (!line.is_empty()).then_some(line)
}

/// How the host's own refusal ends the attempt.
fn host_refusal(reason: &str) -> DeliveryError {
    let reason: String = reason.chars().filter(|ch| !ch.is_control()).take(REASON_LIMIT).collect();
    // Still running after the timeout: the next attempt may well finish. Everything else
    // (no capability, not startable, malformed request) fails the same way every time.
    if reason.contains("still running") {
        DeliveryError::Transient("the program did not finish in time and was stopped".to_string())
    } else {
        DeliveryError::Permanent(reason)
    }
}

/// The environment variable value of `text`: no NUL, which a process environment cannot hold.
fn env_value(text: &str) -> String { text.replace('\0', "") }

/// The event's own severity word, which the plugin otherwise would see as `error` for a critical event.
fn event_severity(msg: &Notification) -> String {
    serde_json::from_str::<serde_json::Value>(&msg.payload)
        .ok()
        .and_then(|event| event.get("severity").and_then(|severity| severity.as_str().map(str::to_string)))
        .unwrap_or_else(|| {
            match msg.severity {
                Severity::Info => "info",
                Severity::Warn => "warn",
                Severity::Error => "error",
            }
            .to_string()
        })
}

fn event_id(msg: &Notification) -> String {
    serde_json::from_str::<serde_json::Value>(&msg.payload)
        .ok()
        .and_then(|event| event.get("id").and_then(|id| id.as_str().map(str::to_string)))
        .unwrap_or_default()
}

/// Run `program` for `msg` and say how the attempt ended.
pub fn deliver(
    program: &str,
    args: &[String],
    timeout_secs: Option<u32>,
    msg: &Notification,
) -> Result<(), DeliveryError> {
    let program = program.trim();
    if program.is_empty() {
        return Err(DeliveryError::Permanent("command-notifier is not configured: `program` is required".to_string()));
    }
    let output = Command::new(program)
        .args(args.iter().cloned())
        .env("TULIPROX_EVENT_ID", env_value(&event_id(msg)))
        .env("TULIPROX_EVENT_SEVERITY", env_value(&event_severity(msg)))
        .env("TULIPROX_EVENT_TITLE", env_value(&msg.title))
        .stdin(msg.payload.clone().into_bytes())
        .timeout_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, MAX_TIMEOUT_SECS))
        .run();
    let outcome = match output {
        Ok(output) if output.success() => Ok(()),
        Ok(output) => {
            let how = output.status.map_or_else(
                || "the program was ended by a signal".to_string(),
                |status| format!("the program exited with status {status}"),
            );
            Err(DeliveryError::Transient(match stderr_line(&output.stderr) {
                Some(line) => format!("{how}: {line}"),
                None => how,
            }))
        }
        Err(reason) => Err(host_refusal(&reason)),
    };
    match &outcome {
        Ok(()) => debug!("notification handled by the program"),
        Err(DeliveryError::Permanent(reason)) => warn!("the program was not run: {reason}"),
        Err(DeliveryError::Transient(reason)) => warn!("the program will be retried: {reason}"),
        Err(DeliveryError::RetryAfter(secs)) => warn!("the program asked to retry in {secs}s"),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::{mock, process::Output};

    const PROGRAM: &str = "/config/notify.sh";

    fn note() -> Notification {
        Notification {
            title: "Disk full".to_string(),
            body: "Disk is 96% full".to_string(),
            severity: Severity::Error,
            payload: r#"{"id":"system.disk.alert","severity":"critical","title":"Disk full"}"#.to_string(),
        }
    }

    fn exits(status: Option<i32>, stderr: &str) {
        mock::process_respond(PROGRAM, Ok(Output { status, stdout: Vec::new(), stderr: stderr.as_bytes().to_vec() }));
    }

    #[test]
    fn the_program_gets_the_event_on_stdin_and_in_its_environment() {
        mock::reset();
        exits(Some(0), "");
        let args = vec!["--from-tuliprox".to_string(), "a b".to_string()];
        assert!(deliver(PROGRAM, &args, None, &note()).is_ok());
        let runs = mock::process_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].program, PROGRAM);
        assert_eq!(runs[0].args, ["--from-tuliprox", "a b"]);
        assert_eq!(runs[0].stdin, note().payload.into_bytes());
        assert_eq!(runs[0].timeout_secs, DEFAULT_TIMEOUT_SECS);
        let env: std::collections::BTreeMap<_, _> = runs[0].env.iter().cloned().collect();
        assert_eq!(env["TULIPROX_EVENT_ID"], "system.disk.alert");
        assert_eq!(env["TULIPROX_EVENT_SEVERITY"], "critical", "the event's own severity, not the plugin's `error`");
        assert_eq!(env["TULIPROX_EVENT_TITLE"], "Disk full");
    }

    #[test]
    fn without_an_event_json_the_severity_comes_from_the_notification() {
        mock::reset();
        exits(Some(0), "");
        let mut msg = note();
        msg.payload = "not json".to_string();
        assert!(deliver(PROGRAM, &[], None, &msg).is_ok());
        let env: std::collections::BTreeMap<_, _> = mock::process_runs()[0].env.iter().cloned().collect();
        assert_eq!(env["TULIPROX_EVENT_SEVERITY"], "error");
        assert_eq!(env["TULIPROX_EVENT_ID"], "");
    }

    #[test]
    fn a_title_with_a_nul_byte_still_runs() {
        mock::reset();
        exits(Some(0), "");
        let mut msg = note();
        msg.title = "a\0b".to_string();
        assert!(deliver(PROGRAM, &[], None, &msg).is_ok());
        let env: std::collections::BTreeMap<_, _> = mock::process_runs()[0].env.iter().cloned().collect();
        assert_eq!(env["TULIPROX_EVENT_TITLE"], "ab");
    }

    #[test]
    fn the_timeout_is_kept_inside_the_hosts_call_limit() {
        mock::reset();
        for (asked, ran) in [(Some(0), 1), (Some(5), 5), (Some(300), MAX_TIMEOUT_SECS), (None, DEFAULT_TIMEOUT_SECS)] {
            exits(Some(0), "");
            assert!(deliver(PROGRAM, &[], asked, &note()).is_ok());
            assert_eq!(mock::process_runs().last().unwrap().timeout_secs, ran, "{asked:?}");
        }
    }

    #[test]
    fn a_failing_program_is_retried_and_says_why() {
        mock::reset();
        exits(Some(3), "\n  could not reach the mail server\nsecond line\n");
        match deliver(PROGRAM, &[], None, &note()) {
            Err(DeliveryError::Transient(reason)) => {
                assert_eq!(reason, "the program exited with status 3: could not reach the mail server");
            }
            other => panic!("{other:?}"),
        }
        exits(None, "");
        match deliver(PROGRAM, &[], None, &note()) {
            Err(DeliveryError::Transient(reason)) => assert_eq!(reason, "the program was ended by a signal"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_reason_is_one_short_line() {
        mock::reset();
        exits(Some(1), &format!("{}\x1b[31m", "x".repeat(500)));
        match deliver(PROGRAM, &[], None, &note()) {
            Err(DeliveryError::Transient(reason)) => {
                assert!(reason.chars().all(|ch| !ch.is_control()), "{reason:?}");
                assert!(reason.chars().count() < 60 + REASON_LIMIT, "{}", reason.chars().count());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_program_that_cannot_start_is_permanent_and_one_that_runs_too_long_is_retried() {
        mock::reset();
        mock::process_respond(PROGRAM, Err("could not run the program: No such file or directory (os error 2)"));
        match deliver(PROGRAM, &[], None, &note()) {
            Err(DeliveryError::Permanent(reason)) => assert!(reason.contains("No such file"), "{reason}"),
            other => panic!("{other:?}"),
        }
        mock::process_respond(PROGRAM, Err("the program was still running after 20 s and was stopped"));
        assert!(matches!(deliver(PROGRAM, &[], None, &note()), Err(DeliveryError::Transient(_))));
        mock::process_respond(PROGRAM, Err("run-process needs the process capability"));
        assert!(matches!(deliver(PROGRAM, &[], None, &note()), Err(DeliveryError::Permanent(_))));
    }

    #[test]
    fn a_missing_program_setting_is_permanent_and_runs_nothing() {
        mock::reset();
        for program in ["", "   "] {
            assert!(matches!(deliver(program, &[], None, &note()), Err(DeliveryError::Permanent(_))));
        }
        assert!(mock::process_runs().is_empty());
    }
}
