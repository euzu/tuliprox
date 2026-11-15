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

/// Host caps for `x-options-rpc` results.
pub const MAX_OPTIONS_COUNT: usize = 1000;
pub const MAX_OPTION_STRING_CHARS: usize = 1024;

/// Validates the guest's answer for an `x-options-rpc` method: must be an array of
/// `{"value": string, "label": string}` (or `{"options": [...]}`), at most 1000 items,
/// and at most 1024 characters per string.
pub fn validate_options_result(json_str: &str) -> Result<(), String> {
    let value: Value =
        serde_json::from_str(json_str).map_err(|err| format!("the options result is not valid JSON: {err}"))?;
    let items = match &value {
        Value::Array(items) => items,
        Value::Object(map) if map.get("options").is_some_and(Value::is_array) => {
            map.get("options").and_then(Value::as_array).unwrap()
        }
        _ => return Err("the options result must be a JSON array of {value, label} objects".to_string()),
    };
    if items.len() > MAX_OPTIONS_COUNT {
        return Err(format!("the options result exceeds {} items (found {})", MAX_OPTIONS_COUNT, items.len()));
    }
    for (i, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(format!("options item {i} is not a JSON object"));
        };
        let Some(val) = obj.get("value").and_then(Value::as_str) else {
            return Err(format!("options item {i} is missing a string `value`"));
        };
        let Some(label) = obj.get("label").and_then(Value::as_str) else {
            return Err(format!("options item {i} is missing a string `label`"));
        };
        if val.chars().count() > MAX_OPTION_STRING_CHARS {
            return Err(format!("options item {i} `value` exceeds {MAX_OPTION_STRING_CHARS} characters"));
        }
        if label.chars().count() > MAX_OPTION_STRING_CHARS {
            return Err(format!("options item {i} `label` exceeds {MAX_OPTION_STRING_CHARS} characters"));
        }
    }
    Ok(())
}

/// Lowercase key fragments that mark an input option as a secret, the same set
/// the MCP result redactor uses.
const SECRET_OPTION_KEY_PARTS: [&str; 9] =
    ["password", "passwd", "secret", "token", "api_key", "apikey", "api-key", "authorization", "cookie"];
/// Exact lowercase option keys that hold a credential identity.
const CREDENTIAL_OPTION_KEYS: [&str; 2] = ["username", "user"];

pub(crate) fn option_is_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_OPTION_KEY_PARTS.iter().any(|part| key.contains(part)) || CREDENTIAL_OPTION_KEYS.contains(&key.as_str())
}

/// Collects every secret that must not appear in a result or log line:
/// password, username, secret-looking option values, and writeOnly form-secret values.
pub(crate) fn collect_call_secrets(
    username: Option<&str>,
    password: Option<&str>,
    options: &[(String, String)],
    form_secret_paths: &[String],
) -> Vec<String> {
    let form_secrets = if form_secret_paths.is_empty() {
        Vec::new()
    } else {
        let options_map: serde_json::Map<String, Value> =
            options.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        crate::settings::secret_values(&Value::Object(options_map), form_secret_paths)
    };
    collect_call_secrets_from_values(username, password, options, &form_secrets)
}

pub(crate) fn collect_call_secrets_from_values(
    username: Option<&str>,
    password: Option<&str>,
    options: &[(String, String)],
    form_secrets: &[String],
) -> Vec<String> {
    let mut secrets = Vec::new();
    let mut push = |secret: &str| {
        if !secret.is_empty() && !secrets.iter().any(|known| known == secret) {
            secrets.push(secret.to_string());
        }
    };
    if let Some(password) = password {
        push(password);
    }
    if let Some(username) = username {
        push(username);
    }
    for (key, value) in options {
        if option_is_secret(key) {
            push(value);
        }
    }
    for secret in form_secrets {
        push(secret);
    }
    secrets
}

