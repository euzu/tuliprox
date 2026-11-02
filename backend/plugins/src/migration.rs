//! Settings migration.
//!
//! A plugin's settings schema carries a `version`; the stored settings carry
//! the version they were written for (`plugins.plugins[].settings_version`).
//! When an update raises the schema version past the stored one, the plugin
//! migrates the stored settings through its optional `settings.migrate` export.
//!
//! The registry never calls a guest while it scans (rule 0), so the migration is
//! its own async step: the app calls [`PluginSystem::migrate_settings`] after a
//! registry reload and persists what it returns through the same `config.yml`
//! write as `PUT …/settings`. Nothing here writes a file.
//!
//! A migration that cannot run (no export), fails, or returns settings the
//! schema rejects keeps the stored settings untouched and leaves a
//! `settings_review` (see [`crate::registry::Registry::settings_review`]).

use crate::{
    api::{check_extension_settings, read_settings_schema, secret_paths_of},
    host_fns,
    manifest::{Capabilities, Manifest},
    registry::{export_names, interface_name, sha256_hex, MigrationFailure, PluginRecord},
    runtime::{call_with_deadline, PluginLimits, PreparedPlugin},
    settings::{
        check_settings, redact, reference_pairs, restore_references, schema_version, secret_values, version_conflict,
    },
    system::PluginSystem,
};
use serde_json::{Map, Value};
use shared::model::{PluginUpdatePolicy, DEFAULT_PLUGIN_SETTINGS_VERSION};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugin_api::bindings::exports::tuliprox::plugin::settings_migration;

/// The settings of one plugin after a successful migration, ready to persist:
/// `settings` are validated against the package's schema and written with
/// `settings_version = version`.
#[derive(Debug, Clone, PartialEq)]
pub struct MigratedSettings {
    pub id: String,
    pub settings: Map<String, Value>,
    pub version: u32,
}

impl PluginSystem {
    /// Whether some enabled plugin has stored settings behind its package's
    /// schema version and a migration that has not failed yet. Cheap: the app
    /// asks it after every reload before it takes the config locks.
    #[must_use]
    pub fn settings_migration_pending(&self, config: &PluginsConfig) -> bool {
        let snapshot = self.registry().snapshot();
        snapshot.plugins.iter().any(|record| self.migration_due(record, config).is_some())
    }

    /// The schema `version` of plugin `id`'s package (what `PUT …/settings`
    /// writes as `settings_version`).
    #[must_use]
    pub fn settings_schema_version(&self, id: &str) -> Option<u32> {
        self.registry().snapshot().get(id).map(|record| record.settings_version)
    }

    /// Migrate the stored settings of every plugin that is due, one guest call
    /// each, and return the ones that succeeded. A failure is logged and
    /// remembered (it shows as `settings_review`, and is not retried until the
    /// package or the stored version changes); the stored settings stay as they
    /// are. Nothing is persisted here: the caller writes `config.yml` under the
    /// config lock and reloads.
    ///
    /// `config`'s settings must be as written (`${env:VAR}` unresolved); a
    /// config read with env resolution needs [`PluginSystem::migrate_settings_as_written`].
    pub async fn migrate_settings(&self, config: &PluginsConfig) -> Vec<MigratedSettings> {
        self.migrate_settings_as_written(config, &HashMap::new()).await
    }

