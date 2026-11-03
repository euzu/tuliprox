use crate::{
    defaults::{default_as_true, is_false},
    error::TuliproxError,
    model::{ByteSize, Prepare},
    utils::parse_duration_millis,
};
use std::collections::HashSet;

/// Directory plugins are loaded from when `plugins.directory` is not set.
/// Resolved against the config directory, so by default `config/plugins`.
pub const DEFAULT_PLUGINS_DIR: &str = "plugins";

/// Default for `plugins.max_total_memory`: every plugin instance together.
pub const DEFAULT_PLUGINS_MAX_TOTAL_MEMORY: &str = "512MB";
/// Default for `plugins.max_memory`: the ceiling of a single instance.
pub const DEFAULT_PLUGINS_MAX_MEMORY: &str = "128MB";
/// Default for `plugins.max_exec`: the deadline of one hook call.
pub const DEFAULT_PLUGINS_MAX_EXEC: &str = "5s";
/// Default for `plugins.max_task_exec`: the deadline of one scheduled task run.
pub const DEFAULT_PLUGINS_MAX_TASK_EXEC: &str = "10m";
/// Default for `plugins.stream_header_ttl`: how long injected upstream headers
/// are reused per (plugin, user, channel) before the plugin is asked again.
pub const DEFAULT_PLUGINS_STREAM_HEADER_TTL: &str = "1m";
/// Default for `plugins.max_instances`: concurrent instances of one plugin.
pub const DEFAULT_PLUGINS_MAX_INSTANCES: u16 = 4;
/// Default for `plugins.batch_size`: items sent to a plugin per hook call.
pub const DEFAULT_PLUGINS_BATCH_SIZE: u32 = 500;
/// Largest `plugins.batch_size`: bounds the guest memory one call needs.
pub const MAX_PLUGINS_BATCH_SIZE: u32 = 10_000;
/// Default for `plugins.boot_update_budget`: how long the background boot
/// update run may take before it gives up until the next boot.
pub const DEFAULT_PLUGINS_BOOT_UPDATE_BUDGET: &str = "30s";
/// Default for `plugins.plugins[].priority`; lower runs first.
pub const DEFAULT_PLUGIN_PRIORITY: i32 = 100;
/// Default for `plugins.plugins[].settings_version`, and the settings-schema
/// `version` of a schema that declares none.
pub const DEFAULT_PLUGIN_SETTINGS_VERSION: u32 = 1;

/// Env-only developer switch: `1` lets marketplace
/// catalog and package URLs use `http://` and loopback destinations, for a
/// local test marketplace. There is no config field; a WARN is logged at
/// startup and on every fetch while it is set.
pub const ENV_PLUGIN_MARKETPLACE_ALLOW_HTTP: &str = "TULIPROX_PLUGIN_MARKETPLACE_ALLOW_HTTP";

/// Shortest and longest plugin id, the `^[a-z][a-z0-9-]{2,40}$` of the manifest.
pub const PLUGIN_ID_LEN: std::ops::RangeInclusive<usize> = 3..=41;

fn default_plugins_directory() -> String { DEFAULT_PLUGINS_DIR.to_string() }
fn default_max_total_memory() -> ByteSize { ByteSize::new(DEFAULT_PLUGINS_MAX_TOTAL_MEMORY) }
fn default_max_memory() -> ByteSize { ByteSize::new(DEFAULT_PLUGINS_MAX_MEMORY) }
fn default_max_exec() -> String { DEFAULT_PLUGINS_MAX_EXEC.to_string() }
fn default_max_task_exec() -> String { DEFAULT_PLUGINS_MAX_TASK_EXEC.to_string() }
fn default_stream_header_ttl() -> String { DEFAULT_PLUGINS_STREAM_HEADER_TTL.to_string() }
fn default_boot_update_budget() -> String { DEFAULT_PLUGINS_BOOT_UPDATE_BUDGET.to_string() }
const fn default_max_instances() -> u16 { DEFAULT_PLUGINS_MAX_INSTANCES }
const fn default_batch_size() -> u32 { DEFAULT_PLUGINS_BATCH_SIZE }
const fn default_plugin_priority() -> i32 { DEFAULT_PLUGIN_PRIORITY }
const fn default_settings_version() -> u32 { DEFAULT_PLUGIN_SETTINGS_VERSION }

