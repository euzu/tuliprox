//! `rpc.call` for manifest-declared `ui.rpc_exports`: input-schema
//! validation, size caps, a per-plugin rate limit and a dedicated instance per
//! call.
//!
//! Everything the host decides before a guest is touched is a pure function
//! ([`require_enabled`], [`export_named`], [`admit`]), so the status codes are
//! tested without a component. The order of the checks is: plugin known (`404`),
//! plugin `enabled` (`409`), method in `ui.rpc_exports` (`404`), a `write`
//! method needs the caller's `SystemWrite` (`403`), payload size (`413`), the
//! per-plugin token bucket (`429`), JSON (`400`), `input_schema` (`422`).
//!
//! A call never reaches the plugin's circuit breaker: it is counted in
//! [`RpcGate`] only, so a UI cannot disable a plugin by clicking.

use crate::{
    dispatch::HookDispatcher,
    host_fns,
    manifest::{RpcExport, RpcKind},
    registry::{LoadedPlugin, PluginState, RegistrySnapshot},
    runtime::{call_with_deadline, CallError},
    settings::{redact, unsupported_constructs, validate},
    system::PluginSystem,
    ui_assets::{read_package_json, sanitize_text},
};
use http::StatusCode;
use serde::de::IgnoredAny;
use serde_json::Value;
use shared::model::PluginFieldErrorDto;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use tuliprox_core::utils::request::{KeyedRateLimiter, RateLimitConfig, RateLimitRejection};

/// The largest request payload, in bytes (`413` above).
const MAX_RPC_PAYLOAD_BYTES: usize = 64 * 1024;

/// The largest answer, in bytes (`502` above).
const MAX_RPC_RESULT_BYTES: usize = 256 * 1024;

/// Per plugin: 10 calls per second, a burst of 20.
const RATE_PER_SECOND: u32 = 10;
const RATE_BURST: u32 = 20;

/// Plugin ids come from the registry's directory scan; this is far above any
/// real count and only bounds the limiter.
const MAX_RATE_KEYS: usize = 1024;

/// Longest guest `err(msg)` handed to the caller, in chars.
const MAX_GUEST_ERROR_CHARS: usize = 1024;

/// A refused or failed call: the HTTP status and the `PluginRpcErrorDto` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcFailure {
    pub status: StatusCode,
    pub error: String,
    /// Schema violations of the payload (`422`).
    pub fields: Vec<PluginFieldErrorDto>,
    /// Set on `429`: when a token is available again.
    pub retry_after: Option<Duration>,
}

fn failure(status: StatusCode, error: impl Into<String>) -> RpcFailure {
    RpcFailure { status, error: error.into(), fields: Vec::new(), retry_after: None }
}

/// What the rpc route counted for one plugin, apart from the circuit breaker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RpcCounts {
    /// Calls that ran to an outcome.
    pub(crate) calls: u64,
    /// Of those, the ones that trapped, timed out, returned `err` or answered badly.
    pub(crate) failures: u64,
    /// Calls the host could not start for lack of memory budget or an instance slot.
    pub(crate) budget_skips: u64,
    /// Calls the rate limit refused.
    pub(crate) rate_limited: u64,
    pub(crate) last_error: Option<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> { mutex.lock().unwrap_or_else(PoisonError::into_inner) }

/// The token buckets and the counters of the rpc route, one set per `Runtime`.
pub(crate) struct RpcGate {
    limiter: KeyedRateLimiter,
    /// Serializes the first registration of a plugin's bucket; registering again
    /// would reset it to a full bucket.
    registering: Mutex<()>,
    counts: Mutex<HashMap<String, RpcCounts>>,
}

impl Default for RpcGate {
    fn default() -> Self {
        Self {
            limiter: KeyedRateLimiter::new(MAX_RATE_KEYS),
            registering: Mutex::new(()),
            counts: Mutex::new(HashMap::new()),
        }
    }
}

impl RpcGate {
    fn try_take(&self, id: &str) -> Result<(), RateLimitRejection> { self.limiter.try_acquire(id) }

