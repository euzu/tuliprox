use crate::api::model::AppState;
use crate::model::Config;
use crate::model::{ApiProxyConfig, AppConfig, SourcesConfig};
use crate::repository::user_repository::{get_api_user_db_path, load_api_user};
use crate::utils::file_reader;
use crate::utils::sys_utils::exit;
use crate::utils::{open_file, read_mappings_file, EnvResolvingReader, FileLockManager};
use crate::utils;
use arc_swap::access::Access;
use arc_swap::{ArcSwap, ArcSwapAny};
use chrono::Local;
use log::{error, info, warn};
use serde::Serialize;
use shared::error::{info_err, info_err_res, TuliproxError};
use shared::model::{ApiProxyConfigDto, AppConfigDto, ConfigDto, ConfigInputAliasDto, ConfigPaths, HdHomeRunDeviceOverview, InputType, SourcesConfigDto, TargetUserDto};
use shared::utils::CONSTANTS;
use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

enum EitherReader<L, R> {
    Left(L),
    Right(R),
}

// `Read`-Trait for Either
impl<L: Read, R: Read> Read for EitherReader<L, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            EitherReader::Left(reader) => reader.read(buf),
            EitherReader::Right(reader) => reader.read(buf),
        }
    }
}

pub fn config_file_reader(file: File, resolve_env: bool) -> impl Read
{
    if resolve_env {
        EitherReader::Left(EnvResolvingReader::new(file_reader(file)))
    } else {
        EitherReader::Right(file_reader(file))
    }
}

pub async fn read_api_proxy_config(config: &AppConfig, resolve_env: bool) -> Result<Option<ApiProxyConfig>, TuliproxError> {
    let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&config.paths);
    let api_proxy_file_path = paths.api_proxy_file_path.as_str();
    if let Some(api_proxy_dto) = read_api_proxy_file(api_proxy_file_path, resolve_env)? {
        let mut errors = vec![];
        let mut api_proxy: ApiProxyConfig = ApiProxyConfig::from(&api_proxy_dto);
        api_proxy.migrate_api_user(config, &mut errors).await;
        if !errors.is_empty() {
            for error in errors {
                error!("{error}");
            }
        }
        Ok(Some(api_proxy))
    } else {
        warn!("cant read api_proxy_config file: {api_proxy_file_path}");
        Ok(None)
    }
}

pub fn read_sources_file_from_path(sources_file: &Path, resolve_env: bool, include_computed: bool, hdhr_config: Option<&HdHomeRunDeviceOverview>) -> Result<SourcesConfigDto, TuliproxError> {
    match open_file(sources_file) {
        Ok(file) => {
            let maybe_sources: Result<SourcesConfigDto, _> = serde_saphyr::from_reader(config_file_reader(file, resolve_env));
            match maybe_sources {
                Ok(mut sources) => {
                    if resolve_env {
                        if let Err(err) = sources.prepare(include_computed, hdhr_config) {
                            return info_err_res!("Can't read the sources-config file: {}: {err}", sources_file.display());
                        }
                    }
                    Ok(sources)
                }
                Err(err) => info_err_res!("Can't read the sources-config file: {}: {err}", sources_file.display())
            }
        }
        Err(err) => info_err_res!("Can't read the sources-config file: {}: {err}", sources_file.display())
    }
}


pub fn read_sources_file(sources_file: &str, resolve_env: bool, include_computed: bool, hdhr_config: Option<&HdHomeRunDeviceOverview>) -> Result<SourcesConfigDto, TuliproxError> {
    read_sources_file_from_path(&PathBuf::from(sources_file), resolve_env, include_computed, hdhr_config)
}

pub fn read_config_file(config_file: &str, resolve_env: bool, include_computed: bool) -> Result<ConfigDto, TuliproxError> {
    match open_file(&std::path::PathBuf::from(config_file)) {
        Ok(file) => {
            let maybe_config: Result<ConfigDto, _> = serde_saphyr::from_reader(config_file_reader(file, resolve_env));
            match maybe_config {
                Ok(mut config) => {
                    if resolve_env {
                        config.prepare(include_computed)?;
                    }
                    Ok(config)
                }
                Err(err) => info_err_res!("Can't read the config file: {config_file}: {err}")
            }
        }
        Err(err) => info_err_res!("Can't read the config file: {config_file}: {err}")
    }
}

