//! Read models for the management API (plan §10), served by `app`'s
//! `plugins_api.rs`: the list, stats, settings, tasks and logs of a plugin, and
//! the preparation of a settings save (`PUT …/settings`).
//!
//! Everything here is cheap except the `init` re-run of a save. Settings are
//! read from the operator's **unresolved** config entry and never from
//! [`crate::state::PluginEnv`], so a resolved secret cannot reach a response.

use crate::{
    host_fns,
    install::sanitize_text,
    logs::{LogQuery, PluginLogEntry},
    manifest::{
        package::{
            extension_schema, settings_schema_path, settings_schema_paths, settings_secret_paths, EXTENSION_KEY,
        },
        Concurrency, Manifest,
    },
    registry::{PluginRecord, PluginState},
    runtime::{call_with_deadline, CallError},
    settings::{check_settings, literal_secret_warning, merge_secrets, redact, secret_values},
    system::PluginSystem,
};
use http::StatusCode;
use serde_json::{Map, Value};
use shared::{
    model::{
        PluginBreakerDto, PluginBreakerStateDto, PluginBudgetDto, PluginFingerprintDto, PluginInstancesDto,
        PluginLogEntryDto, PluginLogLevelDto, PluginLogQueryDto, PluginSecretDto, PluginSettingsDto,
        PluginSettingsErrorDto, PluginStateDto, PluginStatsDto, PluginSummaryDto, PluginTaskConcurrencyDto,
        PluginTaskDto, PluginUsageDto, REDACTED_SECRET,
    },
    utils::CONSTANTS,
};
use std::{fs, path::Path, sync::Arc};
use tuliprox_core::{
    model::{PluginEntry, PluginsConfig},
    utils::CircuitState,
};
use tuliprox_plugin_api::ABI_VERSION;

/// What a rejected settings save answers: the HTTP status and the body.
pub type SettingsRejection = (StatusCode, PluginSettingsErrorDto);

fn rejection(status: StatusCode, error: impl Into<String>) -> SettingsRejection {
    (status, PluginSettingsErrorDto { error: error.into(), fields: Vec::new() })
}

impl PluginSystem {
    /// Every plugin directory found, in run order. `config` supplies the
    /// operator's `enabled` flag; without it nothing reads as enabled.
    #[must_use]
    pub fn api_list(&self, config: Option<&PluginsConfig>) -> Vec<PluginSummaryDto> {
        let registry = self.registry();
        registry
            .snapshot()
            .plugins
            .iter()
            .map(|record| self.with_replacement(summary(record, config, registry.settings_review(record, config))))
            .collect()
    }

    /// The list row of plugin `id`.
    #[must_use]
    pub fn api_summary(&self, id: &str, config: Option<&PluginsConfig>) -> Option<PluginSummaryDto> {
        let registry = self.registry();
        registry
            .snapshot()
            .get(id)
            .map(|record| self.with_replacement(summary(record, config, registry.settings_review(record, config))))
    }

    /// Fill the W8 lane D summary fields: a staged update and the retained prior.
    fn with_replacement(&self, mut summary: PluginSummaryDto) -> PluginSummaryDto {
        summary.update = self.update_summary(&summary.id);
        summary.rollback = self.rollback_summary(&summary.id);
        summary
    }

