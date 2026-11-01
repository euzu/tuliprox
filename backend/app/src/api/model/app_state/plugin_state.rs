//! The plugin host the playlist pipeline runs with.

#[cfg(feature = "plugins")]
use super::create_plugin_http_client_no_redirect;
use crate::model::AppConfig;
use arc_swap::ArcSwapOption;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tuliprox_core::utils::LRUResourceCache;

/// Make the plugins that export `send-notification` messaging channels
/// (`Channel::Plugin`, plan §7 *Notification providers*), and drop
/// messaging's cached channel set on every registry reload so an enabled,
/// disabled or reconfigured plugin channel shows up on the next send.
#[cfg(feature = "plugins")]
pub fn register_plugin_notifier(plugins: &AppPlugins, cancel_token: &CancellationToken) {
    let Some(plugins) = plugins.clone() else {
        return;
    };
    let mut changes = plugins.changes();
    tuliprox_messaging::channels::set_plugin_notifier(Some(Arc::new(plugins)));
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel_token.cancelled() => break,
                reloaded = changes.changed() => {
                    if reloaded.is_err() {
                        break;
                    }
                }
            }
            tuliprox_messaging::channels::invalidate();
        }
    });
}

/// Built without the `plugins` feature: there are no plugin channels.
#[cfg(not(feature = "plugins"))]
pub fn register_plugin_notifier(_plugins: &AppPlugins, _cancel_token: &CancellationToken) {}