pub fn read_app_config_dto(paths: &ConfigPaths,
                           resolve_env: bool,
                           include_computed: bool) -> Result<AppConfigDto, TuliproxError> {
    let config_file = paths.config_file_path.as_str();
    let sources_file = paths.sources_file_path.as_str();
    let api_proxy_file = paths.api_proxy_file_path.as_str();

    let config = read_config_file(config_file, resolve_env, include_computed)?;
    let sources = read_sources_file(sources_file, resolve_env, include_computed, config.get_hdhr_device_overview().as_ref())?;
    let mappings = if let Some(mappings_file) = paths.mapping_file_path.as_ref() {
        read_mappings_file(mappings_file, resolve_env).unwrap_or(None).map(|(_, mappings)| mappings)
    } else {
        None
    };

    let api_proxy = read_api_proxy_file(api_proxy_file, resolve_env).unwrap_or(None);

    Ok(AppConfigDto {
        config,
        sources,
        mappings,
        api_proxy,
    })
}

pub async fn prepare_sources_batch(sources: &mut SourcesConfigDto, include_computed: bool) -> Result<(), TuliproxError> {
    let mut current_index = 0;
    let max_id_in_source = sources.inputs.iter()
        .flat_map(|item| {
            std::iter::once(item.id).chain(
                item.aliases
                    .as_ref()
                    .into_iter()
                    .flatten()
                    .map(|alias| alias.id),
            )
        })
        .max()
        .unwrap_or(0);

    current_index = std::cmp::max(current_index, max_id_in_source);

    for input in &mut sources.inputs {
        match get_batch_aliases(input.input_type, input.url.as_str()).await {
            Ok(Some((_, aliases))) => {
                if let Some(idx) = input.prepare_batch(aliases, current_index)? {
                    current_index = idx;
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!("Failed to read config files aliases: {err}");
                return Err(err);
            }
        }
        // we need to prepare epg after alias, because epg `auto` depends on the first input url.
        input.prepare_epg(include_computed)?;
    }
    Ok(())
}

pub async fn get_batch_aliases(input_type: InputType, url: &str) -> Result<Option<(PathBuf, Vec<ConfigInputAliasDto>)>, TuliproxError> {
    if input_type == InputType::M3uBatch || input_type == InputType::XtreamBatch {
        return match utils::csv_read_inputs(input_type, url).await {
            Ok((file_path, batch_aliases)) => {
                Ok(Some((file_path, batch_aliases)))
            }
            Err(err) => {
                info_err_res!("{err}")
            }
        };
    }
    Ok(None)
}

pub async fn prepare_users(app_config_dto: &mut AppConfigDto, app_config: &AppConfig) -> Result<(), TuliproxError> {
    let use_user_db = app_config_dto
        .api_proxy
        .as_ref()
        .is_some_and(|p| p.use_user_db);

    if use_user_db {
        let user_db_path = get_api_user_db_path(app_config);
        if user_db_path.exists() {
            match load_api_user(app_config).await {
                Ok(stored_users) => if let Some(api_proxy) = app_config_dto.api_proxy.as_mut() {
                    api_proxy.user.extend(stored_users.iter().map(TargetUserDto::from));
                },
                Err(err) => {
                    warn!("Failed to load users from DB at {}: {err}", user_db_path.display());
                }
            }
        }
    }
    Ok(())
}

pub async fn read_initial_app_config(paths: &mut ConfigPaths,
                                     resolve_env: bool,
                                     include_computed: bool,
                                     server_mode: bool) -> Result<AppConfig, TuliproxError> {
    let config_path = paths.config_path.as_str();
    let config_file = paths.config_file_path.as_str();
    let sources_file = paths.sources_file_path.as_str();

    let config_dto = read_config_file(config_file, resolve_env, include_computed)?;
    let mut sources_dto = read_sources_file(sources_file, resolve_env, include_computed, config_dto.get_hdhr_device_overview().as_ref())?;
    prepare_sources_batch(&mut sources_dto, include_computed).await?;
    let sources: SourcesConfig = SourcesConfig::try_from(sources_dto)?;
    let mut config: Config = Config::from(config_dto);
    config.prepare(config_path)?;
    config.update_runtime();

    if paths.mapping_file_path.is_none() {
        let mut path = config.mapping_path.as_ref().map_or_else(|| utils::get_default_mappings_path(config_path), ToString::to_string);
        if resolve_env {
            path = resolve_env_var(&path);
        }
        paths.mapping_file_path.replace(path);
    }

    let mut app_config = AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapAny::default()),
        api_proxy: Arc::new(ArcSwapAny::default()),
        paths: Arc::new(ArcSwap::from_pointee(paths.clone())),
        file_locks: Arc::new(FileLockManager::default()),
        custom_stream_response: Arc::new(ArcSwapAny::default()),
        access_token_secret: Default::default(),
        encrypt_secret: Default::default(),
    };
    app_config.prepare(include_computed)?;
    //print_info(&app_config);

    if let Some(mappings_file) = &paths.mapping_file_path {
        match utils::read_mappings(mappings_file.as_str(), resolve_env) {
            Ok(Some((mapping_paths, mappings))) => {
                app_config.set_mappings(mappings_file, &mappings);
                paths.mapping_files_used = {
                    let vec: Vec<String> = mapping_paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect();

                    if vec.is_empty() { None } else { Some(vec) }
                };
                app_config.paths.store(Arc::new(paths.clone()));
            },
            Ok(None) => info!("Mapping file: not used"),
            Err(err) => exit!("{err}"),
        }
    }

    if server_mode {
        match read_api_proxy_config(&app_config, resolve_env).await {
            Ok(Some(api_proxy)) => app_config.set_api_proxy(api_proxy)?,
            Ok(None) => info!("Api-Proxy file: not used"),
            Err(err) => exit!("{err}"),
        }
    }

    Ok(app_config)
}

