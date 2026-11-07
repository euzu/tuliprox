//! The `check-input` rpc behind the input form's "Check" button: it reads the form's unsaved
//! `plugin_config`, names what each target in `channels` is (or why it is not one) and asks every
//! Invidious instance whether it answers.
//!
//! The form only hands over `plugin_config`, so targets typed into the input's Url field are not
//! checked here. The call runs under the host's hook deadline (5 s by default), so instances are
//! probed with short timeouts and the ones the budget leaves no room for are reported as unchecked.

use crate::{
    discovery::{parse_target, TargetKind},
    BROWSER_UA,
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
                Ok(version) => {
                    reachable += 1;
                    lines.push(format!("  ✓ {instance}: {version}"));
                }
                Err(reason) => lines.push(format!("  ✗ {instance}: {reason}")),
            }
        }
        if reachable == 0 {
            ok = false;
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

/// Asks an instance for `/api/v1/stats`; the answer names its software version when it gives one.
fn probe_instance(instance: &str) -> Result<String, String> {
    let endpoint = format!("{}/api/v1/stats", instance.trim_end_matches('/'));
    let resp = http::Request::get(&endpoint)
        .header("User-Agent", BROWSER_UA)
        .timeout(PROBE_TIMEOUT)
        .send()
        .map_err(|e| format!("not reachable ({e})"))?;
    if !resp.is_success() {
        return Err(format!("answered HTTP {}", resp.status()));
    }
    let stats = resp.json::<Value>().map_err(|_| "answered, but not with Invidious API JSON".to_string())?;
    let software = stats.get("software");
    let name = software.and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or("invidious");
    Ok(match software.and_then(|s| s.get("version")).and_then(Value::as_str) {
        Some(version) => format!("reachable ({name} {version})"),
        None => "reachable".to_string(),
    })
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
            "https://inv.test/api/v1/stats",
            200,
            r#"{"software":{"name":"invidious","version":"2.2025"}}"#,
        );
        let (message, ok) = run(json!({
            "channels": ["@news", "UCaaaaaaaaaaaaaaaaaaaaaa", "@news/live"],
            "invidious_instances": ["https://inv.test/"],
        }));
        assert!(ok, "{message}");
        assert!(message.contains("channel handle @news"), "{message}");
        assert!(message.contains("live broadcast of channel handle @news"), "{message}");
        assert!(message.contains("reachable (invidious 2.2025)"), "{message}");
    }

    #[test]
    fn an_invalid_target_or_a_dead_instance_fails() {
        mock::reset();
        mock::set_now(1_000);
        mock::http_respond(Method::Get, "https://down.test/api/v1/stats", 503, "");
        let (message, ok) = run(json!({
            "channels": ["https://www.youtube.com/channel/nope"],
            "invidious_instances": ["https://down.test"],
        }));
        assert!(!ok, "{message}");
        assert!(message.contains("✗ https://www.youtube.com/channel/nope"), "{message}");
        assert!(message.contains("answered HTTP 503"), "{message}");
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
}