fn is_default_plugins_directory(value: &str) -> bool { value.trim().is_empty() || value == DEFAULT_PLUGINS_DIR }
fn is_default_max_total_memory(value: &ByteSize) -> bool { value.as_str() == DEFAULT_PLUGINS_MAX_TOTAL_MEMORY }
fn is_default_max_memory(value: &ByteSize) -> bool { value.as_str() == DEFAULT_PLUGINS_MAX_MEMORY }
fn is_default_max_exec(value: &str) -> bool { value == DEFAULT_PLUGINS_MAX_EXEC }
fn is_default_max_task_exec(value: &str) -> bool { value == DEFAULT_PLUGINS_MAX_TASK_EXEC }
fn is_default_stream_header_ttl(value: &str) -> bool { value == DEFAULT_PLUGINS_STREAM_HEADER_TTL }
fn is_default_boot_update_budget(value: &str) -> bool { value == DEFAULT_PLUGINS_BOOT_UPDATE_BUDGET }
const fn is_default_max_instances(value: &u16) -> bool { *value == DEFAULT_PLUGINS_MAX_INSTANCES }
const fn is_default_batch_size(value: &u32) -> bool { *value == DEFAULT_PLUGINS_BATCH_SIZE }
const fn is_default_plugin_priority(value: &i32) -> bool { *value == DEFAULT_PLUGIN_PRIORITY }
const fn is_default_settings_version(value: &u32) -> bool { *value == DEFAULT_PLUGIN_SETTINGS_VERSION }
const fn is_true(value: &bool) -> bool { *value }

/// `true` for `^[a-z][a-z0-9-]*$` whose length lies in `len`.
///
/// The shape of every kebab-case identifier of the plugin system: plugin ids,
/// task ids. Hand written because it is a handful of byte checks, and this
/// crate also compiles for the web UI.
#[must_use]
pub fn is_kebab_id(value: &str, len: std::ops::RangeInclusive<usize>) -> bool {
    let bytes = value.as_bytes();
    len.contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes.iter().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// `true` for a plugin id: `^[a-z][a-z0-9-]{2,40}$`.
#[must_use]
pub fn is_valid_plugin_id(value: &str) -> bool { is_kebab_id(value, PLUGIN_ID_LEN) }

/// The length of a plugin category id.
pub const PLUGIN_CATEGORY_LEN: std::ops::RangeInclusive<usize> = 2..=32;

/// The categories the Web UI knows by name (a translated label each). Any other valid category id is
/// shown as written, so a publisher is not held back by this list.
pub const KNOWN_PLUGIN_CATEGORIES: [&str; 9] =
    ["notifier", "playlist", "epg", "metadata", "input", "output", "stream", "automation", "monitoring"];

/// `true` for a plugin category id: `^[a-z][a-z0-9-]{1,31}$`.
#[must_use]
pub fn is_valid_plugin_category(value: &str) -> bool { is_kebab_id(value, PLUGIN_CATEGORY_LEN) }

/// A category as a catalog or a query wrote it, as the id it means: trimmed, lower case, and every run of
/// spaces, underscores and other separators one `-`. `EPG` and `epg` are one category, and `Live TV` is
/// `live-tv`. `None` when nothing valid is left (empty, starts with a digit, too long).
#[must_use]
pub fn normalize_plugin_category(raw: &str) -> Option<String> {
    let mut id = String::new();
    let mut pending_separator = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_separator && !id.is_empty() {
                id.push('-');
            }
            pending_separator = false;
            id.push(ch.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }
    is_valid_plugin_category(&id).then_some(id)
}

/// How a marketplace-installed plugin may move to a newer release.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUpdatePolicy {
    /// Updated at boot when `plugins.auto_update_on_boot` is on and the update
    /// needs no approval; offered by "update all".
    #[default]
    Auto,
    /// Never updated at boot; offered by manual checks and "update all".
    Manual,
    /// Never updated at boot and left out of "update all"; a single update is
    /// still possible by hand.
    Pinned,
}