    /// Take one token of `id`'s bucket, creating the bucket on first use.
    pub(crate) fn take(&self, id: &str) -> Result<(), RpcFailure> {
        let mut outcome = self.try_take(id);
        if matches!(outcome, Err(RateLimitRejection::UnknownKey)) {
            let _guard = lock(&self.registering);
            outcome = self.try_take(id);
            if matches!(outcome, Err(RateLimitRejection::UnknownKey)) {
                if self.limiter.register(id, RateLimitConfig::per_second(RATE_PER_SECOND, RATE_BURST)).is_err() {
                    return Err(failure(StatusCode::SERVICE_UNAVAILABLE, "the rpc rate limiter is full"));
                }
                outcome = self.try_take(id);
            }
        }
        outcome.map_err(|rejection| {
            self.update(id, |counts| counts.rate_limited += 1);
            let retry_after = match rejection {
                RateLimitRejection::Exhausted { retry_after } => Some(retry_after.as_duration()),
                _ => None,
            };
            RpcFailure {
                status: StatusCode::TOO_MANY_REQUESTS,
                error: "too many calls to this plugin; slow down".to_string(),
                fields: Vec::new(),
                retry_after,
            }
        })
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut RpcCounts)) {
        change(lock(&self.counts).entry(id.to_string()).or_default());
    }

    /// Count one call that ran to an outcome.
    pub(crate) fn record(&self, id: &str, error: Option<&str>) {
        self.update(id, |counts| {
            counts.calls += 1;
            if let Some(error) = error {
                counts.failures += 1;
                counts.last_error = Some(error.to_string());
            }
        });
    }

    /// Count one call the host could not start for lack of budget.
    pub(crate) fn record_budget_skip(&self, id: &str) { self.update(id, |counts| counts.budget_skips += 1); }

    pub(crate) fn counts(&self, id: &str) -> RpcCounts { lock(&self.counts).get(id).cloned().unwrap_or_default() }
}

/// `409` unless the plugin is `enabled`. A plugin whose breaker tripped reads as
/// `failed` and is refused too, as is every other state.
fn require_enabled(state: PluginState) -> Result<(), RpcFailure> {
    if state == PluginState::Enabled {
        Ok(())
    } else {
        Err(failure(StatusCode::CONFLICT, format!("the plugin is {}, not enabled", state.as_str())))
    }
}

/// `404` unless `method` is one of the manifest's `ui.rpc_exports`.
fn export_named<'a>(exports: &'a [RpcExport], method: &str) -> Result<&'a RpcExport, RpcFailure> {
    exports
        .iter()
        .find(|export| export.name == method)
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "the plugin exports no such rpc method"))
}

/// The input schema of `export`, read from the package; `None` when it declares
/// none. A schema that cannot be read or that uses constructs the validator
/// cannot honour refuses the call: skipping a constraint would let any payload in.
fn input_schema(dir: &Path, export: &RpcExport) -> Result<Option<Value>, RpcFailure> {
    let Some(relative) = export.input_schema.as_deref() else {
        return Ok(None);
    };
    let schema = read_package_json(dir, relative)
        .ok_or_else(|| failure(StatusCode::BAD_GATEWAY, "the input schema of this method cannot be read"))?;
    if unsupported_constructs(&schema).is_empty() {
        Ok(Some(schema))
    } else {
        Err(failure(StatusCode::BAD_GATEWAY, "the input schema of this method uses constructs the host cannot check"))
    }
}

/// Everything between the route and the guest. Returns the payload the guest
/// gets: the validated JSON, re-serialized compactly, so what the schema saw is
/// exactly what the plugin reads.
fn admit(
    gate: &RpcGate,
    id: &str,
    export: &RpcExport,
    may_write: impl FnOnce() -> bool,
    body: &[u8],
    schema: impl FnOnce() -> Result<Option<Value>, RpcFailure>,
) -> Result<String, RpcFailure> {
    if export.kind == RpcKind::Write && !may_write() {
        return Err(failure(StatusCode::FORBIDDEN, "this method changes data and needs the system write permission"));
    }
    if body.len() > MAX_RPC_PAYLOAD_BYTES {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("the payload exceeds {} KiB", MAX_RPC_PAYLOAD_BYTES / 1024),
        ));
    }
    gate.take(id)?;
    let payload: Value =
        serde_json::from_slice(body).map_err(|_| failure(StatusCode::BAD_REQUEST, "the payload is not valid JSON"))?;
    if let Some(schema) = schema()? {
        let fields = validate(&schema, &payload);
        if !fields.is_empty() {
            return Err(RpcFailure {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                error: "the payload does not match the input schema".to_string(),
                fields,
                retry_after: None,
            });
        }
    }
    Ok(payload.to_string())
}

