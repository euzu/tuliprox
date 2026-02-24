use crate::api::model::{update_app_state_config, update_app_state_sources, AppState, EventMessage};
use crate::model::{Config, Mappings, SourcesConfig};
use crate::utils;
use crate::utils::{
    read_templates, prepare_sources_batch, read_config_file, read_mappings_file_unprepared,
    read_mappings_file_with_templates, read_sources_file, read_sources_file_from_path_with_templates,
};
use log::{debug, error, info};
use shared::error::TuliproxError;
use shared::model::{ConfigType, PatternTemplate};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

impl ConfigFile {
    fn load_prepared_global_templates(app_state: &Arc<AppState>) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let config = app_state.app_config.config.load();

        let sources_inline_templates =
            read_sources_file(paths.sources_file_path.as_str(), false, false, None)?.templates;
        let mapping_inline_templates = if let Some(mapping_file_path) = paths.mapping_file_path.as_ref() {
            read_mappings_file_unprepared(mapping_file_path, false)?
                .map(|(_, mapping)| mapping)
                .and_then(|mapping| mapping.mappings.templates)
        } else {
            None
        };
        let template_bundle = read_templates(
            paths.template_file_path.as_deref().or(config.template_path.as_deref()),
            true,
            sources_inline_templates.as_deref(),
            mapping_inline_templates.as_deref(),
        )?;
        Ok(template_bundle.prepared)
    }

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

    fn apply_mapping_reload(
        app_state: &Arc<AppState>,
        prepared_mapping: Option<PreparedMappingsReload>,
    ) {
        if let Some(prepared_mapping) = prepared_mapping {
            app_state
                .app_config
                .set_mappings(prepared_mapping.mapping_file_path.as_str(), &prepared_mapping.mappings);
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
        let prepared_mapping =
            Self::prepare_mapping_reload(paths.mapping_file_path.as_deref(), prepared_templates)?;
        Self::apply_mapping_reload(app_state, prepared_mapping);
        Ok(())
    }

    fn load_mapping(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let prepared_templates = Self::load_prepared_global_templates(app_state)?;
        Self::load_mapping_with_templates(app_state, prepared_templates.as_deref())
    }

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

    async fn load_config(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let config_file = paths.config_file_path.as_str();
        let config_dto = read_config_file(config_file, true, true)?;

        let default_mapping_path = utils::get_default_mappings_path(paths.config_path.as_str());
        let current_mapping_path = paths
            .mapping_file_path
            .clone()
            .unwrap_or_else(|| default_mapping_path.clone());
        let next_mapping_path = config_dto
            .mapping_path
            .clone()
            .filter(|path| !path.trim().is_empty())
            .unwrap_or(default_mapping_path);
        let mapping_changed = current_mapping_path != next_mapping_path;

        let default_template_path = utils::get_default_templates_path(paths.config_path.as_str());
        let current_template_path = paths
            .template_file_path
            .clone()
            .unwrap_or_else(|| default_template_path.clone());
        let next_template_path = config_dto
            .template_path
            .clone()
            .filter(|path| !path.trim().is_empty())
            .unwrap_or(default_template_path);
        let template_changed = current_template_path != next_template_path;

        let mut config: Config = Config::from(config_dto);
        config.prepare(paths.config_path.as_str()).await?;
        let previous_config: Config = (*app_state.app_config.config.load_full()).clone();
        update_app_state_config(app_state, config).await?;
        let follow_up_result = if template_changed {
            Self::load_sources(app_state).await
        } else if mapping_changed {
            Self::load_mapping(app_state)
        } else {
            Ok(())
        };

        if let Err(err) = follow_up_result {
            error!("Failed to apply dependent reload after config update; rolling back config: {err}");
            if let Err(rollback_err) = update_app_state_config(app_state, previous_config).await {
                error!("Failed to rollback config after reload failure: {rollback_err}");
            }
            return Err(err);
        }
        info!("Loaded config file {config_file}");
        Ok(())
    }

    pub async fn load_sources(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let paths = app_state.app_config.paths.load();
        let sources_file = paths.sources_file_path.clone();
        let mapping_file_path = paths.mapping_file_path.clone();
        let prepared_templates = Self::load_prepared_global_templates(app_state)?;
        let mut sources_dto = {
            let config = app_state.app_config.config.load();
            read_sources_file_from_path_with_templates(
                &PathBuf::from(sources_file.as_str()),
                true,
                true,
                config.get_hdhr_device_overview().as_ref(),
                prepared_templates.as_deref(),
            )?
        };
        prepare_sources_batch(&mut sources_dto, true).await?;
        let sources: SourcesConfig = SourcesConfig::try_from(sources_dto)?;
        let prepared_mapping =
            Self::prepare_mapping_reload(mapping_file_path.as_deref(), prepared_templates.as_deref())?;
        update_app_state_sources(app_state, sources).await?;
        Self::apply_mapping_reload(app_state, prepared_mapping);
        info!("Loaded sources file {sources_file}");
        Ok(())
    }

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
                let event_type = if matches!(self, ConfigFile::Template) {
                    ConfigType::Template
                } else {
                    ConfigType::Sources
                };
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