    /// [`PluginSystem::migrate_settings`] for a `config` whose settings may be
    /// env-resolved (the server reads `config.yml` with env resolution):
    /// `written` holds each plugin's settings as `config.yml` has them, so
    /// the `${env:VAR}` references they hold are put back into the migrated
    /// settings and no resolved secret is persisted.
    pub async fn migrate_settings_as_written(
        &self,
        config: &PluginsConfig,
        written: &HashMap<String, Map<String, Value>>,
    ) -> Vec<MigratedSettings> {
        let snapshot = self.registry().snapshot();
        let mut migrated = Vec::new();
        for record in &snapshot.plugins {
            let Some(entry) = self.migration_due(record, config) else {
                continue;
            };
            match self.run_migration(record, entry, written.get(&*record.id)).await {
                Ok(settings) => {
                    log::info!(
                        "plugin `{}`: settings migrated from version {} to {}",
                        record.id,
                        entry.settings_version,
                        record.settings_version
                    );
                    migrated.push(MigratedSettings {
                        id: record.id.to_string(),
                        settings,
                        version: record.settings_version,
                    });
                }
                Err(reason) => {
                    log::warn!(
                        "plugin `{}`: migrating the settings from version {} to {} failed, the stored settings are \
                         kept: {reason}",
                        record.id,
                        entry.settings_version,
                        record.settings_version
                    );
                    self.registry().note_migration_failure(
                        &record.id,
                        MigrationFailure {
                            from: entry.settings_version,
                            to: record.settings_version,
                            wasm_sha256: record.wasm_sha256.clone(),
                            reason,
                        },
                    );
                }
            }
        }
        migrated
    }