/// Replace every secret in the strings of `value`. Redacting the JSON text would
/// corrupt it whenever a secret is a substring of a key or an escape.
fn redact_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            if secrets.iter().any(|secret| !secret.is_empty() && text.contains(secret.as_str())) {
                *text = redact(text, secrets);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redact_value(item, secrets)),
        Value::Object(map) => map.values_mut().for_each(|item| redact_value(item, secrets)),
        _ => {}
    }
}

/// Check the answer of a guest: at most 256 KiB, valid JSON, and no resolved
/// secret in any string (never sent to the UI).
fn vet_result(text: String, secrets: &[String]) -> Result<String, RpcFailure> {
    if text.len() > MAX_RPC_RESULT_BYTES {
        return Err(failure(
            StatusCode::BAD_GATEWAY,
            format!("the plugin's answer exceeds {} KiB", MAX_RPC_RESULT_BYTES / 1024),
        ));
    }
    if secrets.iter().all(String::is_empty) {
        return serde_json::from_str::<IgnoredAny>(&text)
            .map(|_| text)
            .map_err(|_| failure(StatusCode::BAD_GATEWAY, "the plugin's answer is not valid JSON"));
    }
    let mut value: Value = serde_json::from_str(&text)
        .map_err(|_| failure(StatusCode::BAD_GATEWAY, "the plugin's answer is not valid JSON"))?;
    redact_value(&mut value, secrets);
    Ok(value.to_string())
}

/// What a failed guest call looks like to the caller. The reason of a trap stays
/// in the log: it can carry guest backtraces.
fn call_failure(err: &CallError) -> RpcFailure {
    match err {
        CallError::Budget { .. } => {
            failure(StatusCode::SERVICE_UNAVAILABLE, "no plugin memory or instance slot is free; try again")
        }
        CallError::Deadline => failure(StatusCode::BAD_GATEWAY, "the plugin did not answer in time"),
        CallError::OutOfMemory => failure(StatusCode::BAD_GATEWAY, "the plugin ran out of memory"),
        CallError::Trap(_) | CallError::Instantiate(_) => failure(StatusCode::BAD_GATEWAY, "the plugin failed"),
    }
}

impl PluginSystem {
    /// The largest rpc request body, in bytes; the route's body limit.
    pub const MAX_RPC_PAYLOAD_BYTES: usize = MAX_RPC_PAYLOAD_BYTES;

