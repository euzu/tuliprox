//! The `check-input` rpc behind the input form's "Check" button: it reads the form's unsaved
//! `plugin_config`, names what each target in `channels` is (or why it is not one) and asks every
//! Invidious instance for a video, the request a fetch makes. An instance whose status page answers
//! can still refuse the video API; public instances usually do.
//!
//! The form only hands over `plugin_config`, so targets typed into the input's Url field are not
//! checked here. The call runs under the host's hook deadline (5 s by default), so instances are
//! probed with short timeouts and the ones the budget leaves no room for are reported as unchecked.

use crate::{
    discovery::{parse_target, TargetKind},
    resolver::describe_http_failure,
    ytdlp, BROWSER_UA,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tuliprox_plugin_sdk::{clock, http};

/// The timeout of one instance probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// No probe starts once this many seconds have passed, so the last one ends well inside the
/// default 5 s hook deadline.
const PROBE_BUDGET_SECS: i64 = 2;
/// A long-lived public video ("Me at the zoo") that every working instance can describe.
const PROBE_VIDEO_ID: &str = "jNQXAC9IVRw";

/// The payload a form action sends: `{form, section, action, config}`. Only `config` matters here.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ActionRequest {
    #[serde(default)]
    config: Value,
}

/// `check-input`: a `{message, ok}` answer the form shows under its buttons. `ok` is `false` when
/// a target is invalid, no instance is configured or none of them answered.
pub(crate) fn check_input(request: ActionRequest) -> Result<Value, String> {
    let channels = string_list(request.config.get("channels"));
    let instances = string_list(request.config.get("invidious_instances"));
    let resolver = request
        .config
        .get("resolver")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("invidious");
    let ytdlp_path = request.config.get("ytdlp_path").and_then(Value::as_str);

    let mut lines = Vec::new();
    let mut ok = true;

    if channels.is_empty() {
        lines.push("Channels: none listed (the input's Url still counts).".to_string());
    } else {
        lines.push(format!("Channels ({}):", channels.len()));
        for target in &channels {
            match describe(&parse_target(target)) {
                Ok(kind) => lines.push(format!("  ✓ {target}: {kind}")),
                Err(reason) => {
                    ok = false;
                    lines.push(format!("  ✗ {target}: {reason}"));
                }
            }
        }
    }

    match resolver {
        "yt-dlp" => match ytdlp::find_ytdlp(ytdlp_path) {
            Ok((path, version)) => {
                lines.push(format!("Resolver: yt-dlp (found: {path} (version {version}))"));
            }
            Err(tried) => {
                ok = false;
                lines.push(format!("Resolver: yt-dlp (not found (tried {}))", tried.join(", ")));
            }
        },
        "invidious" => {
            lines.push("Resolver: Invidious".to_string());
            if instances.is_empty() {
                ok = false;
                lines.push("Invidious instances: none listed; at least one is required.".to_string());
            } else {
                lines.push(format!("Invidious instances ({}):", instances.len()));
                let stop_at = clock::now_unix() + PROBE_BUDGET_SECS;
                let mut reachable = 0usize;
                for instance in &instances {
                    if clock::now_unix() >= stop_at {
                        lines.push(format!("  … {instance}: not checked, out of time"));
                        continue;
                    }
                    match probe_instance(instance) {
                        Ok(answer) => {
                            reachable += 1;
                            lines.push(format!("  ✓ {instance}: {answer}"));
                        }
                        Err(reason) => lines.push(format!("  ✗ {instance}: {reason}")),
                    }
                }
                if reachable == 0 {
                    ok = false;
                }
            }
        }
        other => {
            ok = false;
            lines.push(format!("Resolver: unknown '{other}', must be 'invidious' or 'yt-dlp'"));
        }
    }

    Ok(json!({ "message": lines.join("\n"), "ok": ok }))
}

/// A list field of the form: a JSON array of strings, or (hand-written config) a comma-separated string.
fn string_list(value: Option<&Value>) -> Vec<String> {
    let entries: Vec<String> = match value {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(text)) => match serde_json::from_str::<Vec<String>>(text) {
            Ok(list) => list,
            Err(_) => text.split(',').map(str::to_string).collect(),
        },
        _ => Vec::new(),
    };
    entries.into_iter().map(|entry| entry.trim().to_string()).filter(|entry| !entry.is_empty()).collect()
}

/// What a parsed target is, in words, or why it is not a target.
fn describe(kind: &TargetKind) -> Result<String, String> {
    match kind {
        TargetKind::ChannelId(id) => Ok(format!("channel {id}")),
        TargetKind::PlaylistId(id) => Ok(format!("playlist {id}")),
        TargetKind::Handle(handle) => Ok(format!("channel handle {handle}")),
        TargetKind::LegacyName(path) => Ok(format!("channel page /{path}")),
        TargetKind::VideoId(id) => Ok(format!("video {id}")),
        TargetKind::Live(inner) => describe(inner).map(|what| format!("live broadcast of {what}")),
        TargetKind::Invalid(reason) => Err(reason.clone()),
    }
}

