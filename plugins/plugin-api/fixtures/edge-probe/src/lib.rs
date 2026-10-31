//! W6a fixture: the session-edge and dashboard-metrics hooks, driven by the
//! plugin's settings.
//!
//! * `inject-upstream-headers` answers `X-Edge-Probe: 1` and
//!   `X-Edge-User: <user>`. `fail` (a string) makes it return `err`.
//!   `delay_ms` busy-waits that many milliseconds of wall-clock time first, so
//!   a value of a few hundred runs far past the host's 50 ms clamp. `bad_header: true` also returns a `Host` header, to exercise the
//!   denylist.
//! * `collect-metrics` answers `answer=42` and `calls=<n>`, where `n` counts
//!   the calls so far in the plugin's KV store (one fresh instance per call, so
//!   a static would always read 1). `delay_ms` applies here too.

wit_bindgen::generate!({
    path: ["../../wit", "wit"],
    world: "tuliprox:edge-probe/edge-probe",
    generate_all,
});

use exports::tuliprox::plugin::{
    lifecycle,
    metrics_hooks::{self, Metric},
    stream_hooks::{self, UpstreamRequest},
};
use serde_json::{Map, Value};
use std::sync::Mutex;
use tuliprox::plugin::{host, status::Severity, types::LogLevel};

const CALLS_KEY: &str = "calls";

static SETTINGS: Mutex<Option<Map<String, Value>>> = Mutex::new(None);

struct EdgeProbe;

fn settings() -> Map<String, Value> {
    SETTINGS.lock().ok().and_then(|guard| guard.clone()).unwrap_or_default()
}

/// Busy-wait `delay_ms` of wall-clock time (the WASI monotonic clock). A
/// counted loop was not enough: the optimizer still shortened it, so the
/// "slow" scenario never reached the host's 50 ms clamp.
fn spin(settings: &Map<String, Value>) {
    let delay_ms = settings.get("delay_ms").and_then(Value::as_u64).unwrap_or(0);
    let deadline = std::time::Duration::from_millis(delay_ms);
    let start = std::time::Instant::now();
    let mut counter = 0_u64;
    while start.elapsed() < deadline {
        counter = std::hint::black_box(counter.wrapping_add(1));
    }
    std::hint::black_box(counter);
}

impl lifecycle::Guest for EdgeProbe {
    fn init(config: String) -> Result<(), String> {
        let parsed: Value = serde_json::from_str(&config).unwrap_or(Value::Null);
        if let Ok(mut guard) = SETTINGS.lock() {
            *guard = Some(parsed.as_object().cloned().unwrap_or_default());
        }
        host::log(LogLevel::Debug, "edge-probe initialised");
        Ok(())
    }
}

impl stream_hooks::Guest for EdgeProbe {
    fn inject_upstream_headers(req: UpstreamRequest) -> Result<Vec<(String, String)>, String> {
        let settings = settings();
        spin(&settings);
        if let Some(reason) = settings.get("fail").and_then(Value::as_str) {
            return Err(reason.to_string());
        }
        let mut headers =
            vec![("X-Edge-Probe".to_string(), "1".to_string()), ("X-Edge-User".to_string(), req.user.clone())];
        if settings.get("bad_header").and_then(Value::as_bool).unwrap_or(false) {
            headers.push(("Host".to_string(), "evil.example".to_string()));
        }
        Ok(headers)
    }
}

impl metrics_hooks::Guest for EdgeProbe {
    fn collect_metrics() -> Result<Vec<Metric>, String> {
        let settings = settings();
        spin(&settings);
        let previous = match host::kv_get(CALLS_KEY) {
            Ok(Some(bytes)) => String::from_utf8(bytes).ok().and_then(|text| text.parse::<u64>().ok()).unwrap_or(0),
            _ => 0,
        };
        let calls = previous + 1;
        host::kv_set(CALLS_KEY, calls.to_string().as_bytes()).map_err(|err| format!("kv-set failed: {err:?}"))?;
        Ok(vec![
            Metric {
                key: "answer".to_string(),
                label: "Answer".to_string(),
                value: "42".to_string(),
                unit: None,
                severity: Some(Severity::Info),
            },
            Metric {
                key: "calls".to_string(),
                label: "Calls".to_string(),
                value: calls.to_string(),
                unit: Some("calls".to_string()),
                severity: None,
            },
        ])
    }
}

export!(EdgeProbe);