impl PluginUpdatePolicy {
    #[must_use]
    pub const fn is_auto(&self) -> bool { matches!(self, Self::Auto) }
}

/// One `plugins.plugins[]` entry: the operator's switch, order and settings for
/// an installed plugin.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginEntryDto {
    /// The plugin's manifest id.
    pub id: String,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Lower runs first.
    #[serde(default = "default_plugin_priority", skip_serializing_if = "is_default_plugin_priority")]
    pub priority: i32,
    /// Values for the plugin's `config-schema.json`. String values may be
    /// `${env:VAR}` references; they are kept unresolved here.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub settings: serde_json::Map<String, serde_json::Value>,
    /// The settings-schema `version` the stored `settings` are written for. Absent means 1, and 1 is never written.
    #[serde(default = "default_settings_version", skip_serializing_if = "is_default_settings_version")]
    pub settings_version: u32,
    /// How this plugin may be updated from its marketplace source. Absent
    /// means `auto`, and `auto` is never written.
    #[serde(default, skip_serializing_if = "PluginUpdatePolicy::is_auto")]
    pub update_policy: PluginUpdatePolicy,
}

impl PluginEntryDto {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            enabled: default_as_true(),
            priority: DEFAULT_PLUGIN_PRIORITY,
            settings: serde_json::Map::new(),
            settings_version: DEFAULT_PLUGIN_SETTINGS_VERSION,
            update_policy: PluginUpdatePolicy::Auto,
        }
    }
}

/// One `plugins.repositories[]` entry: a marketplace source. `url` is the direct HTTPS/raw URL of the repository's
/// root `marketplace.json` and may be a `${env:VAR}` reference; it is kept
/// unresolved here. `id` uses the plugin id shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginRepositoryDto {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

/// The `plugins:` block of `config.yml`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfigDto {
    /// Load wasm plugins. Off by default: with it off no engine is created and
    /// the pipeline behaves exactly as it does without the plugin system.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,

    /// Directory holding one `<id>/` folder per plugin, each with a
    /// `manifest.yml` and a `plugin.wasm`. A relative path is resolved against
    /// the config directory.
    #[serde(default = "default_plugins_directory", skip_serializing_if = "is_default_plugins_directory")]
    pub directory: String,

    /// Memory all plugin instances may hold together.
    #[serde(default = "default_max_total_memory", skip_serializing_if = "is_default_max_total_memory")]
    pub max_total_memory: ByteSize,

    /// Memory one plugin instance may hold. A manifest's `max_memory_mb` is
    /// clamped to this.
    #[serde(default = "default_max_memory", skip_serializing_if = "is_default_max_memory")]
    pub max_memory: ByteSize,

    /// Deadline of one hook call (`500ms`, `5s`). A unit is required.
    #[serde(default = "default_max_exec", skip_serializing_if = "is_default_max_exec")]
    pub max_exec: String,

    /// Deadline of one scheduled task run (`30s`, `10m`). A unit is required.
    #[serde(default = "default_max_task_exec", skip_serializing_if = "is_default_max_task_exec")]
    pub max_task_exec: String,

    /// How long headers a plugin injected into an upstream stream request are
    /// reused for the same plugin, user and channel (`60s`). A unit is required.
    #[serde(default = "default_stream_header_ttl", skip_serializing_if = "is_default_stream_header_ttl")]
    pub stream_header_ttl: String,

    /// Concurrent instances of one plugin.
    #[serde(default = "default_max_instances", skip_serializing_if = "is_default_max_instances")]
    pub max_instances: u16,

    /// Items sent to a plugin per hook call. Hooks see a large playlist in
    /// batches of this size, never whole.
    #[serde(default = "default_batch_size", skip_serializing_if = "is_default_batch_size")]
    pub batch_size: u32,

    /// At boot, update marketplace-installed, enabled plugins whose policy is
    /// `auto` in the background, when the update needs no approval.
    /// Startup never waits for it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_update_on_boot: bool,

    /// How long the background boot update run may take (`30s`). A unit is
    /// required. On overrun the current packages stay and the next boot retries.
    #[serde(default = "default_boot_update_budget", skip_serializing_if = "is_default_boot_update_budget")]
    pub boot_update_budget: String,

    /// Marketplace sources. Removing one leaves its installed plugins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<PluginRepositoryDto>,

    /// Per-plugin switches, order and settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<PluginEntryDto>,
}