/// Asks an instance for a video the way a fetch does. Only a video answer counts: the status page of an
/// instance answers even when the instance refuses the video API.
fn probe_instance(instance: &str) -> Result<String, String> {
    let endpoint = format!("{}/api/v1/videos/{PROBE_VIDEO_ID}?fields=videoId,error", instance.trim_end_matches('/'));
    let resp = http::Request::get(&endpoint)
        .header("User-Agent", BROWSER_UA)
        .header("Accept", "application/json")
        .timeout(PROBE_TIMEOUT)
        .send()
        .map_err(|e| format!("not reachable ({e})"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !resp.is_success() {
        return Err(describe_http_failure(status, &body));
    }
    let answer =
        serde_json::from_str::<Value>(&body).map_err(|_| "answered, but not with Invidious API JSON".to_string())?;
    if answer.get("videoId").and_then(Value::as_str).is_some() {
        return Ok("serves videos".to_string());
    }
    Err(describe_http_failure(403, &body))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::{http::Method, mock};

    fn run(config: Value) -> (String, bool) {
        let answer = check_input(ActionRequest { config }).expect("check answers");
        (answer["message"].as_str().unwrap_or_default().to_string(), answer["ok"].as_bool().unwrap_or_default())
    }

    #[test]
    fn valid_targets_and_a_reachable_instance_pass() {
        mock::reset();
        mock::set_now(1_000);
        mock::http_respond(
            Method::Get,
            "https://inv.test/api/v1/videos/jNQXAC9IVRw?fields=videoId,error",
            200,
            r#"{"videoId":"jNQXAC9IVRw"}"#,
        );
        let (message, ok) = run(json!({
            "channels": ["@news", "UCaaaaaaaaaaaaaaaaaaaaaa", "@news/live"],
            "invidious_instances": ["https://inv.test/"],
        }));
        assert!(ok, "{message}");
        assert!(message.contains("channel handle @news"), "{message}");
        assert!(message.contains("live broadcast of channel handle @news"), "{message}");
        assert!(message.contains("✓ https://inv.test/: serves videos"), "{message}");
    }

    #[test]
    fn an_invalid_target_or_a_dead_instance_fails() {
        mock::reset();
        mock::set_now(1_000);
        mock::http_respond(Method::Get, "https://down.test/api/v1/videos/jNQXAC9IVRw?fields=videoId,error", 503, "");
        let (message, ok) = run(json!({
            "channels": ["https://www.youtube.com/channel/nope"],
            "invidious_instances": ["https://down.test"],
        }));
        assert!(!ok, "{message}");
        assert!(message.contains("✗ https://www.youtube.com/channel/nope"), "{message}");
        assert!(message.contains("HTTP status 503"), "{message}");
    }

    #[test]
    fn an_instance_that_refuses_the_video_api_fails_the_check() {
        mock::reset();
        mock::set_now(1_000);
        mock::http_respond(
            Method::Get,
            "https://public.test/api/v1/videos/jNQXAC9IVRw?fields=videoId,error",
            200,
            r#"{"error":"access_denied"}"#,
        );
        let (message, ok) = run(json!({ "channels": ["@news"], "invidious_instances": ["https://public.test"] }));
        assert!(!ok, "{message}");
        assert!(message.contains("✗ https://public.test: refuses the video API"), "{message}");
    }

    #[test]
    fn no_instances_fails_without_a_request_and_a_string_list_is_read() {
        mock::reset();
        mock::set_now(1_000);
        let (message, ok) = run(json!({ "channels": "@a, @b" }));
        assert!(!ok, "{message}");
        assert!(message.contains("Channels (2):"), "{message}");
        assert!(message.contains("at least one is required"), "{message}");
        assert!(mock::http_requests().is_empty());
    }

    #[test]
    fn ytdlp_found_reports_version_and_skips_invidious_probing() {
        mock::reset();
        mock::set_now(1_000);
        mock::process_respond(
            "/app/bin/yt-dlp",
            Ok(tuliprox_plugin_sdk::process::Output {
                status: Some(0),
                stdout: b"2024.11.04\n".to_vec(),
                stderr: Vec::new(),
            }),
        );
        let (message, ok) = run(json!({
            "resolver": "yt-dlp",
            "channels": ["@news"],
        }));
        assert!(ok, "{message}");
        assert!(message.contains("Resolver: yt-dlp (found: /app/bin/yt-dlp (version 2024.11.04))"), "{message}");
        // Invidious is never probed and no instances are required
        assert!(mock::http_requests().is_empty());
    }

    #[test]
    fn ytdlp_not_found_reports_tried_paths_and_fails_check() {
        mock::reset();
        mock::set_now(1_000);
        mock::process_respond("/app/bin/yt-dlp", Err("no binary"));
        mock::process_respond("yt-dlp", Err("not on path"));

        let (message, ok) = run(json!({
            "resolver": "yt-dlp",
            "channels": ["@news"],
        }));
        assert!(!ok, "{message}");
        assert!(message.contains("Resolver: yt-dlp (not found (tried /app/bin/yt-dlp, yt-dlp))"), "{message}");
        assert!(mock::http_requests().is_empty());
    }
}