    /// Call `method` of plugin `id` with the request `body` at `POST
    /// /api/v1/plugins/{id}/rpc/{method}`. `may_write` is asked only for a
    /// `kind: write` method, and answers whether the caller has `SystemWrite`.
    ///
    /// On success the JSON text the plugin returned (at most 256 KiB, secrets
    /// redacted). Every caller (the route, MCP tools) gets the same limits.
    ///
    /// # Errors
    /// [`RpcFailure`] with the fixed status: `404` (unknown plugin or method),
    /// `409` (not enabled), `403`, `413`, `429`, `400`, `422` (also a guest
    /// `err(msg)`), `502` and `503`.
    pub async fn api_rpc_call(
        &self,
        id: &str,
        method: &str,
        body: &[u8],
        may_write: impl FnOnce() -> bool,
    ) -> Result<String, RpcFailure> {
        let snapshot = self.registry().snapshot();
        let record = snapshot.get(id).ok_or_else(|| failure(StatusCode::NOT_FOUND, "unknown plugin"))?;
        require_enabled(record.state())?;
        let manifest = record
            .manifest
            .as_deref()
            .ok_or_else(|| failure(StatusCode::CONFLICT, "the plugin has no readable manifest"))?;
        let export = export_named(&manifest.ui.rpc_exports, method)?;
        let gate = self.runtime().rpc();
        let payload = admit(gate, id, export, may_write, body, || input_schema(&record.dir, export))?;

        let loaded = record
            .loaded
            .as_ref()
            .filter(|loaded| loaded.rpc.is_some())
            .ok_or_else(|| failure(StatusCode::BAD_GATEWAY, "the plugin does not export rpc"))?;
        let outcome = self.call_guest(&snapshot, loaded, method, &payload).await;
        let secrets = &loaded.env.secrets;
        match outcome {
            Ok(Ok(text)) => match vet_result(text, secrets) {
                Ok(text) => {
                    gate.record(id, None);
                    Ok(text)
                }
                Err(failure) => {
                    gate.record(id, Some(&failure.error));
                    Err(failure)
                }
            },
            Ok(Err(message)) => {
                let message = sanitize_text(&redact(&message, secrets), MAX_GUEST_ERROR_CHARS);
                gate.record(id, Some(&message));
                Err(failure(StatusCode::UNPROCESSABLE_ENTITY, message))
            }
            Err(err) => {
                let reason = redact(&err.to_string(), secrets);
                if matches!(err, CallError::Budget { .. }) {
                    gate.record_budget_skip(id);
                } else {
                    gate.record(id, Some(&reason));
                }
                log::warn!("[plugin:{id}] rpc `{method}` failed: {reason}");
                Err(call_failure(&err))
            }
        }
    }

