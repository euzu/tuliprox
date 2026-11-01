use crate::{model::macros, utils::resolve_env_var};
use serde_json::{Map, Value};
use shared::{
    model::{
        ByteSize, Bytes, Millis, PluginEntryDto, PluginsConfigDto, DEFAULT_PLUGINS_DIR, DEFAULT_PLUGINS_MAX_EXEC,
        DEFAULT_PLUGINS_MAX_MEMORY, DEFAULT_PLUGINS_MAX_TASK_EXEC, DEFAULT_PLUGINS_MAX_TOTAL_MEMORY,
        DEFAULT_PLUGINS_STREAM_HEADER_TTL, MAX_PLUGINS_BATCH_SIZE,
    },
    utils::parse_duration_millis,
};
use std::path::{Path, PathBuf};

/// Runtime form of one `plugins.plugins[]` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginEntry {
    pub id: String,
    pub enabled: bool,
    /// Lower runs first.
    pub priority: i32,
    /// The plugin's settings exactly as configured: `${env:VAR}` references are
    /// **not** expanded here, so the config report and the DTO round-trip never
    /// see a resolved secret. Use [`PluginsConfig::resolved_settings`] at
    /// instantiation.
    pub settings: Map<String, Value>,
    /// The settings-schema `version` `settings` were written for (1 when the
    /// config says nothing). The registry compares it with the installed
    /// package's schema version (plan §18 *W8 seam freeze*).
    pub settings_version: u32,
}

/// Runtime form of the `plugins:` block.
#[derive(Debug, Clone)]
pub struct PluginsConfig {
    pub enabled: bool,
    /// Directory holding one `<id>/` folder per plugin. Absolute once
    /// [`PluginsConfig::prepare`] has run.
    pub directory: String,
    /// Memory all plugin instances may hold together.
    pub max_total_memory: Bytes,
    /// Memory one plugin instance may hold.
    pub max_memory: Bytes,
    /// Deadline of one hook call.
    pub max_exec: Millis,
    /// Deadline of one scheduled task run.
    pub max_task_exec: Millis,
    /// Reuse of injected upstream headers per (plugin, user, channel).
    pub stream_header_ttl: Millis,
    /// Concurrent instances of one plugin.
    pub max_instances: u16,
    /// Items sent to a plugin per hook call.
    pub batch_size: u32,
    pub plugins: Vec<PluginEntry>,
}

macros::from_impl!(PluginsConfig);

impl Default for PluginsConfig {
    /// Disabled, with the frozen default clamps.
    fn default() -> Self { Self::from(&PluginsConfigDto::default()) }
}

impl PluginsConfig {
    /// Resolve `directory` the way config-relative paths are resolved
    /// elsewhere (see `WebAuthConfig`): blank means the default folder, an
    /// absolute path is kept, a relative one is joined onto the config dir.
    pub fn prepare(&mut self, config_path: &str) {
        let configured = self.directory.trim();
        let directory = if configured.is_empty() { DEFAULT_PLUGINS_DIR } else { configured };
        let path = Path::new(directory);
        let resolved: PathBuf = if path.is_absolute() { path.to_path_buf() } else { Path::new(config_path).join(path) };
        self.directory = resolved.to_string_lossy().to_string();
    }

    /// The configured entry for plugin `id`, if the operator listed one.
    #[must_use]
    pub fn entry(&self, id: &str) -> Option<&PluginEntry> { self.plugins.iter().find(|entry| entry.id == id) }

    /// The settings of plugin `id` with every `${env:VAR}` in a string value
    /// expanded through [`resolve_env_var`], at any depth. `None` when the
    /// operator configured no entry for `id`.
    ///
    /// This is the only place secrets are resolved: call it when an instance is
    /// created and drop the result with the instance.
    #[must_use]
    pub fn resolved_settings(&self, id: &str) -> Option<Value> {
        self.entry(id).map(|entry| resolve_value(&Value::Object(entry.settings.clone())))
    }
}

