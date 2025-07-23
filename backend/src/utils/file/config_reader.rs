use crate::model::Config;
use crate::model::{ApiProxyConfig, AppConfig, SourcesConfig};
use crate::{print_info, utils};
use crate::utils::file_reader;
use crate::utils::sys_utils::exit;
use crate::utils::{open_file, read_mappings_file, EnvResolvingReader, FileLockManager};
use arc_swap::{ArcSwap, ArcSwapAny};
use chrono::Local;
use log::{error, info, warn};
use serde::Serialize;
use shared::error::{create_tuliprox_error, info_err, to_io_error, TuliproxError, TuliproxErrorKind};
use shared::model::{ApiProxyConfigDto, AppConfigDto, ConfigDto, ConfigPaths, HdHomeRunDeviceOverview, SourcesConfigDto};
use shared::utils::{CONSTANTS};
use std::env;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::PathBuf;
use std::sync::Arc;
use arc_swap::access::{Access};

enum EitherReader<L, R> {
    Left(L),
    Right(R),
}

// `Read`-Trait für Either implementieren
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
        EitherReader::Right(BufReader::new(file))
    }
}

pub fn read_api_proxy_config(config: &AppConfig, resolve_env: bool) -> Result<Option<ApiProxyConfig>, TuliproxError> {
    let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&config.paths);
    let api_proxy_file_path = paths.api_proxy_file_path.as_str();
    if let Some(api_proxy_dto) = read_api_proxy_file(api_proxy_file_path, resolve_env)? {
        let mut errors = vec![];
        let mut api_proxy: ApiProxyConfig = ApiProxyConfig::from(&api_proxy_dto);
        api_proxy.migrate_api_user(config, &mut errors);
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

pub fn read_sources_file(sources_file: &str, resolve_env: bool, include_computed: bool, hdhr_config: Option<&HdHomeRunDeviceOverview>) -> Result<SourcesConfigDto, TuliproxError> {
    match open_file(&std::path::PathBuf::from(sources_file)) {
        Ok(file) => {
            let maybe_sources: Result<SourcesConfigDto, _> = serde_yaml::from_reader(config_file_reader(file, resolve_env));
            match maybe_sources {
                Ok(mut sources) => {
                    if resolve_env {
                        if let Err(err) = sources.prepare(include_computed, hdhr_config) {
                            return Err(info_err!(format!("Can't read the sources-config file: {sources_file}: {err}")));
                        }
                    }
                    Ok(sources)
                }
                Err(err) => Err(info_err!(format!("Can't read the sources-config file: {sources_file}: {err}")))
            }
        }
        Err(err) => Err(info_err!(format!("Can't read the sources-config file: {sources_file}: {err}")))
    }
}

pub fn read_config_file(config_file: &str, resolve_env: bool) -> Result<ConfigDto, TuliproxError> {
    match open_file(&std::path::PathBuf::from(config_file)) {
        Ok(file) => {
            let maybe_config: Result<ConfigDto, _> = serde_yaml::from_reader(config_file_reader(file, resolve_env));
            match maybe_config {
                Ok(mut config) => {
                    if resolve_env {
                        config.prepare()?;
                    }
                    Ok(config)
                }
                Err(err) => Err(info_err!(format!("Can't read the config file: {config_file}: {err}")))
            }
        }
        Err(err) => Err(info_err!(format!("Can't read the config file: {config_file}: {err}")))
    }
}

pub fn read_app_config_dto(paths: &ConfigPaths,
                           resolve_env: bool,
                           include_computed: bool) -> Result<AppConfigDto, TuliproxError> {
    let config_file = paths.config_file_path.as_str();
    let sources_file = paths.sources_file_path.as_str();
    let api_proxy_file = paths.api_proxy_file_path.as_str();

    let config = read_config_file(config_file, resolve_env)?;
    let sources = read_sources_file(sources_file, resolve_env, include_computed, config.get_hdhr_device_overview().as_ref())?;
    let mappings = if let Some(mappings_file) = paths.mapping_file_path.as_ref() {
        read_mappings_file(mappings_file, resolve_env).unwrap_or(None)
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


pub fn read_initial_app_config(paths: &mut ConfigPaths,
                       resolve_env: bool,
                       include_computed: bool,
                       server_mode: bool) -> Result<AppConfig, TuliproxError> {
    let config_path = paths.config_path.as_str();
    let config_file = paths.config_file_path.as_str();
    let sources_file = paths.sources_file_path.as_str();

    let config_dto = read_config_file(config_file, resolve_env)?;
    let sources_dto = read_sources_file(sources_file, resolve_env, include_computed, config_dto.get_hdhr_device_overview().as_ref())?;
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
    print_info(&app_config);

    if let Some(mappings_file) = &paths.mapping_file_path {
        match utils::read_mappings(mappings_file.as_str(), resolve_env) {
            Ok(Some(mappings)) => app_config.set_mappings(&mappings),
            Ok(None) => info!("Mapping file: not used"),
            Err(err) => exit!("{err}"),
        }
    }

    if server_mode {
        match read_api_proxy_config(&app_config, resolve_env) {
            Ok(Some(api_proxy)) => app_config.set_api_proxy(api_proxy)?,
            Ok(None) => info!("Api-Proxy file: not used"),
            Err(err) => exit!("{err}"),
        }
    }

    Ok(app_config)
}

pub fn read_api_proxy_file(api_proxy_file: &str, resolve_env: bool) -> Result<Option<ApiProxyConfigDto>, TuliproxError> {
    open_file(&std::path::PathBuf::from(api_proxy_file)).map_or(Ok(None), |file| {
        let maybe_api_proxy: Result<ApiProxyConfigDto, _> = serde_yaml::from_reader(config_file_reader(file, resolve_env));
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
                Err(info_err!(format!("cant read api-proxy-config file: {err}")))
            }
        }
    })
}

pub fn read_api_proxy(config: &AppConfig, resolve_env: bool) -> Option<ApiProxyConfig> {
    let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&config.paths);
    match read_api_proxy_file(paths.api_proxy_file_path.as_str(), resolve_env) {
        Ok(Some(api_proxy_dto)) => {
            let mut errors = vec![];
            let mut api_proxy: ApiProxyConfig = api_proxy_dto.into();
            api_proxy.migrate_api_user(config, &mut errors);
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

fn write_config_file<T>(file_path: &str, backup_dir: &str, config: &T, default_name: &str) -> Result<(), TuliproxError>
where
    T: ?Sized + Serialize,
{
    let path = PathBuf::from(file_path);
    let filename = path.file_name().map_or(default_name.to_string(), |f| f.to_string_lossy().to_string());
    let backup_path = PathBuf::from(backup_dir).join(format!("{filename}_{}", Local::now().format("%Y%m%d_%H%M%S")));


    match std::fs::copy(&path, &backup_path) {
        Ok(_) => {}
        Err(err) => { error!("Could not backup file {}:{}", &backup_path.to_str().unwrap_or("?"), err) }
    }
    info!("Saving file to {}", &path.to_str().unwrap_or("?"));

    File::create(&path)
        .and_then(|f| serde_yaml::to_writer(f, &config).map_err(to_io_error))
        .map_err(|err| create_tuliprox_error!(TuliproxErrorKind::Info, "Could not write file {}: {}", &path.to_str().unwrap_or("?"), err))
}

pub fn save_api_proxy(file_path: &str, backup_dir: &str, config: &ApiProxyConfigDto) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, "api-proxy.yml")
}

pub fn save_main_config(file_path: &str, backup_dir: &str, config: &ConfigDto) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, "config.yml")
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