    /// One guest call in a fresh instance from the memory budget, `init`
    /// included, exactly as a hook call opens one (`HookDispatcher::open`).
    async fn call_guest(
        &self,
        snapshot: &Arc<RegistrySnapshot>,
        loaded: &LoadedPlugin,
        method: &str,
        payload: &str,
    ) -> Result<Result<String, String>, CallError> {
        let indices = loaded.rpc.as_ref().ok_or_else(|| CallError::Instantiate("does not export rpc".to_string()))?;
        let dispatcher = HookDispatcher::new(self.runtime(), Arc::clone(snapshot));
        let mut live = dispatcher.open(loaded).await?;
        let instance = live.instance();
        let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
        let limits = live.limits();
        let result = call_with_deadline(limits, guest.call_call(live.armed(), method, payload)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        result
    }

    /// The rpc counters of plugin `id`, which the management stats add to the
    /// hook counters (the circuit breaker never sees them).
    pub(crate) fn rpc_counts(&self, id: &str) -> RpcCounts { self.runtime().rpc().counts(id) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use shared::model::REDACTED_SECRET;

    fn export(kind: RpcKind, schema: Option<&str>) -> RpcExport {
        RpcExport {
            name: "get-fixtures".to_string(),
            kind,
            mcp: false,
            description: None,
            input_schema: schema.map(ToString::to_string),
        }
    }

    #[allow(clippy::unnecessary_wraps, reason = "the shape of `admit`'s schema closure")]
    fn no_schema() -> Result<Option<Value>, RpcFailure> { Ok(None) }

    fn status(result: &Result<String, RpcFailure>) -> StatusCode {
        result.as_ref().err().map_or(StatusCode::OK, |failure| failure.status)
    }

    fn call(
        gate: &RpcGate,
        export: &RpcExport,
        may_write: bool,
        body: &[u8],
        schema: &Value,
    ) -> Result<String, RpcFailure> {
        admit(gate, "demo", export, || may_write, body, || Ok(Some(schema.clone())))
    }

    #[test]
    fn only_an_enabled_plugin_is_called() {
        assert!(require_enabled(PluginState::Enabled).is_ok());
        for state in [
            PluginState::Discovered,
            PluginState::Invalid,
            PluginState::PendingApproval,
            PluginState::Approved,
            PluginState::Loaded,
            PluginState::Disabled,
            PluginState::Failed,
        ] {
            let refused = require_enabled(state).expect_err(state.as_str());
            assert_eq!(refused.status, StatusCode::CONFLICT, "{}", state.as_str());
        }
    }

    #[test]
    fn an_unknown_method_is_a_404() {
        let exports = [export(RpcKind::Read, None)];
        assert!(export_named(&exports, "get-fixtures").is_ok());
        for method in ["", "get-fixture", "GET-FIXTURES", "get-fixtures ", "../get-fixtures"] {
            assert_eq!(export_named(&exports, method).unwrap_err().status, StatusCode::NOT_FOUND, "{method:?}");
        }
        assert_eq!(export_named(&[], "get-fixtures").unwrap_err().status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_write_method_without_the_permission_is_a_403_and_a_read_is_not_asked() {
        let gate = RpcGate::default();
        let write = export(RpcKind::Write, None);
        let denied = admit(&gate, "demo", &write, || false, b"{}", no_schema);
        assert_eq!(status(&denied), StatusCode::FORBIDDEN);
        assert!(admit(&gate, "demo", &write, || true, b"{}", no_schema).is_ok());

        let read = export(RpcKind::Read, None);
        let asked = std::cell::Cell::new(false);
        let allowed = admit(
            &gate,
            "demo",
            &read,
            || {
                asked.set(true);
                false
            },
            b"{}",
            no_schema,
        );
        assert!(allowed.is_ok());
        assert!(!asked.get(), "a read method never asks for SystemWrite");
    }

    #[test]
    fn a_forbidden_call_does_not_spend_a_token() {
        let gate = RpcGate::default();
        let write = export(RpcKind::Write, None);
        for _ in 0..(RATE_BURST * 3) {
            assert_eq!(status(&admit(&gate, "demo", &write, || false, b"{}", no_schema)), StatusCode::FORBIDDEN);
        }
        assert!(admit(&gate, "demo", &write, || true, b"{}", no_schema).is_ok());
    }

    #[test]
    fn an_oversized_payload_is_a_413() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        let exact = format!("\"{}\"", "a".repeat(MAX_RPC_PAYLOAD_BYTES - 2));
        assert_eq!(exact.len(), MAX_RPC_PAYLOAD_BYTES);
        assert!(admit(&gate, "demo", &read, || true, exact.as_bytes(), no_schema).is_ok());
        let over = format!("\"{}\"", "a".repeat(MAX_RPC_PAYLOAD_BYTES - 1));
        assert_eq!(
            status(&admit(&gate, "demo", &read, || true, over.as_bytes(), no_schema)),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn a_payload_that_is_not_json_is_a_400() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        for body in [&b""[..], b"{", b"not json", b"{\"a\":1} trailing", b"\xff\xfe", b"[1,2"] {
            assert_eq!(
                status(&admit(&gate, "demo", &read, || true, body, no_schema)),
                StatusCode::BAD_REQUEST,
                "{body:?}"
            );
        }
        assert_eq!(admit(&gate, "demo", &read, || true, b"null", no_schema).unwrap(), "null");
    }

    #[test]
    fn a_schema_violation_is_a_422_with_every_field() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, Some("rpc/in.json"));
        let schema = json!({
            "type": "object",
            "required": ["league"],
            "additionalProperties": false,
            "properties": { "league": {"type": "string"}, "limit": {"type": "integer", "maximum": 50} }
        });
        let refused = call(&gate, &read, true, br#"{"limit": 99, "extra": 1}"#, &schema).unwrap_err();
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        let mut paths: Vec<_> = refused.fields.iter().map(|field| field.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["extra", "league", "limit"]);
        assert!(refused.fields.iter().all(|field| !field.message.contains("99")), "messages never echo a value");
        assert_eq!(
            call(&gate, &read, true, br#"{"league": "epl", "limit": 5}"#, &schema).unwrap(),
            r#"{"league":"epl","limit":5}"#
        );
    }

    #[test]
    fn the_guest_gets_the_compact_json_the_schema_saw() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        let sent = admit(&gate, "demo", &read, || true, b"  {\n \"a\" : 1 ,\"a\": 2 }  ", no_schema).unwrap();
        assert_eq!(sent, r#"{"a":2}"#);
    }

    #[test]
    fn a_schema_that_cannot_be_used_refuses_the_call() {
        let read = export(RpcKind::Read, Some("rpc/in.json"));
        let unsupported = json!({"oneOf": [{"type": "string"}]});
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("rpc")).unwrap();
        std::fs::write(dir.path().join("rpc/in.json"), unsupported.to_string()).unwrap();
        assert_eq!(input_schema(dir.path(), &read).unwrap_err().status, StatusCode::BAD_GATEWAY);
        let missing = export(RpcKind::Read, Some("rpc/missing.json"));
        assert_eq!(input_schema(dir.path(), &missing).unwrap_err().status, StatusCode::BAD_GATEWAY);
        assert_eq!(input_schema(dir.path(), &export(RpcKind::Read, None)).unwrap(), None);
    }

    #[test]
    fn the_twenty_first_call_in_a_burst_is_a_429_with_a_retry_hint() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        for call in 0..RATE_BURST {
            assert!(admit(&gate, "demo", &read, || true, b"{}", no_schema).is_ok(), "call {call}");
        }
        let limited = admit(&gate, "demo", &read, || true, b"{}", no_schema).unwrap_err();
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.retry_after.is_some());
        assert_eq!(gate.counts("demo").rate_limited, 1);
    }

    #[test]
    fn each_plugin_has_its_own_bucket() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        for _ in 0..RATE_BURST {
            admit(&gate, "one", &read, || true, b"{}", no_schema).unwrap();
        }
        assert_eq!(status(&admit(&gate, "one", &read, || true, b"{}", no_schema)), StatusCode::TOO_MANY_REQUESTS);
        assert!(admit(&gate, "two", &read, || true, b"{}", no_schema).is_ok());
    }