fn resolve_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(resolve_env_var(text)),
        Value::Array(items) => Value::Array(items.iter().map(resolve_value).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(key, item)| (key.clone(), resolve_value(item))).collect()),
        other => other.clone(),
    }
}

/// The byte count of `value`, or of `default` when `value` does not parse.
/// `PluginsConfigDto::prepare` rejects bad values first; this keeps the
/// conversion total, as the other config conversions are.
fn bytes_or(value: &ByteSize, default: &str) -> Bytes {
    value
        .parse_bytes()
        .ok()
        .or_else(|| ByteSize::new(default).parse_bytes().ok())
        .map_or(Bytes::new(1), Bytes::at_least_1)
}

fn millis_or(value: &str, default: &str) -> Millis {
    Millis::new(
        parse_duration_millis(value, true)
            .or_else(|| parse_duration_millis(default, true))
            .map_or(1, |millis| millis.max(1)),
    )
}

/// `1GB`, `512MB`, `64KB` or `7B`: the largest binary unit that divides `bytes`.
fn format_bytes(bytes: Bytes) -> ByteSize {
    const UNITS: [(&str, u64); 4] = [("TB", 1 << 40), ("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    let count = bytes.get();
    for (unit, size) in UNITS {
        if count >= size && count.is_multiple_of(size) {
            return ByteSize::new(format!("{}{unit}", count / size));
        }
    }
    ByteSize::new(format!("{count}B"))
}

/// `2h`, `10m`, `5s` or `250ms`: the largest unit that divides `millis`.
fn format_millis(millis: Millis) -> String {
    const UNITS: [(&str, u64); 4] = [("d", 86_400_000), ("h", 3_600_000), ("m", 60_000), ("s", 1_000)];
    let count = millis.get();
    for (unit, size) in UNITS {
        if count >= size && count.is_multiple_of(size) {
            return format!("{}{unit}", count / size);
        }
    }
    format!("{count}ms")
}

impl From<&PluginEntryDto> for PluginEntry {
    fn from(dto: &PluginEntryDto) -> Self {
        Self {
            id: dto.id.clone(),
            enabled: dto.enabled,
            priority: dto.priority,
            settings: dto.settings.clone(),
            settings_version: dto.settings_version.max(1),
        }
    }
}

impl From<&PluginEntry> for PluginEntryDto {
    fn from(entry: &PluginEntry) -> Self {
        Self {
            id: entry.id.clone(),
            enabled: entry.enabled,
            priority: entry.priority,
            settings: entry.settings.clone(),
            settings_version: entry.settings_version,
        }
    }
}

impl From<&PluginsConfigDto> for PluginsConfig {
    fn from(dto: &PluginsConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            directory: dto.directory.clone(),
            max_total_memory: bytes_or(&dto.max_total_memory, DEFAULT_PLUGINS_MAX_TOTAL_MEMORY),
            max_memory: bytes_or(&dto.max_memory, DEFAULT_PLUGINS_MAX_MEMORY),
            max_exec: millis_or(&dto.max_exec, DEFAULT_PLUGINS_MAX_EXEC),
            max_task_exec: millis_or(&dto.max_task_exec, DEFAULT_PLUGINS_MAX_TASK_EXEC),
            stream_header_ttl: millis_or(&dto.stream_header_ttl, DEFAULT_PLUGINS_STREAM_HEADER_TTL),
            max_instances: dto.max_instances.max(1),
            batch_size: dto.batch_size.clamp(1, MAX_PLUGINS_BATCH_SIZE),
            plugins: dto.plugins.iter().map(PluginEntry::from).collect(),
        }
    }
}

impl From<&PluginsConfig> for PluginsConfigDto {
    fn from(config: &PluginsConfig) -> Self {
        Self {
            enabled: config.enabled,
            directory: config.directory.clone(),
            max_total_memory: format_bytes(config.max_total_memory),
            max_memory: format_bytes(config.max_memory),
            max_exec: format_millis(config.max_exec),
            max_task_exec: format_millis(config.max_task_exec),
            stream_header_ttl: format_millis(config.stream_header_ttl),
            max_instances: config.max_instances,
            batch_size: config.batch_size,
            plugins: config.plugins.iter().map(PluginEntryDto::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use shared::model::Prepare;

    fn dto_from(value: Value) -> PluginsConfigDto { serde_json::from_value(value).unwrap() }

    #[test]
    fn dto_round_trips_through_runtime_config() {
        let mut entry = PluginEntryDto::new("sports-enricher");
        entry.priority = 7;
        entry.settings.insert("token".to_string(), json!("${env:SPORTS_TOKEN}"));
        let dto = PluginsConfigDto {
            enabled: true,
            directory: "/opt/plugins".to_string(),
            max_total_memory: ByteSize::new("1GB"),
            max_memory: ByteSize::new("256MB"),
            max_exec: "250ms".to_string(),
            max_task_exec: "30m".to_string(),
            stream_header_ttl: "90s".to_string(),
            max_instances: 8,
            batch_size: 250,
            plugins: vec![entry],
        };
        assert_eq!(PluginsConfigDto::from(&PluginsConfig::from(&dto)), dto);
    }

    #[test]
    fn settings_version_defaults_to_one_and_is_only_written_when_it_differs() {
        let dto = dto_from(json!({"plugins": [{"id": "demo-plugin"}, {"id": "other-plugin", "settings_version": 3}]}));
        let config = PluginsConfig::from(&dto);
        assert_eq!(config.entry("demo-plugin").unwrap().settings_version, 1);
        assert_eq!(config.entry("other-plugin").unwrap().settings_version, 3);
        let written = serde_json::to_value(PluginsConfigDto::from(&config)).unwrap();
        assert!(written["plugins"][0].get("settings_version").is_none(), "1 is omitted");
        assert_eq!(written["plugins"][1]["settings_version"], 3);
        // a 0 in the file is read as the default 1
        let zero = PluginsConfig::from(&dto_from(json!({"plugins": [{"id": "demo-plugin", "settings_version": 0}]})));
        assert_eq!(zero.entry("demo-plugin").unwrap().settings_version, 1);
    }

    #[test]
    fn default_dto_maps_to_disabled_with_the_frozen_clamps() {
        let config = PluginsConfig::from(&PluginsConfigDto::default());
        assert!(!config.enabled);
        assert_eq!(config.max_total_memory, Bytes::new(512 * 1024 * 1024));
        assert_eq!(config.max_memory, Bytes::new(128 * 1024 * 1024));
        assert_eq!(config.max_exec, Millis::new(5_000));
        assert_eq!(config.max_task_exec, Millis::new(600_000));
        assert_eq!(config.max_instances, 4);
        assert_eq!(config.plugins.len(), 0);
        assert_eq!(PluginsConfigDto::from(&config), PluginsConfigDto::default());
    }

    #[test]
    fn durations_use_millisecond_support_and_every_unit() {
        let mut dto = dto_from(json!({"max_exec": "250ms", "max_task_exec": "2h"}));
        dto.prepare(()).unwrap();
        let config = PluginsConfig::from(&dto);
        assert_eq!(config.max_exec, Millis::new(250));
        assert_eq!(config.max_task_exec, Millis::new(7_200_000));
    }

    #[test]
    fn invalid_values_fall_back_to_the_defaults_instead_of_panicking() {
        let config =
            PluginsConfig::from(&dto_from(json!({"max_exec": "soon", "max_memory": "lots", "max_instances": 0})));
        assert_eq!(config.max_exec, Millis::new(5_000));
        assert_eq!(config.max_memory, Bytes::new(128 * 1024 * 1024));
        assert_eq!(config.max_instances, 1);
    }

    #[test]
    fn canonical_formatting_picks_the_largest_exact_unit() {
        assert_eq!(format_bytes(Bytes::new(512 << 20)).as_str(), "512MB");
        assert_eq!(format_bytes(Bytes::new(1024 << 20)).as_str(), "1GB");
        assert_eq!(format_bytes(Bytes::new(1500)).as_str(), "1500B");
        assert_eq!(format_millis(Millis::new(600_000)).as_str(), "10m");
        assert_eq!(format_millis(Millis::new(90_000)).as_str(), "90s");
        assert_eq!(format_millis(Millis::new(1_500)).as_str(), "1500ms");
        assert_eq!(format_millis(Millis::new(86_400_000)).as_str(), "1d");
        // what is printed parses back to the same value
        assert_eq!(bytes_or(&format_bytes(Bytes::new(1500)), "1B"), Bytes::new(1500));
        assert_eq!(millis_or(&format_millis(Millis::new(1_500)), "1s"), Millis::new(1_500));
    }

    #[test]
    fn relative_directory_is_joined_onto_the_config_dir() {
        let mut config = PluginsConfig::from(&dto_from(json!({"enabled": true})));
        config.prepare("/etc/tuliprox/config");
        assert_eq!(Path::new(&config.directory), Path::new("/etc/tuliprox/config/plugins"));
    }

    #[test]
    fn blank_directory_falls_back_to_the_default() {
        let mut config = PluginsConfig::from(&dto_from(json!({"enabled": true})));
        config.directory = "  ".to_string();
        config.prepare("/etc/tuliprox/config");
        assert_eq!(Path::new(&config.directory), Path::new("/etc/tuliprox/config/plugins"));
    }

    #[test]
    fn absolute_directory_is_kept() {
        let mut config = PluginsConfig::from(&dto_from(json!({"directory": "/opt/plugins"})));
        config.prepare("/etc/tuliprox/config");
        assert_eq!(config.directory, "/opt/plugins");
    }

    fn config_with_settings(settings: &Value) -> PluginsConfig {
        let mut config = PluginsConfig::default();
        let mut entry = PluginEntry::from(&PluginEntryDto::new("demo-plugin"));
        entry.settings = settings.as_object().cloned().unwrap();
        config.plugins.push(entry);
        config
    }

    #[test]
    fn runtime_keeps_env_references_unresolved() {
        let config = config_with_settings(&json!({"token": "${env:TULIPROX_PLUGIN_TEST_UNSET_A}"}));
        assert_eq!(config.entry("demo-plugin").unwrap().settings["token"], "${env:TULIPROX_PLUGIN_TEST_UNSET_A}");
        assert_eq!(PluginsConfigDto::from(&config).plugins[0].settings["token"], "${env:TULIPROX_PLUGIN_TEST_UNSET_A}");
    }

    #[test]
    fn resolved_settings_expands_env_references_at_any_depth() {
        // PATH exists on every platform the tests run on; the value itself is irrelevant.
        let path = std::env::var("PATH").expect("PATH is set");
        let config = config_with_settings(&json!({
            "token": "${env:PATH}",
            "prefix": "x-${env:PATH}-y",
            "nested": {"list": ["${env:PATH}", 7, true, null]},
            "plain": "keep ${not-env} as is",
            "count": 3
        }));
        let resolved = config.resolved_settings("demo-plugin").unwrap();
        assert_eq!(resolved["token"], path.as_str());
        assert_eq!(resolved["prefix"], format!("x-{path}-y").as_str());
        assert_eq!(resolved["nested"]["list"], json!([path, 7, true, null]));
        assert_eq!(resolved["plain"], "keep ${not-env} as is");
        assert_eq!(resolved["count"], 3);
        // resolving does not touch the stored settings
        assert_eq!(config.entry("demo-plugin").unwrap().settings["token"], "${env:PATH}");
    }

    #[test]
    fn unresolvable_env_reference_stays_literal() {
        let config = config_with_settings(&json!({"token": "${env:TULIPROX_PLUGIN_TEST_UNSET_B}"}));
        assert_eq!(config.resolved_settings("demo-plugin").unwrap()["token"], "${env:TULIPROX_PLUGIN_TEST_UNSET_B}");
    }

    #[test]
    fn resolved_settings_of_an_unknown_plugin_is_none() {
        assert!(config_with_settings(&json!({})).resolved_settings("other-plugin").is_none());
        assert_eq!(config_with_settings(&json!({})).resolved_settings("demo-plugin"), Some(json!({})));
    }
}