    /// Counters, breaker, instances, quotas and the memory budget of plugin `id`.
    #[must_use]
    pub fn api_stats(&self, id: &str) -> Option<PluginStatsDto> {
        let snapshot = self.registry().snapshot();
        let record = snapshot.get(id)?;
        let stats = record.stats();
        // rpc calls count here too, but never reach the circuit breaker.
        let rpc = self.rpc_counts(id);
        let loaded = record.loaded.as_ref();
        let budget = self.runtime().budget_stats();
        let instances = loaded.map(|loaded| {
            let instances = loaded.prepared.stats();
            PluginInstancesDto {
                live: instances.live,
                max: instances.max,
                instantiations: instances.instantiations,
                waits: instances.waits,
            }
        });
        let kv = loaded.and_then(|loaded| loaded.env.host.kv.as_ref()).map(|kv| {
            let usage = kv.usage();
            PluginUsageDto { used_bytes: usage.used_bytes, quota_bytes: usage.quota_bytes }
        });
        let images = loaded.filter(|loaded| loaded.env.capabilities.image_cache).map(|loaded| {
            let usage = loaded.env.images.usage();
            PluginUsageDto { used_bytes: usage.used_bytes, quota_bytes: usage.quota_bytes }
        });
        Some(PluginStatsDto {
            calls: stats.calls + rpc.calls,
            failures: stats.failures + rpc.failures,
            budget_skips: stats.budget_skips + rpc.budget_skips,
            last_error: stats.last_error.or(rpc.last_error),
            breaker: PluginBreakerDto {
                state: breaker_state(stats.breaker.state),
                consecutive_failures: stats.breaker.consecutive_failures,
                failure_threshold: stats.breaker.failure_threshold,
                forced_open: stats.breaker.forced_open,
                retry_after_ms: stats.breaker.retry_after.map(shared::model::Millis::get),
            },
            instances,
            budget: PluginBudgetDto {
                total_mib: budget.total_mib,
                in_use_mib: budget.in_use_mib,
                waits: budget.waits,
                wait_ms_total: budget.wait_ms_total,
                wait_ms_max: budget.wait_ms_max,
                timeouts: budget.timeouts,
            },
            kv,
            images,
            log_dropped: self.shared().logs.get(id).map_or(0, |log| log.dropped_total()),
            fingerprint: loaded.map(|loaded| PluginFingerprintDto {
                version: loaded.fingerprint.version.clone(),
                wasm_sha256: loaded.fingerprint.wasm_sha256.clone(),
                abi: loaded.fingerprint.abi.to_string(),
            }),
            hooks: record.hook_totals(),
        })
    }

    /// The configured settings of plugin `id` with secrets masked
    /// (see [`mask_settings`]), and the schema the package ships.
    ///
    /// Reads the package's schema files: call it from `tokio::task::spawn_blocking`.
    #[must_use]
    pub fn api_settings(&self, id: &str, config: Option<&PluginsConfig>) -> Option<PluginSettingsDto> {
        let snapshot = self.registry().snapshot();
        let record = snapshot.get(id)?;
        let (schema, paths) = settings_inputs(record);
        let configured = config
            .and_then(|config| config.entry(id))
            .map_or_else(|| Value::Object(serde_json::Map::new()), |entry| Value::Object(entry.settings.clone()));
        let (settings, secrets) = mask_settings(&configured, &paths);
        Some(PluginSettingsDto { settings, schema, secrets, warnings: Vec::new() })
    }

