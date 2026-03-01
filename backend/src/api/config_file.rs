use crate::{
    api::model::{update_app_state_config, update_app_state_sources, AppState, EventMessage},
    model::{Config, Mappings, SourcesConfig},
    utils,
    utils::{
        prepare_sources_batch, read_config_file, read_mappings_file_unprepared, read_mappings_file_with_templates,
        read_sources_file, read_sources_file_from_path_with_templates, read_templates,
    },
};
use log::{debug, error, info};
use shared::{
    error::TuliproxError,
    model::{ConfigPaths, ConfigType, PatternTemplate},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use crate::utils::resolve_template_and_mapping_paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigFile {
    Config,
    ApiProxy,
    Mapping,
    Template,
    Sources,
    SourceFile,
}

#[derive(Debug)]
struct PreparedMappingsReload {
    mapping_file_path: String,
    mapping_files: Vec<PathBuf>,
    mappings: Mappings,
}

/// Fully prepared sources + mappings data, ready to apply without any I/O or fallible work.
struct PreparedSourcesReload {
    sources: SourcesConfig,
    mapping: Option<PreparedMappingsReload>,
    sources_file: String,
}

/// What dependent reload (if any) was prepared alongside a config change.
enum PreparedFollowUp {
    Unchanged,
    Mapping(Option<PreparedMappingsReload>),
    Sources(PreparedSourcesReload),
}

impl ConfigFile {
    // -----------------------------------------------------------------
    // Template helpers
    // -----------------------------------------------------------------

    /// Load and merge global templates using the current app state.
    fn load_prepared_global_templates(
        app_state: &Arc<AppState>,
    ) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let config = app_state.app_config.config.load();
        Self::load_prepared_global_templates_with_config(&paths, &config)
    }

    /// Load and merge global templates using explicitly-provided config/paths.
    /// Used in the prepare phase when the new config has not yet been applied to `app_state`.
    fn load_prepared_global_templates_with_config(
        paths: &ConfigPaths,
        config: &Config,
    ) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
        let sources_inline_templates =
            read_sources_file(paths.sources_file_path.as_str(), false, false, None, None)?.templates;

        // Use robust fallbacks for mapping and template paths
        let (effective_template_path, effective_mapping_path) = resolve_template_and_mapping_paths(paths, config.template_path.as_deref(), config.mapping_path.as_deref());

        let mapping_inline_templates = read_mappings_file_unprepared(effective_mapping_path.as_ref(), false)?
            .map(|(_, mapping)| mapping)
            .and_then(|mapping| mapping.mappings.templates);

        let template_bundle = read_templates(
            Some(effective_template_path.as_ref()),
            true,
            sources_inline_templates.as_deref(),
            mapping_inline_templates.as_deref(),
        )?;
        Ok(template_bundle.prepared)
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
            Ok(Some((mapping_files, mappings_cfg))) => Ok(Some(PreparedMappingsReload {
                mapping_file_path: mapping_file_path.to_string(),
                mapping_files,
                mappings: Mappings::from(&mappings_cfg),
            })),
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

    fn load_mapping_with_templates(
        app_state: &Arc<AppState>,
        prepared_templates: Option<&[PatternTemplate]>,
    ) -> Result<(), TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let prepared_mapping = Self::prepare_mapping_reload(paths.mapping_file_path.as_deref(), prepared_templates)?;
        Self::apply_mapping_reload(app_state, prepared_mapping);
        Ok(())
    }

    fn load_mapping(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let prepared_templates = Self::load_prepared_global_templates(app_state)?;
        Self::load_mapping_with_templates(app_state, prepared_templates.as_deref())
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
        let prepared_templates = Self::load_prepared_global_templates_with_config(paths, config)?;
        let mut sources_dto = read_sources_file_from_path_with_templates(
            &PathBuf::from(sources_file.as_str()),
            true,
            true,
            config.get_hdhr_device_overview().as_ref(),
            prepared_templates.as_deref(),
        )?;
        prepare_sources_batch(&mut sources_dto, true).await?;
        let sources: SourcesConfig = SourcesConfig::try_from(sources_dto)?;
        let prepared_mapping =
            Self::prepare_mapping_reload(paths.mapping_file_path.as_deref(), prepared_templates.as_deref())?;
        Ok(PreparedSourcesReload { sources, mapping: prepared_mapping, sources_file })
    }

    /// Apply a fully-prepared sources reload to app state (infallible under normal conditions).
    async fn apply_sources_reload(
        app_state: &Arc<AppState>,
        prepared: PreparedSourcesReload,
    ) -> Result<(), TuliproxError> {
        update_app_state_sources(app_state, prepared.sources).await?;
        Self::apply_mapping_reload(app_state, prepared.mapping);
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
        match utils::read_api_proxy_config(&app_state.app_config, true).await {
            Ok(Some(api_proxy)) => {
                app_state.app_config.set_api_proxy(api_proxy)?;
                let paths = app_state.app_config.paths.load();
                info!("Loaded Api Proxy File: {:?}", &paths.api_proxy_file_path);
            }
            Ok(None) => {
                let paths = app_state.app_config.paths.load();
                info!("Could not load Api Proxy File: {:?}", &paths.api_proxy_file_path);
            }
            Err(err) => {
                error!("Failed to load api-proxy file {err}");
                return Err(err);
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Main config (config.yml) — full prepare-then-apply
    // -----------------------------------------------------------------

    async fn load_config(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let config_file = paths.config_file_path.clone();

        // ── Parse ────────────────────────────────────────────────────
        let config_dto = read_config_file(config_file.as_str(), true, true)?;

        let default_mapping_path = utils::get_default_mappings_path(paths.config_path.as_str());
        let current_mapping_path = paths.mapping_file_path.clone().unwrap_or_else(|| default_mapping_path.clone());
        let next_mapping_path =
            config_dto.mapping_path.clone().filter(|path| !path.trim().is_empty()).unwrap_or(default_mapping_path);
        let mapping_changed = current_mapping_path != next_mapping_path;

        let default_template_path = utils::get_default_templates_path(paths.config_path.as_str());
        let current_template_path = paths.template_file_path.clone().unwrap_or_else(|| default_template_path.clone());
        let next_template_path =
            config_dto.template_path.clone().filter(|path| !path.trim().is_empty()).unwrap_or(default_template_path);
        let template_changed = current_template_path != next_template_path;

        let mut config: Config = Config::from(config_dto);
        config.prepare(paths.config_path.as_str())?;

        // Compute effective runtime paths for the NEW config before apply.
        // This ensures prepare-phase reads/validates against the same files that will be active after apply.
        let mut effective_paths = paths.as_ref().clone();
        effective_paths.mapping_file_path = Some(next_mapping_path.clone());
        effective_paths.template_file_path = Some(next_template_path.clone());

        // ── PREPARE PHASE ────────────────────────────────────────────
        // All dependent data is loaded/validated here, using the NEW config values
        // but without touching any live state. If anything fails, we return an error
        // and the currently-running state remains completely unchanged.
        let follow_up: PreparedFollowUp = if template_changed {
            // Template path changed → sources depend on new templates, reload everything.
            let prepared = Self::prepare_sources_reload_with_config(&config, &effective_paths).await?;
            PreparedFollowUp::Sources(prepared)
        } else if mapping_changed {
            // Only mapping path changed; templates are the same → load templates once.
            let prepared_templates = Self::load_prepared_global_templates_with_config(&effective_paths, &config)?;
            let prepared = Self::prepare_mapping_reload(
                effective_paths.mapping_file_path.as_deref(),
                prepared_templates.as_deref(),
            )?;
            PreparedFollowUp::Mapping(prepared)
        } else {
            PreparedFollowUp::Unchanged
        };

        let previous_config: Config = (*app_state.app_config.config.load_full()).clone();
        let previous_sources: SourcesConfig = (*app_state.app_config.sources.load_full()).clone();
        let previous_forced_targets = app_state.forced_targets.load_full();

        // ── APPLY PHASE ──────────────────────────────────────────────
        // All preparation succeeded — safe to update live state now.
        if let Err(err) = update_app_state_config(app_state, config).await {
            error!("Failed to apply config reload: {err}. Attempting config rollback.");
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
            PreparedFollowUp::Mapping(prepared) => {
                Self::apply_mapping_reload(app_state, prepared);
                Ok(())
            }
            PreparedFollowUp::Sources(prepared) => Self::apply_sources_reload(app_state, prepared).await,
        };

        if let Err(err) = follow_up_result {
            error!("Failed to apply dependent reload after config update: {err}. Attempting rollback.");

            if let Err(rollback_err) = update_app_state_config(app_state, previous_config).await {
                error!("Failed to rollback config after dependent reload failure: {rollback_err}");
                error!(
                    "Dependent reload and config rollback both failed; runtime state may be inconsistent. Please restart the service."
                );
            }

            app_state.forced_targets.store(previous_forced_targets);
            if let Err(rollback_err) = update_app_state_sources(app_state, previous_sources).await {
                error!("Failed to rollback sources after dependent reload failure: {rollback_err}");
                error!(
                    "Source rollback failed after dependent reload error; runtime state may be inconsistent. Please restart the service."
                );
            }

            return Err(err);
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
        match self {
            ConfigFile::ApiProxy => {
                ConfigFile::load_api_proxy(app_state).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::ApiProxy));
            }
            ConfigFile::Mapping => {
                ConfigFile::load_mapping(app_state)?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::Mapping));
            }
            ConfigFile::Template | ConfigFile::Sources => {
                ConfigFile::load_sources(app_state).await?;
                let event_type =
                    if matches!(self, ConfigFile::Template) { ConfigType::Template } else { ConfigType::Sources };
                app_state.event_manager.send_event(EventMessage::ConfigChange(event_type));
            }
            ConfigFile::Config => {
                ConfigFile::load_config(app_state).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::Config));
            }
            ConfigFile::SourceFile => {
                ConfigFile::reload_source_file(app_state).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::Sources));
            }
        }
        Ok(())
    }
}