    #[test]
    fn counters_track_calls_failures_and_budget_skips_apart() {
        let gate = RpcGate::default();
        gate.record("demo", None);
        gate.record("demo", Some("boom"));
        gate.record_budget_skip("demo");
        assert_eq!(
            gate.counts("demo"),
            RpcCounts { calls: 2, failures: 1, budget_skips: 1, rate_limited: 0, last_error: Some("boom".to_string()) }
        );
        assert_eq!(gate.counts("other"), RpcCounts::default());
    }

    #[test]
    fn an_answer_is_capped_checked_and_scrubbed_of_secrets() {
        let secrets = vec!["s3cr3t".to_string()];
        assert_eq!(vet_result(r#"{"a": [1, "x"]}"#.to_string(), &[]).unwrap(), r#"{"a": [1, "x"]}"#);
        assert_eq!(vet_result("not json".to_string(), &[]).unwrap_err().status, StatusCode::BAD_GATEWAY);
        assert_eq!(vet_result("not json".to_string(), &secrets).unwrap_err().status, StatusCode::BAD_GATEWAY);
        let big = format!("\"{}\"", "a".repeat(MAX_RPC_RESULT_BYTES));
        assert_eq!(vet_result(big, &[]).unwrap_err().status, StatusCode::BAD_GATEWAY);
        let scrubbed = vet_result(json!({"k": ["token s3cr3t!", 3], "s3cr3t": true}).to_string(), &secrets).unwrap();
        let value: Value = serde_json::from_str(&scrubbed).unwrap();
        assert_eq!(value["k"][0], format!("token {REDACTED_SECRET}!"));
        assert_eq!(value["k"][1], 3);
        assert!(!scrubbed.contains("token s3cr3t"));
    }

    #[test]
    fn call_failures_map_to_502_and_503() {
        let cases = [
            (CallError::Deadline, StatusCode::BAD_GATEWAY),
            (CallError::OutOfMemory, StatusCode::BAD_GATEWAY),
            (CallError::Trap("wasm backtrace at /home/x".to_string()), StatusCode::BAD_GATEWAY),
            (CallError::Instantiate("bad".to_string()), StatusCode::BAD_GATEWAY),
            (CallError::Budget { waited: Duration::from_millis(5) }, StatusCode::SERVICE_UNAVAILABLE),
        ];
        for (err, expected) in cases {
            let mapped = call_failure(&err);
            assert_eq!(mapped.status, expected, "{err}");
            assert!(!mapped.error.contains("/home/x"), "a trap's reason stays in the log");
        }
    }
}