pub fn read_api_proxy_file(api_proxy_file: &str, resolve_env: bool) -> Result<Option<ApiProxyConfigDto>, TuliproxError> {
    open_file(&std::path::PathBuf::from(api_proxy_file)).map_or(Ok(None), |file| {
        let maybe_api_proxy: Result<ApiProxyConfigDto, _> = serde_saphyr::from_reader(config_file_reader(file, resolve_env));
        match maybe_api_proxy {
            Ok(mut api_proxy_dto) => {
                if resolve_env {
                    if let Err(err) = api_proxy_dto.prepare() {
                        exit!("cant read api-proxy-config file: {err}");
                    }
                }
                Ok(Some(api_proxy_dto))
            }
            Err(err) => {
                info_err_res!("cant read api-proxy-config file: {err}")
            }
        }
    })
}

pub async fn read_api_proxy(config: &AppConfig, resolve_env: bool) -> Option<ApiProxyConfig> {
    let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&config.paths);
    match read_api_proxy_file(paths.api_proxy_file_path.as_str(), resolve_env) {
        Ok(Some(api_proxy_dto)) => {
            let mut errors = vec![];
            let mut api_proxy: ApiProxyConfig = api_proxy_dto.into();
            api_proxy.migrate_api_user(config, &mut errors).await;
            if !errors.is_empty() {
                for error in errors {
                    error!("{error}");
                }
            }
            Some(api_proxy)
        }
        Ok(None) => None,
        Err(err) => {
            error!("Failed to read Api-Proxy file {err}");
            None
        }
    }
}

