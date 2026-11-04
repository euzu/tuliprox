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
/// (`Channel::Plugin`), and drop
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
/// settings schema: once now (the boot load has
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
/// the same input can be compared byte for byte. A
/// test knob, not configuration: unset in normal operation. Host-side timing
/// (the metrics throttle, header TTLs, log stamps) keeps the wall clock.
#[cfg(feature = "plugins")]
pub const ENV_PLUGIN_FROZEN_CLOCK: &str = "TULIPROX_PLUGIN_FROZEN_CLOCK";

#[cfg(feature = "plugins")]
fn frozen_plugin_clock() -> Option<u64> {
    std::env::var(ENV_PLUGIN_FROZEN_CLOCK).ok().and_then(|value| value.trim().parse().ok())
}

/// Say loudly that the activation crash knob is set: the process aborts in
/// the middle of the next plugin update.
#[cfg(feature = "plugins")]
fn warn_crash_knob() {
    let Ok(value) = std::env::var(tuliprox_plugins::ENV_PLUGIN_TEST_CRASH_AT) else {
        return;
    };
    match tuliprox_plugins::crash_knob() {
        Some(stage) => log::warn!(
            "{}={stage}: the server aborts at this stage of the next plugin activation; for crash tests only",
            tuliprox_plugins::ENV_PLUGIN_TEST_CRASH_AT
        ),
        None => log::warn!(
            "{}={value:?} names no activation stage and is ignored; stages: {}",
            tuliprox_plugins::ENV_PLUGIN_TEST_CRASH_AT,
            tuliprox_plugins::ActivationStage::ALL.map(tuliprox_plugins::ActivationStage::as_str).join(", ")
        ),
    }
}