impl Default for PluginsConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: default_plugins_directory(),
            max_total_memory: default_max_total_memory(),
            max_memory: default_max_memory(),
            max_exec: default_max_exec(),
            max_task_exec: default_max_task_exec(),
            stream_header_ttl: default_stream_header_ttl(),
            max_instances: default_max_instances(),
            batch_size: default_batch_size(),
            auto_update_on_boot: false,
            boot_update_budget: default_boot_update_budget(),
            repositories: Vec::new(),
            plugins: Vec::new(),
        }
    }
}

/// Trim `value`, falling back to `default` when it is blank.
fn clean_or_default(value: &mut String, default: &str) {
    let trimmed = value.trim();
    *value = if trimmed.is_empty() { default.to_string() } else { trimmed.to_string() };
}

fn check_size(name: &str, value: &mut ByteSize, default: &str, errors: &mut Vec<String>) -> Option<u64> {
    value.clean_or_default(default);
    match value.parse_bytes() {
        Ok(bytes) if bytes.get() == 0 => {
            errors.push(format!("plugins.{name} must not be zero"));
            None
        }
        Ok(bytes) => Some(bytes.get()),
        Err(err) => {
            errors.push(format!("plugins.{name} `{value}` is not a byte size (for example `128MB`): {err}"));
            None
        }
    }
}

fn check_duration(name: &str, value: &mut String, default: &str, errors: &mut Vec<String>) {
    clean_or_default(value, default);
    match parse_duration_millis(value, true) {
        Some(0) => errors.push(format!("plugins.{name} must not be zero")),
        Some(_) => {}
        None => errors.push(format!(
            "plugins.{name} `{value}` is not a duration; write a number with a unit, for example `500ms`, `5s` or `10m`"
        )),
    }
}

impl Prepare for PluginsConfigDto {
    type Ctx<'a> = ();