    /// Check and prepare a settings save for plugin `id` (plan §10, `PUT
    /// …/settings`), without persisting anything and without touching the live
    /// registry.
    ///
    /// In order: the body must be an object; secret sentinels take the stored
    /// (unresolved) value from `config`; the result is validated against the
    /// plugin's settings schema; then `init` runs in a throwaway instance
    /// under the call deadline with the `${env:VAR}`-resolved candidate.
    ///
    /// Returns the settings to store (unresolved, masks undone) and the
    /// warnings of the save. The caller persists them and reloads the plugins.
    ///
    /// # Errors
    /// `404` for an unknown id, `422` for a body, schema or `init` rejection,
    /// `503` when no instance could be had within the call deadline.
    pub async fn api_prepare_settings(
        &self,
        id: &str,
        incoming: &Value,
        config: Option<&PluginsConfig>,
    ) -> Result<(Map<String, Value>, Vec<String>), SettingsRejection> {
        let snapshot = self.registry().snapshot();
        let record = snapshot.get(id).ok_or_else(|| rejection(StatusCode::NOT_FOUND, "unknown plugin"))?;
        if !incoming.is_object() {
            return Err(rejection(StatusCode::UNPROCESSABLE_ENTITY, "the settings must be a JSON object"));
        }
        let (schema, paths) = {
            let record = Arc::clone(record);
            tokio::task::spawn_blocking(move || settings_inputs(&record)).await.map_err(|err| {
                rejection(StatusCode::INTERNAL_SERVER_ERROR, format!("reading the settings schema failed: {err}"))
            })?
        };

        let stored = config
            .and_then(|config| config.entry(id))
            .map_or_else(|| Value::Object(Map::new()), |entry| Value::Object(entry.settings.clone()));
        let mut merged = incoming.clone();
        let mut warnings: Vec<String> =
            merge_secrets(&mut merged, &stored, &paths).iter().map(|path| literal_secret_warning(path)).collect();

        let Value::Object(settings) = merged else {
            return Err(rejection(StatusCode::UNPROCESSABLE_ENTITY, "the settings must be a JSON object"));
        };
        let extensions = record.manifest.as_deref().is_some_and(|manifest| !manifest.ui.config_extensions.is_empty());
        if extensions {
            check_extension_settings(record, &settings)?;
            // The plugin's own schema never sees the `ext` values.
            let mut own = settings.clone();
            own.remove(EXTENSION_KEY);
            check_settings(schema.as_ref(), &Value::Object(own))
                .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error))?;
        } else {
            check_settings(schema.as_ref(), &Value::Object(settings.clone()))
                .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error))?;
        }

        if record.loaded.is_some() {
            self.run_candidate_init(record, &settings, &paths).await?;
        } else {
            warnings.push(format!(
                "plugin `{id}` is not loaded ({}); `init` was not run and the plugin has not seen these settings",
                record.state().as_str()
            ));
        }
        Ok((settings, warnings))
    }

    /// Plan §18 *W8 seam freeze*: after an update or rollback reload, whether
    /// plugin `id` is loaded and one fresh instance's `init` accepts its
    /// settings within the hook budget. The settings are the effective ones:
    /// migrated in memory when the stored ones are behind the package's schema
    /// (see [`PluginSystem::effective_settings`]), nothing persisted.
    ///
    /// # Errors
    /// Why the package cannot be activated.
    pub(crate) async fn activation_check(&self, id: &str, config: &PluginsConfig) -> Result<(), String> {
        let snapshot = self.registry().snapshot();
        let record = snapshot.get(id).ok_or_else(|| format!("plugin `{id}` is not installed"))?;
        if record.loaded.is_none() {
            let reason = record.failure().unwrap_or_default();
            return Err(format!("plugin `{id}` did not load ({}): {reason}", record.state().as_str()));
        }
        let settings = self.effective_settings(record, config.entry(id)).await?;
        let paths = secret_paths_of(record);
        self.run_candidate_init(record, &settings, &paths).await.map_err(|(_, error)| error.error)
    }

    /// Run `init` of `record` with `settings` in a throwaway instance. The
    /// registry keeps its own environment and instances.
    async fn run_candidate_init(
        &self,
        record: &PluginRecord,
        settings: &Map<String, Value>,
        paths: &[String],
    ) -> Result<(), SettingsRejection> {
        let Some(loaded) = record.loaded.as_ref() else {
            return Ok(());
        };
        // The same resolution a (re)load does, over a config holding only the candidate.
        let candidate = PluginsConfig {
            plugins: vec![PluginEntry {
                id: record.id.to_string(),
                enabled: true,
                priority: record.priority,
                settings: settings.clone(),
                settings_version: record.settings_version,
            }],
            ..PluginsConfig::default()
        };
        let resolved = candidate.resolved_settings(&record.id).unwrap_or_else(|| Value::Object(Map::new()));
        let secrets = secret_values(&resolved, paths);
        let settings_json = resolved.to_string();
        let env = self
            .shared()
            .env_for(&record.id, loaded.env.capabilities, Arc::from(settings_json.as_str()), Arc::from(secrets.clone()))
            .map_err(|err| rejection(StatusCode::INTERNAL_SERVER_ERROR, redact(err.message(), &secrets)))?;

        let failed = |err: &CallError| match err {
            CallError::Budget { .. } => rejection(StatusCode::SERVICE_UNAVAILABLE, redact(&err.to_string(), &secrets)),
            other => rejection(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("init rejected the settings: {}", redact(&other.to_string(), &secrets)),
            ),
        };
        let mut live = self.runtime().instantiate(&loaded.prepared, &env).await.map_err(|err| failed(&err))?;
        let instance = live.instance();
        let lifecycle = loaded
            .lifecycle
            .load(live.store_mut(), &instance)
            .map_err(|err| failed(&CallError::Trap(format!("{err:#}"))))?;
        let init = call_with_deadline(live.limits(), lifecycle.call_init(live.armed(), &settings_json)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        match init.map_err(|err| failed(&err))? {
            Ok(()) => Ok(()),
            Err(message) => Err(rejection(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("init rejected the settings: {}", redact(&message, &secrets)),
            )),
        }
    }

    /// The tasks plugin `id` declares, each with its last run (`lastRun`: the
    /// serialized `PluginTaskRunDto`, `None` before the first run).
    #[must_use]
    pub fn api_tasks(&self, id: &str) -> Option<Vec<PluginTaskDto>> {
        let snapshot = self.registry().snapshot();
        let record = snapshot.get(id)?;
        Some(
            record
                .manifest
                .as_deref()
                .map(|manifest| {
                    manifest
                        .tasks
                        .iter()
                        .map(|declared| {
                            let last_run = self.tasks().last_run(id, &declared.id);
                            task(declared, last_run.and_then(|run| serde_json::to_value(run).ok()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    /// The buffered log lines of plugin `id`, oldest first. Messages were
    /// redacted before they were buffered.
    #[must_use]
    pub fn api_logs(&self, id: &str, query: &PluginLogQueryDto) -> Option<Vec<PluginLogEntryDto>> {
        let snapshot = self.registry().snapshot();
        snapshot.get(id)?;
        let query = LogQuery { min_level: query.level.map(log_level), since_ms: query.since, limit: query.limit };
        Some(
            self.shared()
                .logs
                .get(id)
                .map(|log| log.entries(&query).into_iter().map(log_entry).collect())
                .unwrap_or_default(),
        )
    }
}

fn summary(record: &PluginRecord, config: Option<&PluginsConfig>, settings_review: Option<String>) -> PluginSummaryDto {
    let manifest = record.manifest.as_deref();
    PluginSummaryDto {
        id: record.id.to_string(),
        name: manifest.map_or_else(|| record.id.to_string(), |manifest| manifest.name.clone()),
        version: manifest.map(|manifest| manifest.version.to_string()),
        description: manifest.and_then(|manifest| manifest.description.as_deref()).map(|text| sanitize_text(text, 512)),
        state: state_dto(record.state()),
        failure: record.failure(),
        enabled: config.and_then(|config| config.entry(&record.id)).is_some_and(|entry| entry.enabled),
        priority: record.priority,
        hooks: manifest
            .map(|manifest| manifest.hooks.iter().map(|hook| hook.as_str().to_string()).collect())
            .unwrap_or_default(),
        capabilities: manifest
            .map(|manifest| {
                manifest.capabilities.granted().into_iter().map(|capability| capability.as_str().to_string()).collect()
            })
            .unwrap_or_default(),
        abi: record
            .loaded
            .as_ref()
            .map_or_else(|| ABI_VERSION.to_string(), |loaded| loaded.fingerprint.abi.to_string()),
        wasm_sha256: record.wasm_sha256.clone(),
        settings_review,
        update: None,
        rollback: None,
    }
}

const fn state_dto(state: PluginState) -> PluginStateDto {
    match state {
        PluginState::Discovered => PluginStateDto::Discovered,
        PluginState::Invalid => PluginStateDto::Invalid,
        PluginState::PendingApproval => PluginStateDto::PendingApproval,
        PluginState::Approved => PluginStateDto::Approved,
        PluginState::Loaded => PluginStateDto::Loaded,
        PluginState::Enabled => PluginStateDto::Enabled,
        PluginState::Disabled => PluginStateDto::Disabled,
        PluginState::Failed => PluginStateDto::Failed,
    }
}

const fn breaker_state(state: CircuitState) -> PluginBreakerStateDto {
    match state {
        CircuitState::Closed => PluginBreakerStateDto::Closed,
        CircuitState::Open => PluginBreakerStateDto::Open,
        CircuitState::HalfOpen => PluginBreakerStateDto::HalfOpen,
        CircuitState::Disabled => PluginBreakerStateDto::Disabled,
    }
}

fn task(task: &crate::manifest::Task, last_run: Option<Value>) -> PluginTaskDto {
    PluginTaskDto {
        id: task.id.clone(),
        title: task.title.clone(),
        default_schedule: task.default_schedule.clone(),
        max_exec_ms: task.max_exec_ms,
        concurrency: match task.concurrency {
            Concurrency::Skip => PluginTaskConcurrencyDto::Skip,
            Concurrency::Queue => PluginTaskConcurrencyDto::Queue,
        },
        respect_maintenance: task.respect_maintenance,
        last_run,
    }
}

const fn log_level(level: PluginLogLevelDto) -> log::Level {
    match level {
        PluginLogLevelDto::Error => log::Level::Error,
        PluginLogLevelDto::Warn => log::Level::Warn,
        PluginLogLevelDto::Info => log::Level::Info,
        PluginLogLevelDto::Debug => log::Level::Debug,
        PluginLogLevelDto::Trace => log::Level::Trace,
    }
}

const fn log_level_dto(level: log::Level) -> PluginLogLevelDto {
    match level {
        log::Level::Error => PluginLogLevelDto::Error,
        log::Level::Warn => PluginLogLevelDto::Warn,
        log::Level::Info => PluginLogLevelDto::Info,
        log::Level::Debug => PluginLogLevelDto::Debug,
        log::Level::Trace => PluginLogLevelDto::Trace,
    }
}

fn log_entry(entry: PluginLogEntry) -> PluginLogEntryDto {
    PluginLogEntryDto { ts_ms: entry.ts_ms, level: log_level_dto(entry.level), message: entry.message }
}

// ---------------------------------------------------------------------------
// Settings and secrets
// ---------------------------------------------------------------------------

/// The settings schema of the package at `dir`; `None` without one or when it
/// cannot be read as JSON (the registry already rejected such a plugin).
pub(crate) fn read_settings_schema(dir: &Path, manifest: &Manifest) -> Option<Value> {
    let relative = settings_schema_path(manifest)?;
    let text = fs::read_to_string(dir.join(relative)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The settings schema and the secret property paths of `record`. Reads package
/// files: call it from `spawn_blocking`.
fn settings_inputs(record: &PluginRecord) -> (Option<Value>, Vec<String>) {
    let schema = record.manifest.as_deref().and_then(|manifest| read_settings_schema(&record.dir, manifest));
    (schema, secret_paths_of(record))
}

/// The secret property paths of the whole settings object of `record`: its
/// schemas' own and, under `ext.<section>.`, those of its extensions. Empty for
/// a record without a readable manifest.
pub(crate) fn secret_paths_of(record: &PluginRecord) -> Vec<String> {
    let Some(manifest) = record.manifest.as_deref() else {
        return Vec::new();
    };
    let schemas: Vec<Value> = settings_schema_paths(manifest)
        .into_iter()
        .filter_map(|relative| serde_json::from_str(&fs::read_to_string(record.dir.join(relative)).ok()?).ok())
        .collect();
    settings_secret_paths(&schemas, manifest).unwrap_or_default()
}

/// Check the `ext` object of a save: one object per declared section, each
/// valid against that section's fragments (plan §11 *Config page integration*).
/// Violation paths come back as `ext.<section>.<field>`.
pub(crate) fn check_extension_settings(
    record: &PluginRecord,
    settings: &Map<String, Value>,
) -> Result<(), SettingsRejection> {
    let Some(manifest) = record.manifest.as_deref() else {
        return Ok(());
    };
    let Some(ext) = settings.get(EXTENSION_KEY) else {
        return Ok(());
    };
    let Value::Object(sections) = ext else {
        return Err(rejection(StatusCode::UNPROCESSABLE_ENTITY, format!("`{EXTENSION_KEY}` must be a JSON object")));
    };
    for (section, value) in sections {
        let Some(fragment) = extension_schema(&manifest.ui.config_extensions, section) else {
            return Err(rejection(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("`{EXTENSION_KEY}.{section}`: the plugin declares no config extension for that section"),
            ));
        };
        if !value.is_object() {
            return Err(rejection(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("`{EXTENSION_KEY}.{section}` must be a JSON object"),
            ));
        }
        check_settings(Some(&fragment), value).map_err(|mut error| {
            for field in &mut error.fields {
                field.path = if field.path == "(root)" {
                    format!("{EXTENSION_KEY}.{section}")
                } else {
                    format!("{EXTENSION_KEY}.{section}.{}", field.path)
                };
            }
            (StatusCode::UNPROCESSABLE_ENTITY, error)
        })?;
    }
    Ok(())
}

/// One step of a secret path (`servers[].key`, `extra.*`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step<'a> {
    Key(&'a str),
    /// `*`: every value of an object (`additionalProperties`).
    Any,
    /// `[]`: every element of an array.
    Items,
}

/// The grammar of `manifest::secrets::secret_property_paths`.
pub(crate) fn parse_secret_path(path: &str) -> Vec<Step<'_>> {
    let mut steps = Vec::new();
    if path == "(root)" {
        return steps;
    }
    for segment in path.split('.') {
        let mut name = segment;
        let mut items = 0;
        while let Some(rest) = name.strip_suffix("[]") {
            name = rest;
            items += 1;
        }
        if name == "*" {
            steps.push(Step::Any);
        } else if !name.is_empty() {
            steps.push(Step::Key(name));
        }
        steps.extend(std::iter::repeat_with(|| Step::Items).take(items));
    }
    steps
}

/// Whether the whole string is one `${env:VAR}` reference, so showing it
/// reveals a variable name and no secret.
pub(crate) fn is_env_reference(text: &str) -> bool {
    CONSTANTS.re_env_var.find(text).is_some_and(|found| found.start() == 0 && found.end() == text.len())
}

/// Mask the value at the end of a secret path. Returns whether it counts as set.
fn mask_leaf(value: &mut Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) if text.is_empty() => false,
        Value::String(text) if is_env_reference(text) => true,
        other => {
            *other = Value::String(REDACTED_SECRET.to_string());
            true
        }
    }
}

fn mask_at(value: &mut Value, steps: &[Step<'_>]) -> bool {
    let Some((step, rest)) = steps.split_first() else {
        return mask_leaf(value);
    };
    match (step, value) {
        (Step::Key(name), Value::Object(map)) => map.get_mut(*name).is_some_and(|child| mask_at(child, rest)),
        (Step::Any, Value::Object(map)) => map.values_mut().fold(false, |set, child| mask_at(child, rest) | set),
        (Step::Items, Value::Array(items)) => items.iter_mut().fold(false, |set, child| mask_at(child, rest) | set),
        _ => false,
    }
}

/// `settings` as configured (unresolved) with every value at a secret `paths`
/// entry masked, and one [`PluginSecretDto`] per path.
///
/// A literal becomes [`REDACTED_SECRET`]; a value that is exactly one
/// `${env:VAR}` reference stays as the reference; an absent or `null` value
/// stays absent. An empty string stays empty and is not `set`. A string that
/// mixes a reference with literal text is masked, because the literal part may
/// be the secret.
pub(crate) fn mask_settings(settings: &Value, paths: &[String]) -> (Value, Vec<PluginSecretDto>) {
    let mut masked = settings.clone();
    let secrets = paths
        .iter()
        .map(|path| PluginSecretDto { path: path.clone(), set: mask_at(&mut masked, &parse_secret_path(path)) })
        .collect();
    (masked, secrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::secrets::secret_property_paths;
    use serde_json::json;

    fn paths(list: &[&str]) -> Vec<String> { list.iter().map(|path| (*path).to_string()).collect() }

    #[test]
    fn literal_secrets_are_masked_and_env_references_are_kept() {
        let settings = json!({
            "token": "hunter2",
            "apiKey": "${env:SPORTS_KEY}",
            "region": "eu",
        });
        let (masked, secrets) = mask_settings(&settings, &paths(&["token", "apiKey"]));
        assert_eq!(masked, json!({"token": REDACTED_SECRET, "apiKey": "${env:SPORTS_KEY}", "region": "eu"}));
        assert_eq!(
            secrets,
            vec![
                PluginSecretDto { path: "token".to_string(), set: true },
                PluginSecretDto { path: "apiKey".to_string(), set: true },
            ]
        );
        assert!(!masked.to_string().contains("hunter2"));
    }

    #[test]
    fn absent_null_and_empty_secrets_are_not_set() {
        let settings = json!({"empty": "", "nothing": null});
        let (masked, secrets) = mask_settings(&settings, &paths(&["missing", "empty", "nothing"]));
        assert_eq!(masked, settings);
        assert!(secrets.iter().all(|secret| !secret.set));
        assert_eq!(secrets.len(), 3);
    }

    #[test]
    fn a_reference_mixed_with_literal_text_is_masked() {
        let settings = json!({"a": "pre-${env:X}", "b": "${env:X}${env:Y}", "c": "${env:X} ", "d": "${env:}"});
        let (masked, secrets) = mask_settings(&settings, &paths(&["a", "b", "c", "d"]));
        assert_eq!(
            masked,
            json!({"a": REDACTED_SECRET, "b": REDACTED_SECRET, "c": REDACTED_SECRET, "d": REDACTED_SECRET})
        );
        assert!(secrets.iter().all(|secret| secret.set));
    }

    #[test]
    fn nested_array_and_wildcard_paths_are_masked() {
        let settings = json!({
            "credentials": {"password": "pw", "user": "me"},
            "servers": [{"key": "k1", "host": "a"}, {"key": "${env:K2}", "host": "b"}, {"host": "c"}],
            "extra": {"x": "ex", "y": ""},
            "matrix": [["m1", "m2"]],
        });
        let (masked, secrets) =
            mask_settings(&settings, &paths(&["credentials.password", "servers[].key", "extra.*", "matrix[][]"]));
        assert_eq!(
            masked,
            json!({
                "credentials": {"password": REDACTED_SECRET, "user": "me"},
                "servers": [{"key": REDACTED_SECRET, "host": "a"}, {"key": "${env:K2}", "host": "b"}, {"host": "c"}],
                "extra": {"x": REDACTED_SECRET, "y": ""},
                "matrix": [[REDACTED_SECRET, REDACTED_SECRET]],
            })
        );
        assert!(secrets.iter().all(|secret| secret.set));
    }

    #[test]
    fn a_non_string_secret_is_masked_whole() {
        let settings = json!({"token": {"nested": "v"}, "pin": 1234});
        let (masked, secrets) = mask_settings(&settings, &paths(&["token", "pin"]));
        assert_eq!(masked, json!({"token": REDACTED_SECRET, "pin": REDACTED_SECRET}));
        assert!(secrets.iter().all(|secret| secret.set));
    }

    #[test]
    fn no_secret_paths_leave_the_settings_alone() {
        let settings = json!({"a": "b"});
        let (masked, secrets) = mask_settings(&settings, &[]);
        assert_eq!(masked, settings);
        assert_eq!(secrets, Vec::<PluginSecretDto>::new());
    }

    #[test]
    fn schema_secret_paths_drive_the_mask() {
        let schema = json!({
            "type": "object",
            "properties": {
                "token": {"type": "string", "writeOnly": true},
                "pass": {"type": "string", "format": "password"},
                "name": {"type": "string"},
            }
        });
        let paths = secret_property_paths(&schema).unwrap();
        let (masked, secrets) = mask_settings(&json!({"token": "t", "pass": "${env:P}", "name": "n"}), &paths);
        assert_eq!(masked, json!({"token": REDACTED_SECRET, "pass": "${env:P}", "name": "n"}));
        assert_eq!(secrets.len(), 2);
    }
}
