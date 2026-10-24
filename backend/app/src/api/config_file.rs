use crate::{
    api::{
        api_utils::failover_group_hot_apply::try_hot_apply,
        model::{update_app_state_config, update_app_state_sources, AppState, EventMessage},
    },
    config_loader::{prepare_sources_batch, read_sources_file_with_revision, resolve_failover_source},
    model::{CompiledMappings, Config, ProcessTargets, SourcesConfig},
    utils,
    utils::read_mappings_file_with_templates,
};
use log::{debug, error, info, warn};
use shared::{
    error::TuliproxError,
    model::{ConfigPaths, ConfigType, PatternTemplate},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{model::FailoverOverrides, utils::FailoverSource};
use tuliprox_repository::{load_failover_snapshot, reload_failover_snapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigFile {
    Config,
    ApiProxy,
    Mapping,
    Template,
    Sources,
    SourceFile,
    Failover,
}

#[derive(Debug)]
struct PreparedMappingsReload {
    mapping_file_path: String,
    mapping_files: Vec<PathBuf>,
    mappings: CompiledMappings,
}

/// Fully prepared sources + mappings data, ready to apply without any I/O or fallible work.
struct PreparedSourcesReload {
    sources: SourcesConfig,
    mapping: Option<PreparedMappingsReload>,
    sources_file: String,
    templates: Option<Vec<PatternTemplate>>,
    loaded_revisions: Vec<(PathBuf, blake3::Hash)>,
}

/// What dependent reload (if any) was prepared alongside a config change.
enum PreparedFollowUp {
    Unchanged,
    Mapping(Option<PreparedMappingsReload>, Option<Vec<PatternTemplate>>),
    Sources(Box<PreparedSourcesReload>),
}

/// A failover snapshot published ahead of the config update that moved the failover location.
struct PublishedFailover {
    previous: Arc<FailoverOverrides>,
    next: Arc<FailoverOverrides>,
}

impl PublishedFailover {
    fn publish(app_state: &Arc<AppState>, next: FailoverOverrides) -> Self {
        let next = Arc::new(next);
        Self { previous: app_state.app_config.failover.swap(Arc::clone(&next)), next }
    }

    fn restore(&self, app_state: &Arc<AppState>) { app_state.app_config.failover.store(Arc::clone(&self.previous)); }
}

/// Targets whose failover section was added, removed or edited (different etag) between two snapshots, sorted.
fn changed_override_targets(previous: &FailoverOverrides, next: &FailoverOverrides) -> Vec<String> {
    let mut changed: Vec<&str> = next
        .sections
        .iter()
        .filter(|(target, entry)| previous.sections.get(*target).is_none_or(|old| old.etag != entry.etag))
        .map(|(target, _)| target.as_ref())
        .chain(previous.sections.keys().filter(|target| !next.sections.contains_key(*target)).map(AsRef::as_ref))
        .collect();
    // The two halves are disjoint (present in `next` vs. only in `previous`), so sorting is enough.
    changed.sort_unstable();
    changed.into_iter().map(str::to_string).collect()
}

/// Refreshes CLI-forced target IDs against an incoming `SourcesConfig`.
///
/// IDs are positional and can shift when source ordering changes. Targets that no
/// longer exist are warned about and dropped; the remaining ones are re-validated to
/// obtain their new IDs. The reload proceeds regardless — only the stale entries are
/// removed from the filter.
fn refresh_forced_targets(current: Arc<ProcessTargets>, sources: &SourcesConfig) -> Arc<ProcessTargets> {
    if !current.enabled || current.target_names.is_empty() {
        return current;
    }

    let (still_valid, missing): (Vec<String>, Vec<String>) = current
        .target_names
        .iter()
        .cloned()
        .partition(|name| sources.validate_targets(Some(&vec![name.clone()])).is_ok());

    if !missing.is_empty() {
        warn!("CLI-forced target(s) {missing:?} no longer exist in the updated sources and will be ignored.");
    }

    if still_valid.is_empty() {
        // Every forced target was removed — disable the filter so nothing is blocked.
        warn!("All CLI-forced targets have been removed from the updated sources; disabling target filter.");
        match sources.validate_targets(None) {
            Ok(disabled) => Arc::new(disabled),
            Err(_) => current,
        }
    } else {
        match sources.validate_targets(Some(&still_valid)) {
            Ok(refreshed) => Arc::new(refreshed),
            Err(err) => {
                warn!(
                    "Failed to re-validate remaining CLI-forced targets {still_valid:?}: {err}. Keeping previous IDs."
                );
                current
            }
        }
    }
}

impl ConfigFile {
    // -----------------------------------------------------------------
    // Template helpers
    // -----------------------------------------------------------------

    /// Load and merge global templates using the current app state.
    async fn load_prepared_global_templates(
        app_state: &Arc<AppState>,
    ) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let config = app_state.app_config.config.load();
        Self::load_prepared_global_templates_with_config(&paths, &config).await
    }

    /// Load and merge global templates using explicitly-provided config/paths.
    /// Used in the prepare phase when the new config has not yet been applied to `app_state`.
    async fn load_prepared_global_templates_with_config(
        paths: &ConfigPaths,
        config: &Config,
    ) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
        crate::config_loader::load_prepared_global_templates(paths, config).await
    }

    // -----------------------------------------------------------------
    // Mapping prepare / apply
    // -----------------------------------------------------------------

    fn prepare_mapping_reload(
        mapping_file_path: Option<&str>,
        prepared_templates: Option<&[PatternTemplate]>,
    ) -> Result<Option<PreparedMappingsReload>, TuliproxError> {
        let Some(mapping_file_path) = mapping_file_path else {
            return Ok(None);
        };

        match read_mappings_file_with_templates(mapping_file_path, true, prepared_templates) {
            Ok(Some((mapping_files, mappings_cfg))) => {
                let mappings = CompiledMappings::try_from(&mappings_cfg)?;
                Ok(Some(PreparedMappingsReload {
                    mapping_file_path: mapping_file_path.to_string(),
                    mapping_files,
                    mappings,
                }))
            }
            Ok(None) => {
                info!("No mapping file loaded {mapping_file_path}");
                Ok(None)
            }
            Err(err) => {
                error!("Failed to load mapping file {err}");
                Err(err)
            }
        }
    }

    fn apply_mapping_reload(app_state: &Arc<AppState>, prepared_mapping: Option<PreparedMappingsReload>) {
        if let Some(prepared_mapping) = prepared_mapping {
            app_state.app_config.set_mappings(prepared_mapping.mapping_file_path.as_str(), &prepared_mapping.mappings);
            for mapping_file in prepared_mapping.mapping_files {
                info!("Loaded mapping file {}", mapping_file.display());
            }
        }
    }

    fn apply_user_filter_templates(
        app_state: &Arc<AppState>,
        templates: Option<Vec<PatternTemplate>>,
    ) -> Result<(), TuliproxError> {
        if let Some(current) = app_state.app_config.api_proxy.load_full() {
            let mut config = (*current).clone();
            config.set_templates(templates.map(Arc::from));
            app_state.app_config.set_api_proxy(config)?;
        }
        Ok(())
    }

    fn load_mapping_with_templates(
        app_state: &Arc<AppState>,
        prepared_templates: Option<&[PatternTemplate]>,
    ) -> Result<(), TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let prepared_mapping = Self::prepare_mapping_reload(paths.mapping_file_path.as_deref(), prepared_templates)?;
        Self::apply_mapping_reload(app_state, prepared_mapping);
        Ok(())
    }

    async fn load_mapping(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let prepared_templates = Self::load_prepared_global_templates(app_state).await?;
        Self::load_mapping_with_templates(app_state, prepared_templates.as_deref())?;
        Self::apply_user_filter_templates(app_state, prepared_templates)
    }

    // -----------------------------------------------------------------
    // Sources prepare / apply
    // -----------------------------------------------------------------

    /// Prepare sources + mappings using explicitly-provided config/paths (prepare phase only — no state changes).
    async fn prepare_sources_reload_with_config(
        config: &Config,
        paths: &ConfigPaths,
    ) -> Result<PreparedSourcesReload, TuliproxError> {
        let sources_file = paths.sources_file_path.clone();
        let prepared_templates = Self::load_prepared_global_templates_with_config(paths, config).await?;
        let (mut sources_dto, source_revision) = read_sources_file_with_revision(
            &PathBuf::from(sources_file.as_str()),
            true,
            true,
            config.get_hdhr_device_overview().as_ref(),
            prepared_templates.as_deref(),
        )
        .await?;
        let mut loaded_revisions = crate::config_loader::capture_batch_revisions(&sources_dto).await;
        loaded_revisions.push((PathBuf::from(&sources_file), source_revision));
        prepare_sources_batch(&mut sources_dto, true).await?;
        let sources: SourcesConfig = SourcesConfig::try_from(sources_dto)?;
        let prepared_mapping =
            Self::prepare_mapping_reload(paths.mapping_file_path.as_deref(), prepared_templates.as_deref())?;
        Ok(PreparedSourcesReload {
            sources,
            mapping: prepared_mapping,
            sources_file,
            templates: prepared_templates,
            loaded_revisions,
        })
    }

    /// Apply a fully-prepared sources reload to app state (infallible under normal conditions).
    async fn apply_sources_reload(
        app_state: &Arc<AppState>,
        prepared: PreparedSourcesReload,
    ) -> Result<(), TuliproxError> {
        let current_forced = app_state.playlist_updates.forced_targets.load_full();
        let validated_forced = refresh_forced_targets(current_forced, &prepared.sources);

        update_app_state_sources(app_state, prepared.sources, Some(validated_forced)).await?;
        Self::apply_mapping_reload(app_state, prepared.mapping);
        Self::apply_user_filter_templates(app_state, prepared.templates)?;
        for (path, revision) in prepared.loaded_revisions {
            app_state.app_config.file_locks.record_loaded_revision(&path, revision).await;
        }
        info!("Loaded sources file {}", prepared.sources_file);
        Ok(())
    }

    /// Public entry-point for Sources / Template file-watch events.
    /// Reads config from current app state (config itself has not changed).
    pub async fn load_sources(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let config = app_state.app_config.config.load();
        // PREPARE — no state mutations
        let prepared = Self::prepare_sources_reload_with_config(&config, &paths).await?;
        // APPLY — only reached when preparation fully succeeded
        Self::apply_sources_reload(app_state, prepared).await
    }

    // -----------------------------------------------------------------
    // API proxy
    // -----------------------------------------------------------------

    async fn load_api_proxy(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        match crate::config_loader::read_api_proxy_config(&app_state.app_config, true).await {
            Ok(Some(api_proxy)) => {
                app_state.app_config.set_api_proxy(api_proxy)?;
                let paths = app_state.app_config.paths.load();
                info!("Loaded Api Proxy File: {:?}", paths.api_proxy_file_path);
            }
            Ok(None) => {
                let paths = app_state.app_config.paths.load();
                info!("Could not load Api Proxy File: {:?}", paths.api_proxy_file_path);
            }
            Err(err) => {
                error!("Failed to load api-proxy file {err}");
                return Err(err);
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Failover override files
    // -----------------------------------------------------------------

    /// Reads the failover files of `source` under the failover mutation lock. The live snapshot stays untouched,
    /// so a failing read keeps the last good state active.
    async fn prepare_failover_reload(
        app_state: &Arc<AppState>,
        source: &FailoverSource,
    ) -> Result<FailoverOverrides, TuliproxError> {
        load_failover_snapshot(&app_state.app_config, source)
            .await
            .inspect_err(|err| error!("Failed to load failover overrides: {err}"))
    }

    /// Hot-applies the targets whose section changed between two snapshots and tells listeners which targets
    /// those are. Nothing is emitted when no section changed.
    async fn announce_failover_change(
        app_state: &Arc<AppState>,
        previous: &FailoverOverrides,
        next: &FailoverOverrides,
    ) {
        let changed = changed_override_targets(previous, next);
        if changed.is_empty() {
            return;
        }
        for target in &changed {
            // A removed section is structural and waits for the next playlist update.
            if let Some(entry) = next.sections.get(target.as_str()) {
                let applied = try_hot_apply(app_state, target, entry).await;
                debug!("Failover overrides of target {target}: hot applied = {applied}");
            }
        }
        info!("Loaded failover overrides, changed targets: {}", changed.join(", "));
        app_state.event_manager.send_event(EventMessage::FailoverOverridesChanged { targets: changed });
    }

    /// Entry-point for failover file-watch events; the source is resolved from the live config and paths.
    /// The files are read and the snapshot is swapped under the failover mutation lock, so a reload never
    /// overwrites a concurrent Web UI save with older content.
    pub(crate) async fn load_failover(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let source = {
            let paths = app_state.app_config.paths.load();
            let config = app_state.app_config.config.load();
            resolve_failover_source(&paths, &config, true)
        };
        let (previous, next) = reload_failover_snapshot(&app_state.app_config, &source)
            .await
            .inspect_err(|err| error!("Failed to load failover overrides: {err}"))?;
        Self::announce_failover_change(app_state, &previous, &next).await;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Main config (config.yml) — full prepare-then-apply
    // -----------------------------------------------------------------

    async fn load_config(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let config_file = paths.config_file_path.clone();

        // ── Parse ────────────────────────────────────────────────────
        let config_dto = crate::config_loader::read_config_file_with_options(
            config_file.as_str(),
            crate::config_loader::ReadConfigOptions::resolve_and_compute(),
        )?;

        let current_mapping_path = paths
            .mapping_file_path
            .clone()
            .unwrap_or_else(|| utils::resolve_mapping_file_path(paths.config_path.as_str(), None));
        let next_mapping_path = paths.mapping_file_path.clone().unwrap_or_else(|| {
            utils::resolve_mapping_file_path(paths.config_path.as_str(), config_dto.mapping_path.as_deref())
        });
        let mapping_changed = current_mapping_path != next_mapping_path;

        let current_template_path = paths
            .template_file_path
            .clone()
            .unwrap_or_else(|| utils::resolve_template_file_path(paths.config_path.as_str(), None));
        let next_template_path = paths.template_file_path.clone().unwrap_or_else(|| {
            utils::resolve_template_file_path(paths.config_path.as_str(), config_dto.template_path.as_deref())
        });
        let template_changed = current_template_path != next_template_path;

        let mut config: Config = Config::from(config_dto);
        config.prepare(paths.config_path.as_str(), paths.home_path.as_str())?;

        // Resolve the paths that will be active if this configuration is applied.
        let mut effective_paths = paths.as_ref().clone();
        effective_paths.mapping_file_path = Some(next_mapping_path.clone());
        effective_paths.template_file_path = Some(next_template_path.clone());

        // Load and validate dependent data without changing live state.
        let follow_up: PreparedFollowUp = if template_changed {
            // Template path changed -> sources depend on new templates, reload everything.
            let prepared = Self::prepare_sources_reload_with_config(&config, &effective_paths).await?;
            PreparedFollowUp::Sources(Box::new(prepared))
        } else if mapping_changed {
            // Only mapping path changed; templates are the same -> load templates once.
            let prepared_templates =
                Self::load_prepared_global_templates_with_config(&effective_paths, &config).await?;
            let prepared = Self::prepare_mapping_reload(
                effective_paths.mapping_file_path.as_deref(),
                prepared_templates.as_deref(),
            )?;
            PreparedFollowUp::Mapping(prepared, prepared_templates)
        } else {
            PreparedFollowUp::Unchanged
        };

        // A changed failover location is read before anything is applied, so an unreadable one rejects the reload.
        let next_failover_source = resolve_failover_source(&effective_paths, &config, true);
        let prepared_failover = if app_state.app_config.failover.load().source.as_ref() == Some(&next_failover_source) {
            None
        } else {
            Some(Self::prepare_failover_reload(app_state, &next_failover_source).await?)
        };

        let previous_config: Config = (*app_state.app_config.config.load_full()).clone();
        let previous_sources: SourcesConfig = (*app_state.app_config.sources.load_full()).clone();
        let previous_forced_targets = app_state.playlist_updates.forced_targets.load_full();

        // The config watcher restarted by the config update reads the failover location from the snapshot, so a
        // changed location is published first. A failed reload puts the previous snapshot back.
        let failover_swap = prepared_failover.map(|next| PublishedFailover::publish(app_state, next));
        let restore_failover = || failover_swap.iter().for_each(|swap| swap.restore(app_state));

        // Update live state only after every dependent value has been prepared.
        if let Err(err) = update_app_state_config(app_state, config).await {
            error!("Failed to apply config reload: {err}. Attempting config rollback.");
            restore_failover();
            if let Err(rollback_err) = update_app_state_config(app_state, previous_config).await {
                error!("Failed to rollback config after reload failure: {rollback_err}");
                error!(
                    "Config reload and rollback both failed; runtime state may be inconsistent. Please restart the service."
                );
            }
            return Err(err);
        }

        let follow_up_result: Result<(), TuliproxError> = match follow_up {
            PreparedFollowUp::Unchanged => Ok(()),
            PreparedFollowUp::Mapping(prepared, templates) => {
                Self::apply_mapping_reload(app_state, prepared);
                Self::apply_user_filter_templates(app_state, templates)
            }
            PreparedFollowUp::Sources(prepared) => Self::apply_sources_reload(app_state, *prepared).await,
        };

        if let Err(err) = follow_up_result {
            error!("Failed to apply dependent reload after config update: {err}. Attempting rollback.");
            restore_failover();

            if let Err(rollback_err) = update_app_state_config(app_state, previous_config).await {
                error!("Failed to rollback config after dependent reload failure: {rollback_err}");
                error!(
                    "Dependent reload and config rollback both failed; runtime state may be inconsistent. Please restart the service."
                );
            }

            if let Err(rollback_err) =
                update_app_state_sources(app_state, previous_sources, Some(previous_forced_targets)).await
            {
                error!("Failed to rollback sources after dependent reload failure: {rollback_err}");
                error!(
                    "Source rollback failed after dependent reload error; runtime state may be inconsistent. Please restart the service."
                );
            }

            return Err(err);
        }

        if let Some(swap) = &failover_swap {
            Self::announce_failover_change(app_state, &swap.previous, &swap.next).await;
        }

        info!("Loaded config file {config_file}");
        Ok(())
    }

    // -----------------------------------------------------------------
    // Reload dispatcher
    // -----------------------------------------------------------------

    async fn reload_source_file(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        // TODO selective update and not complete sources update ?
        ConfigFile::load_sources(app_state).await
    }

    pub(crate) async fn reload(&self, file_path: &Path, app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        debug!("File change detected {}", file_path.display());
        // Channels and templates are built once and cached, so an edited
        // token, URL or template would otherwise not take effect until a
        // restart.
        tuliprox_messaging::channels::invalidate();
        tuliprox_messaging::render::invalidate_cache();
        match self {
            ConfigFile::ApiProxy => {
                ConfigFile::load_api_proxy(app_state).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::ApiProxy));
            }
            ConfigFile::Mapping => {
                ConfigFile::load_mapping(app_state).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::Mapping));
            }
            ConfigFile::Template | ConfigFile::Sources => {
                ConfigFile::load_sources(app_state).await?;
                let event_type =
                    if matches!(self, ConfigFile::Template) { ConfigType::Template } else { ConfigType::Sources };
                app_state.event_manager.send_event(EventMessage::ConfigChange(event_type));
            }
            ConfigFile::Config => {
                Box::pin(ConfigFile::load_config(app_state)).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::Config));
            }
            ConfigFile::SourceFile => {
                ConfigFile::reload_source_file(app_state).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::Sources));
            }
            ConfigFile::Failover => ConfigFile::load_failover(app_state).await?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{changed_override_targets, refresh_forced_targets, ConfigFile};
    use crate::{
        api::model::{create_test_app_state, AppState, EventMessage},
        model::{Config, ConfigSource, ConfigTarget, ProcessTargets, SourcesConfig},
    };
    use shared::model::ProcessingOrder;
    use std::{path::Path, sync::Arc};
    use tuliprox_core::{
        model::{FailoverOverrides, TargetOverridesEntry},
        utils::FailoverSource,
    };

    fn section_yaml(target: &str, stream_ids: &[&str]) -> String {
        let excluded = stream_ids
            .iter()
            .map(|id| format!("      - {{ input: a, stream_id: \"{id}\" }}\n"))
            .collect::<Vec<_>>()
            .concat();
        format!("version: 1\ntargets:\n  - target: {target}\n    excluded:\n{excluded}")
    }

    fn app_state_in(config_dir: &Path) -> Arc<AppState> {
        let app_state = create_test_app_state(Config::default());
        let mut paths = (*app_state.app_config.paths.load_full()).clone();
        paths.config_path = config_dir.to_string_lossy().into_owned();
        paths.config_file_path = config_dir.join("config.yml").to_string_lossy().into_owned();
        app_state.app_config.paths.store(Arc::new(paths));
        app_state
    }

    /// Mirrors the start-up load so the snapshot carries the source a reload compares against.
    fn load_initial_failover(app_state: &Arc<AppState>) {
        let paths = app_state.app_config.paths.load();
        let config = app_state.app_config.config.load();
        let source = crate::config_loader::resolve_failover_source(&paths, &config, true);
        let overrides = tuliprox_repository::load_failover_overrides(&source).expect("initial failover load");
        app_state.app_config.failover.store(Arc::new(overrides));
    }

    fn drain_overrides_events(events: &mut tokio::sync::broadcast::Receiver<EventMessage>) -> Vec<Vec<String>> {
        let mut changed = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let EventMessage::FailoverOverridesChanged { targets } = event {
                changed.push(targets);
            }
        }
        changed
    }

    fn entry(etag: &str) -> Arc<TargetOverridesEntry> {
        Arc::new(TargetOverridesEntry {
            overrides: shared::model::TargetFailoverOverridesDto::new("t"),
            file: std::path::PathBuf::from("failover.yml"),
            etag: Arc::from(etag),
            findings: Vec::new(),
        })
    }

    fn snapshot(sections: &[(&str, &str)]) -> FailoverOverrides {
        FailoverOverrides {
            sections: sections.iter().map(|(target, etag)| (Arc::from(*target), entry(etag))).collect(),
            ..FailoverOverrides::default()
        }
    }

    #[test]
    fn changed_targets_are_edited_added_and_removed_sections_sorted() {
        let previous = snapshot(&[("b", "1"), ("same", "9"), ("gone", "1"), ("a", "1")]);
        let next = snapshot(&[("b", "2"), ("same", "9"), ("new", "1"), ("a", "2")]);

        assert_eq!(changed_override_targets(&previous, &next), vec!["a", "b", "gone", "new"]);
        assert_eq!(changed_override_targets(&next, &next).len(), 0);
    }

    #[tokio::test]
    async fn failover_change_swaps_snapshot_and_emits_event() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("failover.yml");
        std::fs::write(&file, section_yaml("Main", &["1"]))?;
        let app_state = app_state_in(dir.path());
        load_initial_failover(&app_state);
        let old_etag = Arc::clone(&app_state.app_config.failover.load().sections["Main"].etag);
        let mut events = app_state.event_manager.get_event_channel();

        std::fs::write(&file, section_yaml("Main", &["1", "2"]))?;
        ConfigFile::Failover.reload(&file, &app_state).await?;

        let snapshot = app_state.app_config.failover.load_full();
        assert_ne!(snapshot.sections["Main"].etag, old_etag);
        assert_eq!(snapshot.source, Some(FailoverSource::File(file.clone())));
        assert_eq!(drain_overrides_events(&mut events), vec![vec!["Main".to_string()]]);

        // Same content again: swapped, but no section changed, so nothing is announced.
        ConfigFile::Failover.reload(&file, &app_state).await?;
        assert_eq!(drain_overrides_events(&mut events).len(), 0);

        // A section added in a second target and the first one removed are both reported, sorted.
        std::fs::write(&file, section_yaml("Other", &["1"]))?;
        ConfigFile::Failover.reload(&file, &app_state).await?;
        assert_eq!(drain_overrides_events(&mut events), vec![vec!["Main".to_string(), "Other".to_string()]]);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_failover_change_keeps_previous_snapshot() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("failover.yml");
        std::fs::write(&file, section_yaml("Main", &["1"]))?;
        let app_state = app_state_in(dir.path());
        load_initial_failover(&app_state);
        let before = app_state.app_config.failover.load_full();
        let mut events = app_state.event_manager.get_event_channel();

        std::fs::write(&file, "version: 9\ntargets: []\n")?;
        let result = ConfigFile::Failover.reload(&file, &app_state).await;

        assert!(result.is_err(), "an unsupported version must fail the reload");
        let after = app_state.app_config.failover.load_full();
        assert!(Arc::ptr_eq(&before, &after), "the previous snapshot must stay active");
        assert!(after.error.is_none());
        assert!(after.sections.contains_key("Main"));
        assert_eq!(drain_overrides_events(&mut events).len(), 0);
        Ok(())
    }

    const MINIMAL_CONFIG: &str = "api:\n  host: localhost\n  port: 8901\n  web_root: ./web\n";

    #[tokio::test]
    async fn config_failover_path_change_reloads_failover() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("failover.yml"), section_yaml("Main", &["1"]))?;
        std::fs::write(dir.path().join("other.yml"), section_yaml("Other", &["1"]))?;
        let storage = dir.path().join("storage");
        let config_yml = dir.path().join("config.yml");
        let config_text = |extra: &str| format!("{MINIMAL_CONFIG}storage_dir: {}\n{extra}", storage.display());
        std::fs::write(&config_yml, config_text(""))?;
        let app_state = app_state_in(dir.path());
        load_initial_failover(&app_state);
        assert!(app_state.app_config.failover.load().sections.contains_key("Main"));
        let mut events = app_state.event_manager.get_event_channel();

        // Unchanged location: the failover snapshot is not touched.
        let before = app_state.app_config.failover.load_full();
        ConfigFile::Config.reload(&config_yml, &app_state).await?;
        assert!(Arc::ptr_eq(&before, &app_state.app_config.failover.load_full()));

        std::fs::write(&config_yml, config_text("failover_path: other.yml\n"))?;
        ConfigFile::Config.reload(&config_yml, &app_state).await?;

        let snapshot = app_state.app_config.failover.load_full();
        assert_eq!(snapshot.source, Some(FailoverSource::File(dir.path().join("other.yml"))));
        assert!(snapshot.sections.contains_key("Other") && !snapshot.sections.contains_key("Main"));
        assert_eq!(drain_overrides_events(&mut events), vec![vec!["Main".to_string(), "Other".to_string()]]);

        // An unreadable new location rejects the whole reload and keeps config and snapshot.
        std::fs::write(dir.path().join("broken.yml"), "version: 9\ntargets: []\n")?;
        std::fs::write(&config_yml, config_text("failover_path: broken.yml\n"))?;
        assert!(ConfigFile::Config.reload(&config_yml, &app_state).await.is_err());
        assert_eq!(
            app_state.app_config.failover.load().source,
            Some(FailoverSource::File(dir.path().join("other.yml")))
        );
        assert_eq!(app_state.app_config.config.load().failover_path.as_deref(), Some("other.yml"));
        Ok(())
    }

    #[tokio::test]
    async fn failover_path_change_watches_new_source() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("failover.yml"), section_yaml("Main", &["1"]))?;
        let other = dir.path().join("other.yml");
        std::fs::write(&other, section_yaml("Other", &["1"]))?;
        let storage = dir.path().join("storage");
        let config_yml = dir.path().join("config.yml");
        let config_text = |extra: &str| {
            format!("{MINIMAL_CONFIG}storage_dir: {}\nconfig_hot_reload: true\n{extra}", storage.display())
        };
        std::fs::write(&config_yml, config_text(""))?;
        let app_state = app_state_in(dir.path());
        load_initial_failover(&app_state);

        // The config reload restarts the file watcher; it must already see the new failover location.
        std::fs::write(&config_yml, config_text("failover_path: other.yml\n"))?;
        ConfigFile::Config.reload(&config_yml, &app_state).await?;
        let mut events = app_state.event_manager.get_event_channel();
        drain_overrides_events(&mut events);

        std::fs::write(&other, section_yaml("Other", &["1", "2"]))?;
        let changed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Ok(EventMessage::FailoverOverridesChanged { targets }) => break Some(targets),
                    Ok(_) => {}
                    Err(_) => break None,
                }
            }
        })
        .await;
        app_state.cancel_tokens.load().file_watch.cancel();
        assert_eq!(changed?, Some(vec!["Other".to_string()]));
        Ok(())
    }

    fn target(id: u16, name: &str) -> Arc<ConfigTarget> {
        Arc::new(ConfigTarget {
            id,
            enabled: true,
            name: name.to_string(),
            options: None,
            sort: None,
            filter: crate::model::StagedFilter::default(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            curation: None,
            execution_plan: crate::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        })
    }

    fn sources_with(targets: Vec<Arc<ConfigTarget>>) -> SourcesConfig {
        SourcesConfig { sources: vec![ConfigSource { inputs: vec![], targets }], ..SourcesConfig::default() }
    }

    fn forced(names: &[&str]) -> Arc<ProcessTargets> {
        Arc::new(ProcessTargets {
            enabled: true,
            inputs: vec![],
            targets: vec![],
            target_names: names.iter().map(|name| (*name).to_string()).collect(),
        })
    }

    /// A reload must not invent a target filter for an operator who never
    /// passed `-target`.
    #[test]
    fn a_disabled_filter_survives_a_reload_untouched() {
        let current =
            Arc::new(ProcessTargets { enabled: false, inputs: vec![], targets: vec![], target_names: vec![] });

        let refreshed = refresh_forced_targets(Arc::clone(&current), &sources_with(vec![target(1, "my-target")]));

        assert!(!refreshed.enabled, "a reload must not enable a filter the operator never asked for");
        assert_eq!(refreshed.target_names.len(), 0);
    }

    /// Target ids are positional, so a surviving name must come back with
    /// whatever id it has now rather than the one it had at startup.
    #[test]
    fn a_surviving_forced_target_is_revalidated_to_its_new_id() {
        let refreshed = refresh_forced_targets(forced(&["my-target"]), &sources_with(vec![target(42, "my-target")]));

        assert!(refreshed.enabled);
        assert_eq!(refreshed.target_names, vec!["my-target".to_string()]);
        assert_eq!(refreshed.targets, vec![42], "the filter must carry the id from the incoming sources");
    }

    /// One target disappearing must not take the rest of the filter with it.
    #[test]
    fn a_removed_forced_target_is_dropped_and_the_rest_kept() {
        let refreshed = refresh_forced_targets(forced(&["kept", "gone"]), &sources_with(vec![target(1, "kept")]));

        assert!(refreshed.enabled);
        assert_eq!(refreshed.target_names, vec!["kept".to_string()]);
        assert_eq!(refreshed.targets, vec![1]);
    }

    /// With every forced target gone the filter would block everything, so it
    /// is disabled instead.
    #[test]
    fn losing_every_forced_target_disables_the_filter_rather_than_blocking_all() {
        let refreshed = refresh_forced_targets(forced(&["gone"]), &sources_with(vec![target(1, "other")]));

        assert!(!refreshed.enabled, "an empty filter must not be left enabled: it would process nothing");
        assert_eq!(refreshed.target_names.len(), 0);
    }
}