    /// The entry of `record` when its settings are due for a migration: enabled,
    /// loaded with a `settings-migration` export, stored behind the package's
    /// schema, and no failed attempt for exactly this pair of versions.
    fn migration_due<'a>(&self, record: &PluginRecord, config: &'a PluginsConfig) -> Option<&'a PluginEntry> {
        let entry = config.entry(&record.id).filter(|entry| entry.enabled)?;
        let exports = record.loaded.as_ref().is_some_and(|loaded| loaded.settings_migration.is_some());
        (exports
            && entry.settings_version < record.settings_version
            && self.registry().migration_failure(record, entry.settings_version).is_none())
        .then_some(entry)
    }

    /// The settings `record` runs on under `entry`, for the activation check:
    /// the stored ones at the package's schema version, the migrated ones when
    /// they are behind and the package can migrate them, else the stored ones if
    /// they still fit the schema. Nothing is persisted.
    ///
    /// # Errors
    /// The settings neither migrate nor fit the installed schema, so the plugin
    /// would not run.
    pub(crate) async fn effective_settings(
        &self,
        record: &PluginRecord,
        entry: Option<&PluginEntry>,
    ) -> Result<Map<String, Value>, String> {
        let Some(entry) = entry else {
            return Ok(Map::new());
        };
        let can_migrate = record.loaded.as_ref().is_some_and(|loaded| loaded.settings_migration.is_some());
        if entry.settings_version < record.settings_version && can_migrate {
            match self.run_migration(record, entry, None).await {
                Ok(settings) => return Ok(settings),
                Err(reason) => log::warn!(
                    "plugin `{}`: the settings migration failed, checking the stored settings instead: {reason}",
                    record.id
                ),
            }
        }
        let schema = read_settings_schema_of(record);
        match version_conflict(schema.as_ref(), record.settings_version, entry.settings_version, &entry.settings) {
            Some(conflict) => Err(conflict),
            None => Ok(entry.settings.clone()),
        }
    }

    /// Call the `settings.migrate` export of `record` on a fresh instance with
    /// the stored settings of `entry`, and validate what it returns.
    ///
    /// The guest sees the `${env:VAR}`-resolved settings (the contract), and
    /// `init` does not run first: the old settings may not fit what the new
    /// code's `init` expects. The references are put back into the result, so a
    /// resolved secret is never written to `config.yml`.
    /// `written` is the entry's settings as `config.yml` has them, when the
    /// caller's config holds them env-resolved; `None` means `entry` is as written.
    async fn run_migration(
        &self,
        record: &PluginRecord,
        entry: &PluginEntry,
        written: Option<&Map<String, Value>>,
    ) -> Result<Map<String, Value>, String> {
        let loaded = record.loaded.as_ref().ok_or("the plugin is not loaded")?;
        let guest_indices = loaded.settings_migration.as_ref().ok_or("the plugin exports no settings migration")?;
        let paths = secret_paths_of(record);
        let settings = self
            .call_migration(
                &record.id,
                loaded.env.capabilities,
                &loaded.prepared,
                guest_indices,
                entry,
                written,
                &paths,
            )
            .await?;
        validate_migrated(record, &settings)?;
        Ok(settings)
    }

    /// The guest call of [`PluginSystem::run_migration`], on whatever
    /// `prepared` component and `guest_indices` the caller bound: a fresh
    /// instance, the stored settings of `entry` in, the settings JSON object
    /// out (references restored, **not** yet validated).
    #[allow(clippy::too_many_arguments)]
    async fn call_migration(
        &self,
        id: &Arc<str>,
        capabilities: Capabilities,
        prepared: &PreparedPlugin,
        guest_indices: &settings_migration::GuestIndices,
        entry: &PluginEntry,
        written: Option<&Map<String, Value>>,
        secret_paths: &[String],
    ) -> Result<Map<String, Value>, String> {
        let stored = Value::Object(written.unwrap_or(&entry.settings).clone());
        let candidate = PluginsConfig { plugins: vec![entry.clone()], ..PluginsConfig::default() };
        let resolved = candidate.resolved_settings(id).unwrap_or_else(|| Value::Object(Map::new()));
        let secrets = secret_values(&resolved, secret_paths);
        let pairs = reference_pairs(&stored, &resolved);
        let settings_json = resolved.to_string();
        let env = self
            .shared()
            .env_for(id, capabilities, Arc::from(settings_json.as_str()), Arc::from(secrets.clone()))
            .map_err(|err| redact(err.message(), &secrets))?;

        let mut live =
            self.runtime().instantiate(prepared, &env).await.map_err(|err| redact(&err.to_string(), &secrets))?;
        let instance = live.instance();
        let guest = guest_indices
            .load(live.store_mut(), &instance)
            .map_err(|err| redact(&format!("cannot bind the settings-migration export: {err:#}"), &secrets))?;
        let called =
            call_with_deadline(live.limits(), guest.call_migrate(live.armed(), entry.settings_version, &settings_json))
                .await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        let output = called
            .map_err(|err| redact(&err.to_string(), &secrets))?
            .map_err(|message| format!("migrate returned err: {}", redact(&message, &secrets)))?;

        let mut value: Value =
            serde_json::from_str(&output).map_err(|_| "migrate returned something that is not JSON".to_string())?;
        restore_references(&mut value, &pairs);
        let Value::Object(settings) = value else {
            return Err("migrate must return a JSON object".to_string());
        };
        Ok(settings)
    }

    /// What installing a **staged** package would do to the stored settings of
    /// `entry` (`None`: no config entry, version 1 and no settings), without
    /// changing anything: the package's `settings.migrate` runs on a fresh
    /// instance of the staged component, and its output is validated against
    /// the staged schema. Nothing is persisted, the registry is not touched and
    /// no failure is remembered ([`Registry::note_migration_failure`] is never
    /// called), so it can be repeated freely. `entry` must hold the settings as
    /// written (its `${env:VAR}` references are resolved for the guest only).
    ///
    /// `dir` is the staged package directory and `wasm` the bytes of its
    /// `plugin.wasm` (`None` for a UI-only package). The staged component is
    /// compiled into the runtime's cache; the next reload drops it again.
    pub(crate) async fn dry_run_settings_migration(
        &self,
        dir: PathBuf,
        manifest: Arc<Manifest>,
        wasm: Option<Vec<u8>>,
        entry: Option<&PluginEntry>,
    ) -> SettingsDryRun {
        let from = entry.map_or(DEFAULT_PLUGIN_SETTINGS_VERSION, |entry| entry.settings_version);
        let system = self.clone();
        // Reads the schema files and compiles the component: blocking work.
        let prepared = tokio::task::spawn_blocking(move || system.prepare_staged_migration(dir, &manifest, wasm, from))
            .await
            .unwrap_or_else(|err| Err(SettingsDryRun::Failed { from, to: from, reason: err.to_string() }));
        let staged = match prepared {
            Ok(staged) => staged,
            Err(outcome) => return outcome,
        };
        let to = staged.record.settings_version;
        let default_entry;
        let entry = if let Some(entry) = entry {
            entry
        } else {
            default_entry = PluginEntry {
                id: staged.record.id.to_string(),
                enabled: false,
                priority: 0,
                settings: Map::new(),
                settings_version: from,
                update_policy: PluginUpdatePolicy::default(),
            };
            &default_entry
        };
        let result = self
            .call_migration(
                &staged.record.id,
                staged.capabilities,
                &staged.prepared,
                &staged.guest,
                entry,
                None,
                &staged.secret_paths,
            )
            .await
            .and_then(|settings| validate_migrated(&staged.record, &settings));
        match result {
            Ok(()) => SettingsDryRun::Migrates { from, to },
            Err(reason) => SettingsDryRun::Failed { from, to, reason },
        }
    }

    /// The blocking half of [`PluginSystem::dry_run_settings_migration`]: the
    /// schema version, and, when the stored version is behind, the compiled
    /// component with its `settings-migration` binding. `Err` is the final
    /// verdict when no guest call is needed (or possible).
    fn prepare_staged_migration(
        &self,
        dir: PathBuf,
        manifest: &Arc<Manifest>,
        wasm: Option<Vec<u8>>,
        from: u32,
    ) -> Result<StagedMigration, SettingsDryRun> {
        let mut record = PluginRecord::new(&manifest.id, dir, 0);
        record.manifest = Some(Arc::clone(manifest));
        record.settings_version = read_settings_schema_of(&record)
            .and_then(|schema| schema_version(&schema).ok())
            .unwrap_or(DEFAULT_PLUGIN_SETTINGS_VERSION);
        let to = record.settings_version;
        if to == from {
            return Err(SettingsDryRun::Unchanged);
        }
        if to < from {
            return Err(SettingsDryRun::Downgrade { from, to });
        }
        let Some(wasm) = wasm else {
            return Err(SettingsDryRun::Missing { from, to });
        };
        let failed = |reason: String| SettingsDryRun::Failed { from, to, reason };
        let runtime = self.runtime();
        let limits = PluginLimits::clamp(&manifest.resources, runtime.config());
        let (component, prepared) = runtime
            .prepare_cached(&sha256_hex(&wasm), &wasm, limits)
            .map_err(|err| failed(err.message().to_string()))?;
        let (exports, _unknown) = export_names(runtime.engine(), &component);
        if !exports.contains(&interface_name("settings-migration")) {
            return Err(SettingsDryRun::Missing { from, to });
        }
        let guest = settings_migration::GuestIndices::new(prepared.pre())
            .map_err(|err| failed(format!("cannot bind the settings-migration export: {err:#}")))?;
        let secret_paths = secret_paths_of(&record);
        Ok(StagedMigration { record, capabilities: manifest.capabilities, prepared, guest, secret_paths })
    }
}