    /// Normalise spelling and check every clamp, reporting all problems at once.
    fn prepare(&mut self, (): ()) -> Result<(), TuliproxError> {
        let mut errors = Vec::new();

        let total =
            check_size("max_total_memory", &mut self.max_total_memory, DEFAULT_PLUGINS_MAX_TOTAL_MEMORY, &mut errors);
        let each = check_size("max_memory", &mut self.max_memory, DEFAULT_PLUGINS_MAX_MEMORY, &mut errors);
        if let (Some(total), Some(each)) = (total, each) {
            if each > total {
                errors.push(format!(
                    "plugins.max_memory `{}` must not exceed plugins.max_total_memory `{}`",
                    self.max_memory, self.max_total_memory
                ));
            }
        }
        check_duration("max_exec", &mut self.max_exec, DEFAULT_PLUGINS_MAX_EXEC, &mut errors);
        check_duration("max_task_exec", &mut self.max_task_exec, DEFAULT_PLUGINS_MAX_TASK_EXEC, &mut errors);
        check_duration(
            "stream_header_ttl",
            &mut self.stream_header_ttl,
            DEFAULT_PLUGINS_STREAM_HEADER_TTL,
            &mut errors,
        );
        check_duration(
            "boot_update_budget",
            &mut self.boot_update_budget,
            DEFAULT_PLUGINS_BOOT_UPDATE_BUDGET,
            &mut errors,
        );
        if self.max_instances == 0 {
            errors.push("plugins.max_instances must be at least 1".to_string());
        }
        if !(1..=MAX_PLUGINS_BATCH_SIZE).contains(&self.batch_size) {
            errors.push(format!("plugins.batch_size must be between 1 and {MAX_PLUGINS_BATCH_SIZE}"));
        }

        let mut seen = HashSet::new();
        let mut reported = HashSet::new();
        for entry in &mut self.plugins {
            entry.id = entry.id.trim().to_string();
            if !is_valid_plugin_id(&entry.id) {
                errors.push(format!(
                    "plugins.plugins id `{}` is invalid: use lowercase letters, digits and `-`, starting with a letter, 3 to 41 characters",
                    entry.id
                ));
            } else if !seen.insert(entry.id.clone()) && reported.insert(entry.id.clone()) {
                errors.push(format!("plugins.plugins lists `{}` more than once", entry.id));
            }
        }

        let mut seen = HashSet::new();
        let mut reported = HashSet::new();
        for repository in &mut self.repositories {
            repository.id = repository.id.trim().to_string();
            repository.name = repository.name.trim().to_string();
            repository.url = repository.url.trim().to_string();
            if !is_valid_plugin_id(&repository.id) {
                errors.push(format!(
                    "plugins.repositories id `{}` is invalid: use lowercase letters, digits and `-`, starting with a letter, 3 to 41 characters",
                    repository.id
                ));
            } else if !seen.insert(repository.id.clone()) && reported.insert(repository.id.clone()) {
                errors.push(format!("plugins.repositories lists `{}` more than once", repository.id));
            }
            if repository.name.is_empty() {
                errors.push(format!("plugins.repositories `{}` needs a name", repository.id));
            }
            if repository.url.is_empty() {
                errors.push(format!("plugins.repositories `{}` needs a url", repository.id));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(TuliproxError::Plugin(errors.join("\n")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn from_json(value: serde_json::Value) -> PluginsConfigDto { serde_json::from_value(value).unwrap() }

    fn prepare_message(mut dto: PluginsConfigDto) -> String {
        dto.prepare(()).expect_err("config should be rejected").to_string()
    }

    #[test]
    fn empty_block_is_disabled_with_the_frozen_defaults() {
        let dto: PluginsConfigDto = serde_json::from_str("{}").unwrap();
        assert_eq!(dto, PluginsConfigDto::default());
        assert!(!dto.enabled);
        assert_eq!(dto.directory, "plugins");
        assert_eq!(dto.max_total_memory.as_str(), "512MB");
        assert_eq!(dto.max_memory.as_str(), "128MB");
        assert_eq!(dto.max_exec, "5s");
        assert_eq!(dto.max_task_exec, "10m");
        assert_eq!(dto.max_instances, 4);
        assert_eq!(dto.plugins.len(), 0);
    }

    #[test]
    fn default_block_serializes_empty_and_prepares_cleanly() {
        assert_eq!(serde_json::to_string(&PluginsConfigDto::default()).unwrap(), "{}");
        let mut dto = PluginsConfigDto::default();
        dto.prepare(()).unwrap();
        assert_eq!(dto, PluginsConfigDto::default());
    }

    #[test]
    fn round_trips_non_default_values() {
        let mut entry = PluginEntryDto::new("sports-enricher");
        entry.enabled = false;
        entry.priority = -5;
        entry.settings.insert("token".to_string(), json!("${env:SPORTS_TOKEN}"));
        entry.settings.insert("leagues".to_string(), json!(["nfl", "nba"]));
        entry.update_policy = PluginUpdatePolicy::Pinned;
        let dto = PluginsConfigDto {
            enabled: true,
            directory: "/opt/tuliprox/plugins".to_string(),
            max_total_memory: ByteSize::new("1GB"),
            max_memory: ByteSize::new("256MB"),
            max_exec: "250ms".to_string(),
            max_task_exec: "30m".to_string(),
            stream_header_ttl: "90s".to_string(),
            max_instances: 8,
            batch_size: 250,
            auto_update_on_boot: true,
            boot_update_budget: "45s".to_string(),
            repositories: vec![PluginRepositoryDto {
                id: "community".to_string(),
                name: "Community".to_string(),
                url: "${env:TP_MARKET_URL}".to_string(),
                enabled: false,
            }],
            plugins: vec![entry],
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert_eq!(serde_json::from_str::<PluginsConfigDto>(&json).unwrap(), dto);
    }

    #[test]
    fn entry_defaults_are_enabled_priority_100_no_settings() {
        let dto = from_json(json!({"plugins": [{"id": "demo-plugin"}]}));
        let entry = &dto.plugins[0];
        assert!(entry.enabled);
        assert_eq!(entry.priority, 100);
        assert_eq!(entry.settings.len(), 0);
        assert_eq!(serde_json::to_value(entry).unwrap(), json!({"id": "demo-plugin"}));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(serde_json::from_str::<PluginsConfigDto>(r#"{"enabld": true}"#).is_err());
        assert!(
            serde_json::from_str::<PluginsConfigDto>(r#"{"plugins": [{"id": "demo-plugin", "setting": {}}]}"#).is_err()
        );
    }

    #[test]
    fn settings_must_be_an_object() {
        assert!(serde_json::from_str::<PluginsConfigDto>(r#"{"plugins": [{"id": "demo-plugin", "settings": [1]}]}"#)
            .is_err());
    }

    #[test]
    fn prepare_trims_and_fills_blank_values() {
        let mut dto =
            from_json(json!({"max_memory": " 64MB ", "max_exec": "  ", "plugins": [{"id": " demo-plugin "}]}));
        dto.prepare(()).unwrap();
        assert_eq!(dto.max_memory.as_str(), "64MB");
        assert_eq!(dto.max_exec, "5s");
        assert_eq!(dto.plugins[0].id, "demo-plugin");
    }

    #[test]
    fn prepare_accepts_millisecond_and_larger_units() {
        let mut dto = from_json(json!({"max_exec": "250ms", "max_task_exec": "2h"}));
        dto.prepare(()).unwrap();
    }

    #[test]
    fn durations_need_a_unit_and_must_not_be_zero() {
        assert!(prepare_message(from_json(json!({"max_exec": "5000"}))).contains("plugins.max_exec"));
        assert!(prepare_message(from_json(json!({"max_exec": "soon"}))).contains("not a duration"));
        assert!(prepare_message(from_json(json!({"max_task_exec": "0s"})))
            .contains("plugins.max_task_exec must not be zero"));
    }

    #[test]
    fn sizes_must_parse_and_must_not_be_zero() {
        assert!(prepare_message(from_json(json!({"max_memory": "lots"}))).contains("not a byte size"));
        assert!(prepare_message(from_json(json!({"max_total_memory": "0"}))).contains("must not be zero"));
    }

    #[test]
    fn per_instance_memory_must_fit_in_the_total() {
        let message = prepare_message(from_json(json!({"max_memory": "1GB"})));
        assert!(message.contains("must not exceed plugins.max_total_memory"), "{message}");
        let mut equal = from_json(json!({"max_memory": "512MB"}));
        equal.prepare(()).unwrap();
    }

    #[test]
    fn instances_must_be_positive() {
        assert!(prepare_message(from_json(json!({"max_instances": 0}))).contains("max_instances"));
    }

    #[test]
    fn plugin_ids_must_be_valid_and_unique() {
        let message = prepare_message(from_json(
            json!({"plugins": [{"id": "ok-plugin"}, {"id": "ok-plugin"}, {"id": "ok-plugin"}, {"id": "Bad_Id"}]}),
        ));
        assert!(message.contains("`Bad_Id` is invalid"), "{message}");
        assert_eq!(message.matches("more than once").count(), 1, "{message}");
    }

    #[test]
    fn prepare_reports_every_problem_at_once() {
        let message = prepare_message(from_json(json!({
            "max_exec": "never", "max_instances": 0, "max_memory": "x", "plugins": [{"id": "a"}]
        })));
        assert_eq!(message.lines().count(), 4, "{message}");
    }

    #[test]
    fn repositories_default_to_none_and_enabled() {
        let dto = from_json(
            json!({"repositories": [{"id": "community", "name": "Community", "url": "https://x.test/marketplace.json"}]}),
        );
        assert!(dto.repositories[0].enabled);
        assert_eq!(
            serde_json::to_value(&dto.repositories[0]).unwrap(),
            json!({"id": "community", "name": "Community", "url": "https://x.test/marketplace.json"})
        );
        assert_eq!(PluginsConfigDto::default().repositories.len(), 0);
    }

    #[test]
    fn repositories_are_trimmed_and_keep_env_references() {
        let mut dto = from_json(
            json!({"repositories": [{"id": " community ", "name": " Community ", "url": " ${env:TP_MARKET_URL} "}]}),
        );
        dto.prepare(()).unwrap();
        let repository = &dto.repositories[0];
        assert_eq!(repository.id, "community");
        assert_eq!(repository.name, "Community");
        assert_eq!(repository.url, "${env:TP_MARKET_URL}");
        let json = serde_json::to_string(&dto).unwrap();
        assert_eq!(serde_json::from_str::<PluginsConfigDto>(&json).unwrap(), dto);
    }

    #[test]
    fn repositories_reject_bad_id_duplicates_blank_name_and_url() {
        let message = prepare_message(from_json(json!({"repositories": [
            {"id": "Bad_Id", "name": "A", "url": "https://x.test/m.json"},
            {"id": "dup-repo", "name": "B", "url": "https://x.test/m.json"},
            {"id": "dup-repo", "name": "C", "url": "https://x.test/m.json"},
            {"id": "dup-repo", "name": "D", "url": "https://x.test/m.json"},
            {"id": "blank-one", "name": "  ", "url": " "},
        ]})));
        assert!(message.contains("`Bad_Id` is invalid"), "{message}");
        assert_eq!(message.matches("more than once").count(), 1, "{message}");
        assert!(message.contains("`blank-one` needs a name"), "{message}");
        assert!(message.contains("`blank-one` needs a url"), "{message}");
        assert_eq!(message.lines().count(), 4, "{message}");
    }

    #[test]
    fn plugin_id_shape_matches_the_manifest_regex() {
        for ok in ["abc", "sports-enricher", "a1-", &format!("a{}", "b".repeat(40))] {
            assert!(is_valid_plugin_id(ok), "{ok}");
        }
        for bad in ["", "ab", "1abc", "-abc", "Abc", "ab_c", "ab c", "abc\n", &format!("a{}", "b".repeat(41))] {
            assert!(!is_valid_plugin_id(bad), "{bad:?}");
        }
        assert!(is_kebab_id("ab", 2..=41));
        assert!(!is_kebab_id("a", 2..=41));
    }

    #[test]
    fn a_category_id_is_a_short_kebab_id() {
        for ok in ["ab", "epg", "notifier", "live-tv", "a1", &format!("a{}", "b".repeat(31))] {
            assert!(is_valid_plugin_category(ok), "{ok}");
        }
        for bad in ["", "a", "1ab", "-ab", "Epg", "live_tv", "live tv", &format!("a{}", "b".repeat(32))] {
            assert!(!is_valid_plugin_category(bad), "{bad:?}");
        }
        // The names the Web UI translates are all valid ids.
        assert!(KNOWN_PLUGIN_CATEGORIES.iter().all(|category| is_valid_plugin_category(category)));
    }

    #[test]
    fn a_category_written_loosely_is_normalised_to_its_id() {
        for (raw, want) in [
            ("epg", "epg"),
            ("EPG", "epg"),
            ("  Epg  ", "epg"),
            ("Live TV", "live-tv"),
            ("live_tv", "live-tv"),
            ("live--tv", "live-tv"),
            ("live - tv", "live-tv"),
            ("Live/TV", "live-tv"),
            ("-notifier-", "notifier"),
            ("Home Automation 2", "home-automation-2"),
        ] {
            assert_eq!(normalize_plugin_category(raw).as_deref(), Some(want), "{raw:?}");
        }
        // Nothing valid left: empty, a single character, starts with a digit, or too long.
        for bad in ["", "   ", "---", "a", "1pass", "9", &"x".repeat(33)] {
            assert_eq!(normalize_plugin_category(bad), None, "{bad:?}");
        }
        // The result is always a valid id, and normalising twice changes nothing.
        for raw in ["EPG", "Live TV", "a b c", "Ünïcode Cat"] {
            if let Some(id) = normalize_plugin_category(raw) {
                assert!(is_valid_plugin_category(&id), "{raw:?} -> {id:?}");
                assert_eq!(normalize_plugin_category(&id).as_deref(), Some(id.as_str()));
            }
        }
    }
}
