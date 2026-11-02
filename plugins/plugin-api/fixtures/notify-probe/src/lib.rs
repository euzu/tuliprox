//! Fixture: a notification channel, driven by the plugin's settings.
//!
//! The scenario is the `mode` setting the test puts in the plugin's config
//! entry (a `routing` object next to it is read by the host, not by this
//! guest):
//!
//! * `ok` (the default): log the title and severity, deliver.
//! * `transient`: answer `delivery-error::transient`, so the outbox retries.
//! * `trap`: the guest traps (`unreachable`), which the host maps to a retry.

wit_bindgen::generate!({
    path: ["../../wit", "wit"],
    world: "tuliprox:notify-probe/notify-probe",
    generate_all,
});

use exports::tuliprox::plugin::{
    lifecycle,
    notification_hooks::{self, DeliveryError, Notification},
};
use serde_json::Value;
use std::sync::Mutex;
use tuliprox::plugin::{host, status::Severity, types::LogLevel};

static MODE: Mutex<Option<String>> = Mutex::new(None);

struct NotifyProbe;

fn mode() -> String {
    MODE.lock().ok().and_then(|guard| guard.clone()).unwrap_or_else(|| "ok".to_string())
}

impl lifecycle::Guest for NotifyProbe {
    fn init(config: String) -> Result<(), String> {
        let parsed: Value = serde_json::from_str(&config).unwrap_or(Value::Null);
        let mode = parsed.get("mode").and_then(Value::as_str).unwrap_or("ok").to_string();
        if let Ok(mut guard) = MODE.lock() {
            *guard = Some(mode);
        }
        host::log(LogLevel::Debug, "notify-probe initialised");
        Ok(())
    }
}

impl notification_hooks::Guest for NotifyProbe {
    fn send_notification(msg: Notification) -> Result<(), DeliveryError> {
        let severity = match msg.severity {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        };
        host::log(LogLevel::Info, &format!("notify-probe: [{severity}] {}", msg.title));
        match mode().as_str() {
            "transient" => Err(DeliveryError::Transient("notify-probe: simulated outage".to_string())),
            "trap" => panic!("notify-probe: simulated trap"),
            _ => Ok(()),
        }
    }
}

export!(NotifyProbe);
