use crate::api::model::{update_app_state_config, update_app_state_sources, AppState, EventMessage};
use crate::model::{Config, SourcesConfig};
use crate::utils;
use crate::utils::{
    read_templates, prepare_sources_batch, read_config_file, read_mappings_file_unprepared,
    read_mappings_file_with_templates, read_sources_file, read_sources_file_from_path_with_templates,
};
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use log::{debug, error, info};
use shared::error::TuliproxError;
use shared::model::{ConfigPaths, ConfigType, PatternTemplate};
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

impl ConfigFile {
    fn load_prepared_global_templates(app_state: &Arc<AppState>) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
        let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);

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

    fn load_mapping_with_templates(
        app_state: &Arc<AppState>,
        prepared_templates: Option<&[PatternTemplate]>,
    ) -> Result<(), TuliproxError> {
        let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
        if let Some(mapping_file_path) = paths.mapping_file_path.as_ref() {
            match read_mappings_file_with_templates(mapping_file_path, true, prepared_templates) {
                Ok(Some((mapping_files, mappings_cfg))) => {
                    let mappings = crate::model::Mappings::from(&mappings_cfg);
                    app_state.app_config.set_mappings(mapping_file_path, &mappings);
                    for mapping_file in mapping_files {
                        info!("Loaded mapping file {}", mapping_file.display());
                    }
                }
                Ok(None) => {
                    info!("No mapping file loaded {mapping_file_path}");
                }
                Err(err) => {
                    error!("Failed to load mapping file {err}");
                    return Err(err);
                }
            }
        }
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
                let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
                info!("Loaded Api Proxy File: {:?}", &paths.api_proxy_file_path);
            }
            Ok(None) => {
                let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
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
        let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
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
        update_app_state_config(app_state, config).await?;
        info!("Loaded config file {config_file}");
        if template_changed {
            Self::load_sources(app_state).await?;
        } else if mapping_changed {
            Self::load_mapping(app_state)?;
        }
        Ok(())
    }

    pub async fn load_sources(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
        let sources_file = paths.sources_file_path.as_str();
        let prepared_templates = Self::load_prepared_global_templates(app_state)?;
        let mut sources_dto = {
            let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);
            read_sources_file_from_path_with_templates(
                &PathBuf::from(sources_file),
                true,
                true,
                config.get_hdhr_device_overview().as_ref(),
                prepared_templates.as_deref(),
            )?
        };
        prepare_sources_batch(&mut sources_dto, true).await?;
        let sources: SourcesConfig = SourcesConfig::try_from(sources_dto)?;
        update_app_state_sources(app_state, sources).await?;
        info!("Loaded sources file {sources_file}");
        // mappings are not stored, so we need to reload and apply them if sources change.
        Self::load_mapping_with_templates(app_state, prepared_templates.as_deref())
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