/// A configured `type: plugin` input, resolved by the caller of an
/// `input_scoped` export and handed to the guest beside the arguments.
///
/// The host resolves it from the server's sources (the app layer builds this
/// from `ConfigInput`); the plugin host only checks that it belongs to the
/// plugin and builds the payload.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RpcInput {
    /// The plugin the input is configured for.
    pub plugin: String,
    /// The input's configured name, which is also what the caller names.
    pub name: String,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// The input's plugin options, in configuration order.
    pub options: Vec<(String, String)>,
    /// Resolved values of secret fields from the plugin's input form schema.
    pub form_secrets: Vec<String>,
}

impl RpcInput {
    /// Every value that must not appear in a result or a log line: the
    /// password, the username, and any option whose key looks secret.
    fn secrets(&self) -> Vec<String> {
        collect_call_secrets_from_values(
            self.username.as_deref(),
            self.password.as_deref(),
            &self.options,
            &self.form_secrets,
        )
    }

    /// The context object the guest reads: `{name, url, username, password, options}`.
    fn context(&self) -> Value {
        let options: serde_json::Map<String, Value> =
            self.options.iter().map(|(key, value)| (key.clone(), Value::String(value.clone()))).collect();
        serde_json::json!({
            "name": self.name,
            "url": self.url,
            "username": self.username,
            "password": self.password,
            "options": options,
        })
    }
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

/// What `admit` hands to the guest call.
#[derive(Debug)]
struct Admitted {
    /// The payload, re-serialized compactly, so what the schema saw is exactly
    /// what the plugin reads.
    payload: String,
    /// Values to scrub from the result and from every log line of this call.
    secrets: Vec<String>,
}

/// Split the payload of an `input_scoped` export into the arguments and the
/// resolved input. `input` names the configured input; everything else is what
/// the export's `input_schema` describes.
fn scoped_payload(id: &str, payload: Value, input: Option<&RpcInput>) -> Result<(Value, RpcInput), RpcFailure> {
    let Value::Object(mut arguments) = payload else {
        return Err(failure(StatusCode::BAD_REQUEST, "the payload must be a JSON object for an `input_scoped` export"));
    };
    let has_draft = arguments.remove("input_draft").is_some();
    let selector = match arguments.remove("input") {
        Some(Value::String(selector)) => selector,
        Some(_) => return Err(failure(StatusCode::BAD_REQUEST, "`input` must be the name of a configured input")),
        None if has_draft => input.map(|i| i.name.clone()).unwrap_or_default(),
        None => {
            return Err(failure(StatusCode::BAD_REQUEST, "the payload needs an `input` naming a configured input"));
        }
    };
    let Some(input) = input else {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            format!("no configured plugin input is named `{}`", sanitize_text(&selector, 64)),
        ));
    };
    if input.plugin != id {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            format!(
                "the input `{}` belongs to plugin `{}`, not `{id}`",
                sanitize_text(&input.name, 64),
                sanitize_text(&input.plugin, 64)
            ),
        ));
    }
    if !selector.is_empty() && input.name != selector {
        return Err(failure(StatusCode::BAD_REQUEST, "the resolved input does not match the requested `input`"));
    }
    Ok((Value::Object(arguments), input.clone()))
}