/// Migrate the stored settings of plugins whose package moved to a newer
/// settings schema (plan §18 *W8 seam freeze*): once now (the boot load has
/// already happened), then after every registry reload, whatever caused it (a
/// config reload, an approval, an update, a directory change). Each pass is a
/// no-op unless some enabled plugin is due, and a failed migration is not
/// retried until the package or the stored version changes.
#[cfg(feature = "plugins")]
pub fn spawn_settings_migration(plugins: &AppPlugins, app_config: &Arc<AppConfig>, cancel_token: &CancellationToken) {
    let Some(system) = plugins.as_ref() else {
        return;
    };
    let mut changes = system.changes();
    let plugins = plugins.clone();
    let app_config = Arc::clone(app_config);
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            crate::api::endpoints::plugins_settings_api::migrate_stored_settings(&plugins, &app_config).await;
            tokio::select! {
                () = cancel_token.cancelled() => break,
                reloaded = changes.changed() => {
                    if reloaded.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// A one-shot (CLI) run: migrate stored settings that are behind their
/// plugin's settings schema for this run only, in memory, and say so. A CLI
/// run must not write `config.yml` (it may share it with a running server),
/// so the migration is persisted by the next server start or settings save.
#[cfg(feature = "plugins")]
pub async fn migrate_settings_for_run(plugins: &AppPlugins, app_config: &AppConfig) {
    let (Some(system), Some(current)) = (plugins.as_ref(), app_config.plugins.load_full()) else {
        return;
    };
    if !system.settings_migration_pending(&current) {
        return;
    }
    let written: std::collections::HashMap<String, serde_json::Map<String, serde_json::Value>> =
        match crate::api::endpoints::plugins_settings_api::written_plugins_config(app_config).await {
            Ok(written) => written.plugins.into_iter().map(|entry| (entry.id, entry.settings)).collect(),
            Err(err) => {
                log::error!("Cannot read config.yml for the plugin settings migration, not migrating: {err}");
                return;
            }
        };
    let migrated = system.migrate_settings_as_written(&current, &written).await;
    if migrated.is_empty() {
        return;
    }
    let mut next = (*current).clone();
    for item in &migrated {
        if let Some(entry) = next.plugins.iter_mut().find(|entry| entry.id == item.id) {
            entry.settings.clone_from(&item.settings);
            entry.settings_version = item.version;
        }
        log::warn!(
            "plugin `{}`: settings migrated to version {} for this run only; config.yml is not changed by a CLI run \
             (the next server start persists the migration)",
            item.id,
            item.version
        );
    }
    app_config.plugins.store(Some(Arc::new(next)));
    reload_plugins(plugins, app_config).await;
}

/// Built without the `plugins` feature: there are no settings to migrate.
#[cfg(not(feature = "plugins"))]
#[allow(clippy::unused_async, reason = "Same signature as the `plugins` build.")]
pub async fn migrate_settings_for_run(_plugins: &AppPlugins, _app_config: &AppConfig) {}

/// Built without the `plugins` feature: there are no settings to migrate.
#[cfg(not(feature = "plugins"))]
pub fn spawn_settings_migration(
    _plugins: &AppPlugins,
    _app_config: &Arc<AppConfig>,
    _cancel_token: &CancellationToken,
) {
}

/// `AppState.cache`: the resource cache `cache-image` stores into. `None`
/// inside means it is disabled.
pub type ResourceCacheHandle = Arc<ArcSwapOption<RwLock<LRUResourceCache>>>;

/// Chosen at startup from `plugins.enabled`. `None` costs one branch per hook
/// site per input; the hook itself never runs.
#[cfg(feature = "plugins")]
pub type AppPlugins = Option<tuliprox_plugins::PluginSystem>;

/// Built without the `plugins` feature: every hook site compiles away.
#[cfg(not(feature = "plugins"))]
pub type AppPlugins = shared::model::NoPlugins;

/// A handle for one processing run. Works in both builds: `PluginSystem` is a
/// cheap `Arc` clone, and `NoPlugins` is `Copy`.
#[allow(clippy::clone_on_copy, reason = "`AppPlugins` is `Copy` only without the `plugins` feature")]
pub fn share_plugins(plugins: &AppPlugins) -> AppPlugins { plugins.clone() }

/// Freezes the plugins' `now-unix` at this Unix time (seconds), so two runs on
/// the same input can be compared byte for byte (plan §7 *Determinism*). A
/// test knob, not configuration: unset in normal operation. Host-side timing
/// (the metrics throttle, header TTLs, log stamps) keeps the wall clock.
#[cfg(feature = "plugins")]
pub const ENV_PLUGIN_FROZEN_CLOCK: &str = "TULIPROX_PLUGIN_FROZEN_CLOCK";

#[cfg(feature = "plugins")]
fn frozen_plugin_clock() -> Option<u64> {
    std::env::var(ENV_PLUGIN_FROZEN_CLOCK).ok().and_then(|value| value.trim().parse().ok())
}

/// Load the configured plugins.
///
/// Compiling components is CPU-bound, so it runs on the blocking pool. A
/// failure is logged and the server runs without plugins: plugins fail open.
/// Plugins get their own HTTP client (no redirects, no resource destination
/// resolver, plan §9); when it cannot be built they stay off rather than fall
/// back to another client.
///
/// `resource_cache` is the server's resource cache; the CLI passes `None`, and
/// `cache-image` then returns `Err`.
#[cfg(feature = "plugins")]
pub async fn load_plugins(app_config: &AppConfig, resource_cache: Option<&ResourceCacheHandle>) -> AppPlugins {
    let config = app_config.plugins.load_full()?;
    let config = (*config).clone();
    let client = match create_plugin_http_client_no_redirect(app_config) {
        Ok(client) => client,
        Err(err) => {
            log::error!("Plugins disabled: {err}");
            return None;
        }
    };
    let (storage_dir, web_ui_path) = {
        let cfg = app_config.config.load();
        let web_ui_path = cfg.web_ui.as_ref().and_then(|web_ui| web_ui.path.clone()).unwrap_or_default();
        (cfg.storage_dir.clone(), web_ui_path)
    };
    let mut services =
        tuliprox_plugins::PluginServices::new(client, std::path::PathBuf::from(storage_dir).join("plugins"));
    if let Some(cache) = resource_cache {
        let secret = app_config.get_encrypt_secret();
        let resource_url = shared::utils::concat_path_leading_slash(&web_ui_path, "api/v1/playlist/resource");
        let seal: tuliprox_plugins::SealResourceUrl = Arc::new(move |url: &str| {
            shared::utils::concat_path(&resource_url, &shared::utils::seal_web_ui_resource_url(&secret, url))
        });
        services = services.with_resource_cache(Arc::clone(cache), seal);
    }
    if let Some(now_secs) = frozen_plugin_clock() {
        log::warn!("Plugin clock frozen at {now_secs} ({ENV_PLUGIN_FROZEN_CLOCK}); for determinism checks only");
        services = services.with_clock(tuliprox_plugins::HostClock::Frozen(tuliprox_core::utils::ManualClock::new(
            now_secs.saturating_mul(1000),
        )));
    }
    match tokio::task::spawn_blocking(move || tuliprox_plugins::PluginSystem::load(&config, services)).await {
        Ok(Ok(plugins)) => plugins,
        Ok(Err(err)) => {
            log::error!("Plugins disabled: {err}");
            None
        }
        Err(err) => {
            log::error!("Plugins disabled, the loader failed: {err}");
            None
        }
    }
}

#[cfg(not(feature = "plugins"))]
#[allow(clippy::unused_async, reason = "Same signature as the `plugins` build.")]
pub async fn load_plugins(_app_config: &AppConfig, _resource_cache: Option<&ResourceCacheHandle>) -> AppPlugins {
    shared::model::NoPlugins
}

/// Apply a changed `plugins:` section (config hot reload) on the blocking pool.
/// Turning plugins on or off needs a restart: the engine exists only when they
/// were enabled at startup.
#[cfg(feature = "plugins")]
pub async fn reload_plugins(plugins: &AppPlugins, app_config: &AppConfig) {
    let config = app_config.plugins.load_full();
    let enabled = config.as_ref().is_some_and(|config| config.enabled);
    match (plugins, config) {
        (Some(system), Some(config)) if enabled => {
            let system = system.clone();
            if let Err(err) = tokio::task::spawn_blocking(move || system.reload(&config)).await {
                log::error!("Plugin reload failed: {err}");
            }
        }
        (Some(_), _) => log::warn!("plugins were disabled in the config; this takes effect after a restart"),
        (None, _) if enabled => log::warn!("plugins were enabled in the config; this takes effect after a restart"),
        (None, _) => {}
    }
}

#[cfg(not(feature = "plugins"))]
#[allow(clippy::unused_async, reason = "Same signature as the `plugins` build.")]
pub async fn reload_plugins(_plugins: &AppPlugins, _app_config: &AppConfig) {}

/// [`load_plugins`], then [`validate_plugin_refs`] against the loaded sources:
/// at startup an unknown or disabled plugin reference is a config error.
pub async fn load_validated_plugins(
    app_config: &AppConfig,
    resource_cache: Option<&ResourceCacheHandle>,
) -> Result<AppPlugins, shared::error::TuliproxError> {
    let plugins = load_plugins(app_config, resource_cache).await;
    validate_plugin_refs(&plugins, &arc_swap::ArcSwapAny::load(app_config.sources.as_ref()))?;
    validate_plugin_schedules(&plugins, app_config.config.load().schedules.as_deref().unwrap_or_default())?;
    Ok(plugins)
}

/// Checks the `type: plugin_task` schedules (plan §7 *Scheduled plugin
/// tasks*): such an entry names both `plugin` and `task`, the fields appear on
/// no other type, and, with plugins on, the plugin is installed and declares
/// the task. With plugins off only the shape is checked; the scheduler skips
/// plugin tasks then.
pub fn validate_plugin_schedules(
    plugins: &AppPlugins,
    schedules: &[tuliprox_core::model::ScheduleConfig],
) -> Result<(), shared::error::TuliproxError> {
    use shared::model::ScheduleTaskType;
    let mut problems = Vec::new();
    for schedule in schedules {
        let is_task = schedule.task_type == ScheduleTaskType::PluginTask;
        match (is_task, schedule.plugin.as_deref(), schedule.task.as_deref()) {
            (true, Some(plugin), Some(task)) => {
                #[cfg(feature = "plugins")]
                if let Some(system) = plugins {
                    if system.task_respects_maintenance(plugin, task).is_none() {
                        problems.push(format!(
                            "schedule `{}`: plugin `{plugin}` is not installed or declares no task `{task}`",
                            schedule.schedule
                        ));
                    }
                }
                #[cfg(not(feature = "plugins"))]
                let _ = (plugin, task, plugins);
            }
            (true, _, _) => problems
                .push(format!("schedule `{}`: `type: plugin_task` needs both `plugin` and `task`", schedule.schedule)),
            (false, None, None) => {}
            (false, _, _) => problems.push(format!(
                "schedule `{}`: `plugin` and `task` are only valid with `type: plugin_task`",
                schedule.schedule
            )),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(shared::error::TuliproxError::Plugin(format!("invalid plugin schedules: {}", problems.join("; "))))
    }
}

/// Checks every target's plugin references against the loaded plugins (plan
/// §18 *Validation timing*): the strategy refs (`plugin:<id>`) and the
/// `plugin.<id>.<fn>(…)` terms of the processing filter must name a loaded
/// plugin that provides the hook. Plugin terms anywhere else (persist or
/// output filters) are rejected: those filters evaluate without plugins, so the
/// term would always be `false`. The function name is not checked; the
/// manifest does not declare it.
///
/// With plugins disabled nothing provides anything, so any reference fails.
pub fn validate_plugin_refs<P: shared::model::PluginHost>(
    plugins: &P,
    sources: &tuliprox_core::model::SourcesConfig,
) -> Result<(), shared::error::TuliproxError> {
    use shared::model::PluginHook;
    let mut problems = Vec::new();
    // `type: plugin` inputs (plan §18 Q6): with plugins off nothing provides
    // `fetch-input`, so they are rejected here.
    for input in &sources.inputs {
        if let Some((hook, plugin)) = input.plugin_ref() {
            if !plugins.provides(hook, plugin) {
                problems.push(format!("input {}: no loaded plugin {plugin} provides {}", input.name, hook.as_str()));
            }
        }
    }
    for target in sources.sources.iter().flat_map(|source| source.targets.iter()) {
        for (hook, plugin) in target.plugin_refs() {
            if !plugins.provides(hook, plugin) {
                problems.push(format!("target {}: no loaded plugin {plugin} provides {}", target.name, hook.as_str()));
            }
        }
        for output in target.plugin_outputs() {
            if let (Some(format), Some(provided)) = (output.format.as_deref(), plugins.output_format(&output.plugin)) {
                if provided != format {
                    problems.push(format!(
                        "target {}: plugin {} renders `{provided}`, not `{format}`",
                        target.name, output.plugin
                    ));
                }
            }
        }
        if let Some(filter) = target.filter.processing.as_ref() {
            for call in filter.plugin_calls() {
                if !plugins.provides(PluginHook::FilterEval, call.plugin) {
                    problems.push(format!(
                        "target {}: filter term plugin.{}.{} names no loaded filter plugin",
                        target.name, call.plugin, call.func
                    ));
                }
            }
        }
        let other_filters =
            target.filter.persist.iter().chain(target.output.iter().filter_map(|output| output.filter()));
        for filter in other_filters {
            if let Some(call) = filter.plugin_calls().next() {
                problems.push(format!(
                    "target {}: plugin.{}.{} is only allowed in the processing filter",
                    target.name, call.plugin, call.func
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(shared::error::TuliproxError::ConfigSource(format!("invalid plugin references: {}", problems.join("; "))))
    }
}