/// Load the configured plugins.
///
/// Compiling components is CPU-bound, so it runs on the blocking pool. A
/// failure is logged and the server runs without plugins: plugins fail open.
/// Plugins get their own HTTP client (no redirects, no resource destination
/// resolver); when it cannot be built they stay off rather than fall
/// back to another client. Before the scan, activation journals left by a
/// kill are recovered ([`tuliprox_plugins::recover_activations`]).
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
    let data_dir = std::path::PathBuf::from(storage_dir).join("plugins");
    let mut services = tuliprox_plugins::PluginServices::new(client, data_dir.clone());
    if let Some(cache) = resource_cache {
        let secret = app_config.get_encrypt_secret();
        let resource_url = shared::utils::concat_path_leading_slash(&web_ui_path, "api/v1/playlist/resource");
        let seal: tuliprox_plugins::SealResourceUrl = Arc::new(move |url: &str| {
            shared::utils::concat_path(&resource_url, &shared::utils::seal_web_ui_resource_url(&secret, url))
        });
        services = services.with_resource_cache(Arc::clone(cache), seal);
    }
    if tuliprox_core::model::marketplace_allow_http() {
        log::warn!(
            "{}=1: plugin marketplace catalogs and packages may use http:// and local destinations; for local testing only",
            shared::model::ENV_PLUGIN_MARKETPLACE_ALLOW_HTTP
        );
    }
    if let Some(now_secs) = frozen_plugin_clock() {
        log::warn!("Plugin clock frozen at {now_secs} ({ENV_PLUGIN_FROZEN_CLOCK}); for determinism checks only");
        services = services.with_clock(tuliprox_plugins::HostClock::Frozen(tuliprox_core::utils::ManualClock::new(
            now_secs.saturating_mul(1000),
        )));
    }
    warn_crash_knob();
    let loaded = tokio::task::spawn_blocking(move || {
        if config.enabled {
            // Before the first scan: an activation a kill interrupted is settled
            // first, so the scan sees a consistent package and pin.
            let recovered = tuliprox_plugins::recover_activations(&data_dir, std::path::Path::new(&config.directory));
            tuliprox_plugins::record_recovered(&data_dir, &recovered);
        }
        tuliprox_plugins::PluginSystem::load(&config, services)
    });
    match loaded.await {
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

/// [`load_plugins`], then report every plugin reference the loaded plugins cannot
/// satisfy.
///
/// Startup never fails for one. A plugin can be uninstalled, disabled or simply
/// not installed yet while a `source.yml` entry still names it, and that must not
/// keep the server down: the reference is logged and then ignored, and it fails on
/// its own at the point of use (the input's fetch, the output's render, the
/// scheduled task's run), so everything else keeps working. An explicit config
/// save still rejects such a reference — `AppState::set_sources` — because that is
/// an operator action with immediate feedback and a file left unmodified.
pub async fn load_validated_plugins(
    app_config: &AppConfig,
    resource_cache: Option<&ResourceCacheHandle>,
) -> AppPlugins {
    let plugins = load_plugins(app_config, resource_cache).await;
    report_unsatisfied_plugin_refs(
        &plugins,
        app_config.sources.load().as_ref(),
        app_config.config.load().schedules.as_deref().unwrap_or_default(),
    );
    plugins
}

/// Logs every plugin reference the loaded plugins cannot satisfy, one line per set
/// of problems. Never fails: see [`load_validated_plugins`].
pub fn report_unsatisfied_plugin_refs(
    plugins: &AppPlugins,
    sources: &tuliprox_core::model::SourcesConfig,
    schedules: &[tuliprox_core::model::ScheduleConfig],
) {
    if let Err(err) = validate_plugin_refs(plugins, sources) {
        log::error!(
            "{err}. Every input, target and output that names a missing plugin is inactive; everything else runs."
        );
    }
    if let Err(err) = validate_plugin_schedules(plugins, schedules) {
        log::error!("{err}. The affected plugin tasks do not run.");
    }
}

/// Checks the `type: plugin_task` schedules: such an entry names both `plugin` and `task`, the fields appear on
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
        // The scheduler skips a plugin task whose expression does not parse (it never ends the process for
        // one), so the operator hears about it here.
        if is_task {
            if let Err(err) = <cron::Schedule as std::str::FromStr>::from_str(&schedule.schedule) {
                problems.push(format!("schedule `{}`: invalid cron expression: {err}", schedule.schedule));
            }
        }
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

/// Checks every target's plugin references against the loaded plugins: the strategy refs (`plugin:<id>`) and the
/// `plugin.<id>.<fn>(…)` terms of the processing filter must name a loaded
/// plugin that provides the hook. The function name is not checked; the
/// manifest does not declare it.
///
/// With plugins disabled nothing provides anything, so any reference fails.
pub fn validate_plugin_refs<P: shared::model::PluginHost>(
    plugins: &P,
    sources: &tuliprox_core::model::SourcesConfig,
) -> Result<(), shared::error::TuliproxError> {
    use shared::model::PluginHook;
    let mut problems = Vec::new();
    // `type: plugin` inputs: with plugins off nothing provides
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
            if let Some(format) = output.format.as_deref() {
                match plugins.output_format(&output.plugin) {
                    Some(provided) => {
                        if provided != format {
                            problems.push(format!(
                                "target {}: plugin {} renders `{provided}`, not `{format}`",
                                target.name, output.plugin
                            ));
                        }
                    }
                    None => {
                        if plugins.provides(PluginHook::RenderOutput, &output.plugin) {
                            problems.push(format!(
                                "target {}: plugin {} declares no output format, but output asks for `{format}`",
                                target.name, output.plugin
                            ));
                        }
                    }
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
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(shared::error::TuliproxError::ConfigSource(format!("invalid plugin references: {}", problems.join("; "))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{PluginHook, PluginHost};
    use tuliprox_core::model::{ConfigSource, ConfigTarget, PluginTargetOutput, SourcesConfig, TargetOutput};

    #[derive(Clone)]
    struct TestHost {
        output_format: Option<String>,
    }

    impl PluginHost for TestHost {
        fn provides(&self, hook: PluginHook, plugin: &str) -> bool {
            hook == PluginHook::RenderOutput && plugin == "custom-out"
        }

        fn output_format(&self, plugin: &str) -> Option<String> {
            if plugin == "custom-out" {
                self.output_format.clone()
            } else {
                None
            }
        }
    }

    fn sources_with_output(format: Option<&str>) -> SourcesConfig {
        let mut target = ConfigTarget::from(&shared::model::ConfigTargetDto::default());
        target.name = "t1".into();
        target.output = vec![TargetOutput::Plugin(PluginTargetOutput {
            plugin: "custom-out".into(),
            format: format.map(String::from),
            filename: None,
            plugin_config: serde_json::Map::new(),
            filter: None,
        })];
        SourcesConfig {
            sources: vec![ConfigSource { inputs: vec![], targets: vec![target.into()] }],
            ..SourcesConfig::default()
        }
    }

    fn plugin_task_schedule(expression: &str) -> tuliprox_core::model::ScheduleConfig {
        tuliprox_core::model::ScheduleConfig {
            schedule: expression.to_string(),
            task_type: shared::model::ScheduleTaskType::PluginTask,
            targets: None,
            plugin: Some("sports".to_string()),
            task: Some("refresh".to_string()),
        }
    }

    #[test]
    fn validate_plugin_schedules_rejects_an_invalid_cron_expression_with_a_clear_message() {
        // No plugin host: only the shape and the expression are checked.
        let plugins = AppPlugins::default();
        let err = validate_plugin_schedules(&plugins, &[plugin_task_schedule("every day at noon")])
            .expect_err("an unparsable expression is a config error");
        let message = err.to_string();
        assert!(message.contains("schedule `every day at noon`: invalid cron expression"), "{message}");
        validate_plugin_schedules(&plugins, &[plugin_task_schedule("0 0 4 * * * *")]).expect("a valid expression");
    }

    /// Starting must survive a reference to a plugin that is gone. The problems are still found —
    /// `validate_plugin_refs` reports them — but the boot path only logs them, so the server comes up
    /// and every other input and target keeps running.
    #[test]
    fn a_missing_plugin_never_keeps_the_server_down() {
        let plugins = AppPlugins::default();
        let sources = SourcesConfig {
            inputs: vec![std::sync::Arc::new(tuliprox_core::model::ConfigInput {
                name: "youtbepl".into(),
                input_type: shared::model::InputType::Plugin,
                plugin: Some("youtube-input".into()),
                // A disabled input never runs, so it needs no plugin.
                enabled: true,
                ..tuliprox_core::model::ConfigInput::default()
            })],
            ..SourcesConfig::default()
        };
        let schedules = [plugin_task_schedule("0 0 4 * * * *")];

        // Still detected, so the log names what is ignored. (The schedule check needs a loaded
        // plugin system to know which tasks exist; without one it checks the shape only.)
        assert!(validate_plugin_refs(&plugins, &sources).is_err());

        // And reported without failing: the boot path calls exactly this and returns the plugins.
        report_unsatisfied_plugin_refs(&plugins, &sources, &schedules);
    }

    #[test]
    fn validate_plugin_refs_rejects_target_format_when_plugin_declares_none() {
        let host = TestHost { output_format: None };
        let sources = sources_with_output(Some("csv"));
        let err = validate_plugin_refs(&host, &sources).expect_err("should reject format when plugin declares none");
        assert!(err.to_string().contains("target t1: plugin custom-out"));
    }

    #[test]
    fn validate_plugin_refs_allows_target_to_omit_format() {
        let host = TestHost { output_format: Some("tsv".into()) };
        let sources = sources_with_output(None);
        assert!(validate_plugin_refs(&host, &sources).is_ok());
    }

    #[test]
    fn validate_plugin_refs_rejects_mismatched_target_format() {
        let host = TestHost { output_format: Some("tsv".into()) };
        let sources = sources_with_output(Some("csv"));
        let err = validate_plugin_refs(&host, &sources).expect_err("should reject mismatched format");
        assert!(err.to_string().contains("plugin custom-out renders `tsv`, not `csv`"));
    }
}