/// A staged package opened far enough to call its `settings.migrate`.
struct StagedMigration {
    /// Carries the staged manifest, directory and schema version for the
    /// validation; never registered anywhere.
    record: PluginRecord,
    capabilities: Capabilities,
    prepared: PreparedPlugin,
    guest: settings_migration::GuestIndices,
    secret_paths: Vec<String>,
}

/// What [`PluginSystem::dry_run_settings_migration`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettingsDryRun {
    /// The staged schema version equals the stored one; nothing to migrate.
    Unchanged,
    /// The package migrates the stored settings to a valid result.
    Migrates { from: u32, to: u32 },
    /// The staged schema version is below the stored one.
    Downgrade { from: u32, to: u32 },
    /// The version moves up and the package has no `settings.migrate`.
    Missing { from: u32, to: u32 },
    /// The migration failed, or its output does not fit the staged schema.
    Failed { from: u32, to: u32, reason: String },
}

fn read_settings_schema_of(record: &PluginRecord) -> Option<Value> {
    record.manifest.as_deref().and_then(|manifest| read_settings_schema(&record.dir, manifest))
}

/// The migrated settings must pass what a `PUT …/settings` would check: the
/// plugin's schema over everything but `ext`, and the extension fragments.
fn validate_migrated(record: &PluginRecord, settings: &Map<String, Value>) -> Result<(), String> {
    let schema = read_settings_schema_of(record);
    let mut own = settings.clone();
    own.remove(crate::manifest::package::EXTENSION_KEY);
    check_settings(schema.as_ref(), &Value::Object(own)).map_err(|error| {
        let fields: Vec<String> =
            error.fields.iter().take(3).map(|field| format!("{}: {}", field.path, field.message)).collect();
        if fields.is_empty() {
            format!("the migrated settings are rejected: {}", error.error)
        } else {
            format!("the migrated settings do not match the schema: {}", fields.join(", "))
        }
    })?;
    check_extension_settings(record, settings)
        .map_err(|(_, error)| format!("the migrated settings are rejected: {}", error.error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PluginServices;
    use serde_json::json;
    use std::path::PathBuf;

    const ID: &str = "settings-migrate-probe";

    /// The built `settings-migrate-probe` fixture loaded with one entry, or
    /// `None` (after saying so) when it is not built.
    fn load(settings: &Value, version: u32) -> Option<(PluginSystem, PluginsConfig, tempfile::TempDir)> {
        let directory = std::env::var_os("TULIPROX_PLUGIN_FIXTURES").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixtures/plugins"),
            Into::into,
        );
        if !directory.join(ID).join("plugin.wasm").is_file() {
            eprintln!("skipped: fixture not built; run bin/build_plugin_fixture.sh");
            return None;
        }
        let data = tempfile::tempdir().unwrap();
        let entry = PluginEntry {
            id: ID.to_string(),
            enabled: true,
            priority: 0,
            settings: settings.as_object().cloned().unwrap_or_default(),
            settings_version: version,
            update_policy: shared::model::PluginUpdatePolicy::Auto,
        };
        let config = PluginsConfig {
            enabled: true,
            directory: directory.to_string_lossy().to_string(),
            plugins: vec![entry],
            ..PluginsConfig::default()
        };
        let system =
            PluginSystem::load(&config, PluginServices::new(reqwest::Client::new(), data.path().to_path_buf()))
                .unwrap()
                .unwrap();
        Some((system, config, data))
    }

    #[tokio::test]
    async fn the_activation_check_runs_init_on_the_migrated_settings() {
        // the v1 shape does not fit the v2 schema, and the plugin's `init` parses
        // only the v2 shape: the check passes because it migrates in memory
        let Some((system, config, _data)) = load(&json!({"label": "News"}), 1) else { return };
        assert_eq!(system.activation_check(ID, &config).await, Ok(()));
        // nothing was persisted or remembered by the check
        assert!(system.settings_migration_pending(&config));
    }

    #[tokio::test]
    async fn the_activation_check_accepts_old_settings_that_still_fit() {
        let Some((system, config, _data)) = load(&json!({"group": "Old"}), 1) else { return };
        assert_eq!(system.activation_check(ID, &config).await, Ok(()));
    }

    #[tokio::test]
    async fn the_activation_check_fails_when_neither_migrating_nor_the_old_settings_work() {
        let Some((system, config, _data)) = load(&json!({"label": "fail"}), 1) else { return };
        let error = system.activation_check(ID, &config).await.unwrap_err();
        assert!(error.contains("do not match the settings schema"), "{error}");
    }

    #[tokio::test]
    async fn the_activation_check_fails_for_an_unknown_plugin() {
        let Some((system, config, _data)) = load(&json!({}), 2) else { return };
        let error = system.activation_check("no-such-plugin", &config).await.unwrap_err();
        assert!(error.contains("not installed"), "{error}");
    }
}
