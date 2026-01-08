use crate::api::model::{update_app_state_config, update_app_state_sources, AppState, EventMessage};
use crate::model::{Config, SourcesConfig};
use crate::utils;
use crate::utils::{prepare_sources_batch, read_config_file, read_sources_file};
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use log::{debug, error, info};
use shared::error::TuliproxError;
use shared::model::{ConfigPaths, ConfigType};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigFile {
    Config,
    ApiProxy,
    Mapping,
    Sources,
    SourceFile,
}

impl ConfigFile {
    fn load_mapping(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
        if let Some(mapping_file_path) = paths.mapping_file_path.as_ref() {
            match utils::read_mappings(mapping_file_path, true) {
                Ok(Some((mapping_files, mappings_cfg))) => {
                    app_state.app_config.set_mappings(mapping_file_path, &mappings_cfg);
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
        let mapping_changed = paths.mapping_file_path.as_ref() != config_dto.mapping_path.as_ref();
        let mut config: Config = Config::from(config_dto);
        config.prepare(paths.config_path.as_str())?;
        update_app_state_config(app_state, config).await?;
        info!("Loaded config file {config_file}");
        if mapping_changed {
            Self::load_mapping(app_state)?;
        }
        Ok(())
    }

    pub async fn load_sources(app_state: &Arc<AppState>) -> Result<(), TuliproxError> {
        let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
        let sources_file = paths.sources_file_path.as_str();
        let mut sources_dto = {
            let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);
            read_sources_file(sources_file, true, true, config.get_hdhr_device_overview().as_ref())?
        };
        prepare_sources_batch(&mut sources_dto, true).await?;
        let sources: SourcesConfig = SourcesConfig::try_from(sources_dto)?;
        update_app_state_sources(app_state, sources).await?;
        info!("Loaded sources file {sources_file}");
        // mappings are not stored, so we need to reload and apply them if sources change.
        Self::load_mapping(app_state)
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
            ConfigFile::Config => {
                ConfigFile::load_config(app_state).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::Config));
            }
            ConfigFile::Sources => {
                ConfigFile::load_sources(app_state).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::Sources));
            }
            ConfigFile::SourceFile => {
                ConfigFile::reload_source_file(app_state).await?;
                app_state.event_manager.send_event(EventMessage::ConfigChange(ConfigType::Sources));
            }
        }
        Ok(())
    }
}