/// Everything between the route and the guest. Returns the payload the guest
/// gets: for an `input_scoped` export `{"args": …, "input": {…}}`, otherwise the
/// validated JSON as it is. The size cap applies to the payload the guest
/// actually reads, the wrapped one included.
fn admit(
    gate: &RpcGate,
    id: &str,
    export: &RpcExport,
    may_write: impl FnOnce() -> bool,
    body: &[u8],
    schema: impl FnOnce() -> Result<Option<Value>, RpcFailure>,
    input: Option<&RpcInput>,
) -> Result<Admitted, RpcFailure> {
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
    let (arguments, input) = if export.input_scoped {
        let (arguments, input) = scoped_payload(id, payload, input)?;
        (arguments, Some(input))
    } else {
        (payload, None)
    };
    if let Some(schema) = schema()? {
        let fields = validate(&schema, &arguments);
        if !fields.is_empty() {
            return Err(RpcFailure {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                error: "the payload does not match the input schema".to_string(),
                fields,
                retry_after: None,
            });
        }
    }
    let secrets = input.as_ref().map(RpcInput::secrets).unwrap_or_default();
    let payload = match &input {
        Some(input) => serde_json::json!({ "args": arguments, "input": input.context() }).to_string(),
        None => arguments.to_string(),
    };
    if payload.len() > MAX_RPC_PAYLOAD_BYTES {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("the payload with the input context exceeds {} KiB", MAX_RPC_PAYLOAD_BYTES / 1024),
        ));
    }
    Ok(Admitted { payload, secrets })
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
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (mut key, mut val) in entries {
                if secrets.iter().any(|secret| !secret.is_empty() && key.contains(secret.as_str())) {
                    key = redact(&key, secrets);
                }
                redact_value(&mut val, secrets);
                map.insert(key, val);
            }
        }
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
    /// This is [`PluginSystem::api_rpc_call_with_input`] without an input
    /// context: an `input_scoped` export refuses the call (`400`), everything
    /// else reads the body exactly as before.
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
        self.api_rpc_call_with_input(id, method, body, may_write, None).await
    }

    /// [`PluginSystem::api_rpc_call`] with the `type: plugin` input the caller
    /// resolved for an `input_scoped` export. The body names the input in its
    /// `input` member (the rest is the arguments); the guest reads
    /// `{"args": …, "input": {name, url, username, password, options}}`.
    ///
    /// The input's secrets never reach the caller: the result, a guest error and
    /// the log lines of this call are scrubbed of the password, the username and
    /// every secret-looking option value. `input` is ignored for an export that
    /// is not `input_scoped`, which keeps its payload exactly as before.
    ///
    /// # Errors
    /// The same as [`PluginSystem::api_rpc_call`], plus `400` when an
    /// `input_scoped` export is called without a usable input.
    pub async fn api_rpc_call_with_input(
        &self,
        id: &str,
        method: &str,
        body: &[u8],
        may_write: impl FnOnce() -> bool,
        input: Option<RpcInput>,
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
        let admitted = admit(gate, id, export, may_write, body, || input_schema(&record.dir, export), input.as_ref())?;

        let loaded = record
            .loaded
            .as_ref()
            .filter(|loaded| loaded.rpc.is_some())
            .ok_or_else(|| failure(StatusCode::BAD_GATEWAY, "the plugin does not export rpc"))?;
        let outcome = self.call_guest(&snapshot, loaded, method, &admitted.payload, &admitted.secrets).await;
        let secrets: Vec<String> = loaded.env.secrets.iter().chain(admitted.secrets.iter()).cloned().collect();
        let secrets = &secrets;
        match outcome {
            Ok(Ok(text)) => match vet_result(text, secrets) {
                Ok(text) => {
                    if record.is_options_rpc(method) {
                        if let Err(err) = validate_options_result(&text) {
                            gate.record(id, Some(&err));
                            return Err(failure(StatusCode::BAD_GATEWAY, err));
                        }
                    }
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
    ///
    /// `secrets` are redacted from every log line the guest writes during the
    /// call (an `input_scoped` call's input values).
    async fn call_guest(
        &self,
        snapshot: &Arc<RegistrySnapshot>,
        loaded: &LoadedPlugin,
        method: &str,
        payload: &str,
        secrets: &[String],
    ) -> Result<Result<String, String>, CallError> {
        let indices = loaded.rpc.as_ref().ok_or_else(|| CallError::Instantiate("does not export rpc".to_string()))?;
        let dispatcher = HookDispatcher::new(self.runtime(), Arc::clone(snapshot));
        let mut live = dispatcher.open(loaded).await?;
        live.store_mut().data_mut().host.set_call_secrets(secrets.to_vec());
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
            destructive: false,
            output_schema: None,
            input_scoped: false,
        }
    }

    #[allow(clippy::unnecessary_wraps, reason = "the shape of `admit`'s schema closure")]
    fn no_schema() -> Result<Option<Value>, RpcFailure> { Ok(None) }

    fn status<T>(result: &Result<T, RpcFailure>) -> StatusCode {
        result.as_ref().err().map_or(StatusCode::OK, |failure| failure.status)
    }

    fn call(
        gate: &RpcGate,
        export: &RpcExport,
        may_write: bool,
        body: &[u8],
        schema: &Value,
    ) -> Result<Admitted, RpcFailure> {
        admit(gate, "demo", export, || may_write, body, || Ok(Some(schema.clone())), None)
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
        let denied = admit(&gate, "demo", &write, || false, b"{}", no_schema, None);
        assert_eq!(status(&denied), StatusCode::FORBIDDEN);
        assert!(admit(&gate, "demo", &write, || true, b"{}", no_schema, None).is_ok());

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
            None,
        );
        assert!(allowed.is_ok());
        assert!(!asked.get(), "a read method never asks for SystemWrite");
    }

    #[test]
    fn a_forbidden_call_does_not_spend_a_token() {
        let gate = RpcGate::default();
        let write = export(RpcKind::Write, None);
        for _ in 0..(RATE_BURST * 3) {
            assert_eq!(status(&admit(&gate, "demo", &write, || false, b"{}", no_schema, None)), StatusCode::FORBIDDEN);
        }
        assert!(admit(&gate, "demo", &write, || true, b"{}", no_schema, None).is_ok());
    }

    #[test]
    fn an_oversized_payload_is_a_413() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        let exact = format!("\"{}\"", "a".repeat(MAX_RPC_PAYLOAD_BYTES - 2));
        assert_eq!(exact.len(), MAX_RPC_PAYLOAD_BYTES);
        assert!(admit(&gate, "demo", &read, || true, exact.as_bytes(), no_schema, None).is_ok());
        let over = format!("\"{}\"", "a".repeat(MAX_RPC_PAYLOAD_BYTES - 1));
        assert_eq!(
            status(&admit(&gate, "demo", &read, || true, over.as_bytes(), no_schema, None)),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn a_payload_that_is_not_json_is_a_400() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        for body in [&b""[..], b"{", b"not json", b"{\"a\":1} trailing", b"\xff\xfe", b"[1,2"] {
            assert_eq!(
                status(&admit(&gate, "demo", &read, || true, body, no_schema, None)),
                StatusCode::BAD_REQUEST,
                "{body:?}"
            );
        }
        assert_eq!(admit(&gate, "demo", &read, || true, b"null", no_schema, None).unwrap().payload, "null");
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
            call(&gate, &read, true, br#"{"league": "epl", "limit": 5}"#, &schema).unwrap().payload,
            r#"{"league":"epl","limit":5}"#
        );
    }

    #[test]
    fn the_guest_gets_the_compact_json_the_schema_saw() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        let sent = admit(&gate, "demo", &read, || true, b"  {\n \"a\" : 1 ,\"a\": 2 }  ", no_schema, None).unwrap();
        assert_eq!(sent.payload, r#"{"a":2}"#);
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
            assert!(admit(&gate, "demo", &read, || true, b"{}", no_schema, None).is_ok(), "call {call}");
        }
        let limited = admit(&gate, "demo", &read, || true, b"{}", no_schema, None).unwrap_err();
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.retry_after.is_some());
        assert_eq!(gate.counts("demo").rate_limited, 1);
    }

    #[test]
    fn each_plugin_has_its_own_bucket() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        for _ in 0..RATE_BURST {
            admit(&gate, "one", &read, || true, b"{}", no_schema, None).unwrap();
        }
        assert_eq!(status(&admit(&gate, "one", &read, || true, b"{}", no_schema, None)), StatusCode::TOO_MANY_REQUESTS);
        assert!(admit(&gate, "two", &read, || true, b"{}", no_schema, None).is_ok());
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
        assert_eq!(value[REDACTED_SECRET], true);
        assert!(!scrubbed.contains("s3cr3t"));
    }

    #[test]
    fn object_keys_and_values_with_secrets_are_redacted_in_rpc_result() {
        let secrets = vec!["super_secret_key".to_string(), "my_password".to_string()];
        let input_json = json!({
            "key_with_super_secret_key_inside": {
                "nested_my_password": "value containing super_secret_key",
                "normal_key": "normal_val"
            },
            "array": [
                { "key_with_my_password": 123 }
            ]
        });
        let result = vet_result(input_json.to_string(), &secrets).expect("valid json");
        assert!(!result.contains("super_secret_key"), "secret found in keys or values: {result}");
        assert!(!result.contains("my_password"), "password found in keys or values: {result}");
    }

    fn scoped_export() -> RpcExport { RpcExport { input_scoped: true, ..export(RpcKind::Read, Some("rpc/in.json")) } }

    fn living_room() -> RpcInput {
        RpcInput {
            plugin: "demo".to_string(),
            name: "living-room".to_string(),
            url: "http://demo.local/".to_string(),
            username: Some("alice".to_string()),
            password: Some("hunter2".to_string()),
            options: vec![("region".to_string(), "eu".to_string()), ("api_token".to_string(), "t-0k3n".to_string())],
            form_secrets: Vec::new(),
        }
    }

    fn schema_league() -> Value {
        json!({"type": "object", "properties": {"league": {"type": "string"}}, "additionalProperties": false})
    }

    #[test]
    fn a_plain_export_gets_its_payload_exactly_as_before() {
        let gate = RpcGate::default();
        let read = export(RpcKind::Read, None);
        // An input context is ignored, and an `input` member stays a plain argument.
        let admitted =
            admit(&gate, "demo", &read, || true, br#"{"input": "x"}"#, no_schema, Some(&living_room())).unwrap();
        assert_eq!(admitted.payload, r#"{"input":"x"}"#);
        assert_eq!(admitted.secrets, Vec::<String>::new());
    }

    #[test]
    fn an_input_scoped_export_wraps_the_arguments_and_the_context() {
        let gate = RpcGate::default();
        let export = scoped_export();
        let admitted = admit(
            &gate,
            "demo",
            &export,
            || true,
            br#"{"input": "living-room", "league": "epl"}"#,
            || Ok(Some(schema_league())),
            Some(&living_room()),
        )
        .unwrap();
        let payload: Value = serde_json::from_str(&admitted.payload).unwrap();
        assert_eq!(payload["args"], json!({ "league": "epl" }));
        assert_eq!(payload["input"]["name"], "living-room");
        assert_eq!(payload["input"]["url"], "http://demo.local/");
        assert_eq!(payload["input"]["username"], "alice");
        assert_eq!(payload["input"]["password"], "hunter2");
        assert_eq!(payload["input"]["options"], json!({ "region": "eu", "api_token": "t-0k3n" }));
        let mut secrets = admitted.secrets.clone();
        secrets.sort();
        assert_eq!(secrets, ["alice", "hunter2", "t-0k3n"]);

        // The schema checks the arguments, not the input selector.
        let missing = admit(
            &gate,
            "demo",
            &export,
            || true,
            br#"{"input": "living-room"}"#,
            || Ok(Some(json!({"type": "object", "required": ["league"]}))),
            Some(&living_room()),
        )
        .unwrap_err();
        assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(missing.fields[0].path, "league");
    }

    #[test]
    fn an_input_scoped_call_without_a_usable_input_is_a_400() {
        let gate = RpcGate::default();
        let export = scoped_export();
        let cases: [(&[u8], Option<RpcInput>, &str); 6] = [
            (br#"{"league": "epl"}"#, Some(living_room()), "needs an `input`"),
            (br#"{"input": 3}"#, Some(living_room()), "must be the name of a configured input"),
            (br#"{"input": "living-room"}"#, None, "no configured plugin input is named"),
            (
                br#"{"input": "living-room"}"#,
                Some(RpcInput { plugin: "other".to_string(), ..living_room() }),
                "belongs to plugin `other`",
            ),
            (br#"{"input": "bedroom"}"#, Some(living_room()), "does not match the requested `input`"),
            (b"[1,2]", Some(living_room()), "must be a JSON object"),
        ];
        for (body, input, needle) in cases {
            let refused = admit(&gate, "demo", &export, || true, body, no_schema, input.as_ref()).unwrap_err();
            assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{needle}");
            assert!(refused.error.contains(needle), "{} does not contain {needle}", refused.error);
        }
        assert!(gate.counts("demo").calls == 0, "a refused call never reaches the guest");
    }

    #[test]
    fn a_scoped_payload_that_the_context_pushes_over_the_cap_is_a_413() {
        let gate = RpcGate::default();
        let export = scoped_export();
        let big =
            RpcInput { options: vec![("region".to_string(), "r".repeat(MAX_RPC_PAYLOAD_BYTES))], ..living_room() };
        let refused =
            admit(&gate, "demo", &export, || true, br#"{"input": "living-room"}"#, no_schema, Some(&big)).unwrap_err();
        assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn input_secrets_are_scrubbed_from_a_result() {
        let secrets = living_room().secrets();
        let text = json!({"echo": {"password": "hunter2", "options": {"api_token": "t-0k3n"}, "user": "alice"}});
        let scrubbed = vet_result(text.to_string(), &secrets).unwrap();
        for leaked in ["hunter2", "t-0k3n", "alice"] {
            assert!(!scrubbed.contains(leaked), "{leaked} leaked: {scrubbed}");
        }
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

    #[test]
    fn validate_options_result_checks_shape_and_caps() {
        assert!(validate_options_result(r#"[{"value":"1","label":"One"}]"#).is_ok());
        assert!(validate_options_result(r#"{"options":[{"value":"1","label":"One"}]}"#).is_ok());
        assert!(validate_options_result(r#"{"not":"array"}"#).is_err());
        assert!(validate_options_result(r#"[{"value":123,"label":"One"}]"#).is_err());
        assert!(validate_options_result(r#"[{"value":"1"}]"#).is_err());

        // String too long
        let long_val = format!(r#"[{{ "value": "{}", "label": "Ok" }}]"#, "x".repeat(1025));
        assert!(validate_options_result(&long_val).is_err());

        // Oversized list
        let mut items = Vec::new();
        for i in 0..1001 {
            items.push(json!({"value": i.to_string(), "label": i.to_string()}));
        }
        let big = Value::Array(items).to_string();
        let err = validate_options_result(&big).unwrap_err();
        assert!(err.contains("exceeds 1000 items"));
    }

    #[test]
    fn form_secrets_are_scrubbed_from_rpc_result() {
        let input = RpcInput { form_secrets: vec!["my_form_secret_123".to_string()], ..living_room() };
        let secrets = input.secrets();
        assert!(secrets.contains(&"my_form_secret_123".to_string()));
        let text = json!({"output": "leaked my_form_secret_123 in result"});
        let scrubbed = vet_result(text.to_string(), &secrets).unwrap();
        assert!(!scrubbed.contains("my_form_secret_123"));
        assert!(scrubbed.contains(REDACTED_SECRET));
    }

    #[test]
    fn collect_call_secrets_collects_credentials_options_and_form_secrets() {
        let options = vec![
            ("token".to_string(), "tok-sec".to_string()),
            ("plain_opt".to_string(), "not-a-secret".to_string()),
            ("nested".to_string(), "form-secret-val".to_string()),
        ];
        let form_paths = vec!["nested".to_string()];
        let secrets = collect_call_secrets(Some("my_user"), Some("my_pass"), &options, &form_paths);
        assert!(secrets.contains(&"my_user".to_string()));
        assert!(secrets.contains(&"my_pass".to_string()));
        assert!(secrets.contains(&"tok-sec".to_string()));
        assert!(secrets.contains(&"form-secret-val".to_string()));
        assert!(!secrets.contains(&"not-a-secret".to_string()));
    }
}