async fn write_config_file<T>(file_path: &str, backup_dir: &str, config: &T, default_name: &str) -> Result<(), TuliproxError>
where
    T: ?Sized + Serialize,
{
    let path = PathBuf::from(file_path);
    let filename = path.file_name().map_or(default_name.to_string(), |f| f.to_string_lossy().to_string());
    
    let mut serialized = String::new();
    let options = serde_saphyr::SerializerOptions {
        prefer_block_scalars: false,
        ..Default::default()
    };
    serde_saphyr::to_fmt_writer_with_options(&mut serialized, &config, options)
        .map_err(|err| info_err!("Could not serialize config: {}", err))?;

    if path.exists() {
        if let Ok(existing) = fs::read_to_string(&path).await {
            if existing == serialized {
                // info!("File {} unchanged, skipping write", path.display());
                return Ok(());
            }
        }
    }

    let backup_path = PathBuf::from(backup_dir).join(format!("{filename}_{}", Local::now().format("%Y%m%d_%H%M%S")));

    match fs::copy(&path, &backup_path).await {
        Ok(_) => {}
        Err(err) => { error!("Could not backup file {}:{}", &backup_path.to_str().unwrap_or("?"), err) }
    }
    info!("Saving file to {}", &path.to_str().unwrap_or("?"));

    fs::write(&path, serialized)
        .await
        .map_err(|err| info_err!("Could not write file {}: {}", &path.to_str().unwrap_or("?"), err))
}

pub async fn save_api_proxy(file_path: &str, backup_dir: &str, config: &ApiProxyConfigDto) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, "api-proxy.yml").await
}

pub async fn save_main_config(file_path: &str, backup_dir: &str, config: &ConfigDto) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, "config.yml").await
}

pub async fn save_sources_config<T>(file_path: &str, backup_dir: &str, config: &T) -> Result<(), TuliproxError>
where
    T: ?Sized + Serialize,
{
    write_config_file(file_path, backup_dir, config, "source.yml").await
}

pub async fn persist_source_config(app_state: &Arc<AppState>, source_file_path: Option<&Path>, doc: SourcesConfigDto) -> Result<SourcesConfigDto, TuliproxError>
{
    let source_file = {
        source_file_path
            .and_then(|p| p.to_str())
            .map_or_else(|| {
                let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
                paths.sources_file_path.clone()
            }, ToString::to_string)
    };
    let backup_dir = {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);
        config.get_backup_dir().to_string()
    };

    let mut source_config = doc.clone();
    for input in &mut source_config.inputs {
        if matches!(input.input_type, InputType::XtreamBatch | InputType::M3uBatch) && utils::is_csv_file(input.url.as_str()) {
            if let Some(aliases) = &input.aliases {
                if let Err(err) = utils::csv_write_inputs(input.url.as_str(), aliases).await {
                    error!("Could not persist aliases to csv {}: {}", input.url, err);
                }
            }
            input.aliases = None;
        }
        input.id = 0;
        if let Some(aliases) = input.aliases.as_mut() {
            for alias in aliases {
                alias.id = 0;
            }
        }
    }
    for source in &mut source_config.sources {
        for target in &mut source.targets {
            target.id = 0;
        }
    }

    save_sources_config(&source_file, &backup_dir, &source_config).await?;
    Ok(doc)
}

pub async fn validate_and_persist_source_config(app_state: &Arc<AppState>, dto: SourcesConfigDto) -> Result<SourcesConfigDto, TuliproxError> {
    {
        let mut new_dto = dto.clone();
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);
        new_dto.prepare(true, config.get_hdhr_device_overview().as_ref())?;
    }

    persist_source_config(app_state, None, dto).await
}

pub fn resolve_env_var(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    CONSTANTS.re_env_var.replace_all(value, |caps: &regex::Captures| {
        let var_name = &caps["var"];
        env::var(var_name).unwrap_or_else(|e| {
            error!("Could not resolve env var '{var_name}': {e}");
            format!("${{env:{var_name}}}")
        })
    }).to_string()
}

#[cfg(test)]
mod tests {
    use crate::utils::resolve_env_var;

    #[test]
    fn test_resolve() {
        let resolved = resolve_env_var("${env:HOME}");
        assert_eq!(resolved, std::env::var("HOME").unwrap());
    }
}
