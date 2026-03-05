use crate::api::model::AppState;
use crate::model::Config;
use crate::model::{ApiProxyConfig, AppConfig, SourcesConfig};
use crate::repository::{
    csv_read_inputs, csv_write_inputs, get_api_user_db_path, is_csv_file, load_api_user,
};
use crate::utils;
use crate::utils::{file_exists_async, file_reader};
use crate::utils::sys_utils::exit;
use crate::utils::{
    open_file, read_mappings_file_unprepared, read_templates_file, EnvResolvingReader,
    FileLockManager,
};
use arc_swap::{ArcSwap, ArcSwapAny};
use chrono::Local;
use log::{error, info, warn};
use serde::Serialize;
use shared::error::{info_err, info_err_res, TuliproxError};
use shared::foundation::prepare_templates;
use shared::model::{
    ApiProxyConfigDto, AppConfigDto, ConfigDto, ConfigInputAliasDto, ConfigPaths, HdHomeRunDeviceOverview, InputType,
    MsgKind, PatternTemplate, SourcesConfigDto, TargetUserDto, TemplateDefinitionDto,
};
use shared::utils::{generate_default_access_secret, generate_default_encrypt_secret, CONSTANTS, PROVIDER_SCHEME_PREFIX, TEMPLATE_FILE};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::OnceCell;
use shared::concat_string;
use crate::utils::request::{is_uri};
use url::Url;

pub(crate) struct PreparedTemplateBundle {
    pub(crate) definition: Option<TemplateDefinitionDto>,
    pub(crate) prepared: Option<Vec<PatternTemplate>>,
    pub(crate) files_used: Option<Vec<String>>,
}

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

pub fn config_file_reader(file: File, resolve_env: bool) -> impl Read {
    if resolve_env {
        EitherReader::Left(EnvResolvingReader::new(file_reader(file)))
    } else {
        EitherReader::Right(file_reader(file))
    }
}

pub async fn read_api_proxy_config(
    config: &AppConfig,
    resolve_env: bool,
) -> Result<Option<ApiProxyConfig>, TuliproxError> {
    let paths = config.paths.load();
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
        warn!("can't read api_proxy_config file: {api_proxy_file_path}");
        Ok(None)
    }
}

async fn parse_sources_file_from_path(
    sources_file: &Path,
    resolve_env: bool,
) -> Result<SourcesConfigDto, TuliproxError> {
    let sources_file = sources_file.to_path_buf();
    tokio::task::spawn_blocking(move || match open_file(&sources_file) {
        Ok(file) => {
            let maybe_sources: Result<SourcesConfigDto, _> =
                serde_saphyr::from_reader(config_file_reader(file, resolve_env));
            match maybe_sources {
                Ok(sources) => Ok(sources),
                Err(err) => info_err_res!(
                    "Can't read the sources-config file: {}: {err}",
                    sources_file.display()
                ),
            }
        }
        Err(err) => info_err_res!(
            "Can't read the sources-config file: {}: {err}",
            sources_file.display()
        ),
    })
    .await
    .map_err(|join_err| info_err!("Failed to read sources-config file: {join_err}"))?
}

pub fn resolve_template_and_mapping_paths<'a>(paths: &'a ConfigPaths, template_path: Option<&'a str>, mapping_path: Option<&'a str>) -> (Cow<'a, str>, Cow<'a, str>) {
    // Resolve effective paths for templates and mappings (with robust fallbacks)
    let effective_template_path = paths.template_file_path.as_deref()
        .or(template_path).map_or_else(|| Cow::Owned(utils::get_default_templates_path(&paths.config_path)), Cow::Borrowed);

    let effective_mapping_path = paths.mapping_file_path.as_deref()
        .or(mapping_path).map_or_else(|| Cow::Owned(utils::get_default_mappings_path(&paths.config_path)), Cow::Borrowed);

    (effective_template_path, effective_mapping_path)
}

pub async fn read_sources_file_from_path_with_templates(
    sources_file: &Path,
    resolve_env: bool,
    include_computed: bool,
    hdhr_config: Option<&HdHomeRunDeviceOverview>,
    prepared_templates: Option<&[shared::model::PatternTemplate]>,
) -> Result<SourcesConfigDto, TuliproxError> {
    let mut sources = parse_sources_file_from_path(sources_file, resolve_env).await?;
    if resolve_env {
        if let Err(err) = sources.prepare(include_computed, hdhr_config, prepared_templates) {
            return info_err_res!(
                "Can't read the sources-config file: {}: {err}",
                sources_file.display()
            );
        }
    }
    Ok(sources)
}

pub async fn read_sources_file_from_path(
    sources_file: &Path,
    resolve_env: bool,
    include_computed: bool,
    hdhr_config: Option<&HdHomeRunDeviceOverview>,
) -> Result<SourcesConfigDto, TuliproxError> {
    read_sources_file_from_path_with_templates(sources_file, resolve_env, include_computed, hdhr_config, None).await
}

pub async fn read_sources_file(
    sources_file: &str,
    resolve_env: bool,
    include_computed: bool,
    hdhr_config: Option<&HdHomeRunDeviceOverview>,
    prepared_templates: Option<&[shared::model::PatternTemplate]>,
) -> Result<SourcesConfigDto, TuliproxError> {
    read_sources_file_from_path_with_templates(&PathBuf::from(sources_file), resolve_env, include_computed, hdhr_config, prepared_templates).await
}

pub fn read_config_file(
    config_file: &str,
    resolve_env: bool,
    include_computed: bool,
) -> Result<ConfigDto, TuliproxError> {
    match open_file(&std::path::PathBuf::from(config_file)) {
        Ok(file) => {
            let maybe_config: Result<ConfigDto, _> =
                serde_saphyr::from_reader(config_file_reader(file, resolve_env));
            match maybe_config {
                Ok(mut config) => {
                    if resolve_env {
                        config.prepare(include_computed)?;
                    }
                    Ok(config)
                }
                Err(err) => info_err_res!("Can't read the config file: {config_file}: {err}"),
            }
        }
        Err(err) => info_err_res!("Can't read the config file: {config_file}: {err}"),
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn read_templates(
    template_file_path: Option<&str>,
    resolve_env: bool,
    sources_inline_templates: Option<&[PatternTemplate]>,
    mappings_inline_templates: Option<&[PatternTemplate]>,
) -> Result<PreparedTemplateBundle, TuliproxError> {
    struct TemplateNameUsage {
        count: usize,
        sources: HashSet<String>,
    }

    fn track_template_name(
        usage: &mut HashMap<String, TemplateNameUsage>,
        template_name: &str,
        source: &str,
    ) {
        let entry = usage
            .entry(template_name.to_string())
            .or_insert_with(|| TemplateNameUsage {
                count: 0,
                sources: HashSet::new(),
            });
        entry.count = entry.count.saturating_add(1);
        entry.sources.insert(source.to_string());
    }

    let mut loaded_template_files: Vec<String> = Vec::new();
    let mut merged_templates: Vec<PatternTemplate> = Vec::new();
    let mut template_name_usage: HashMap<String, TemplateNameUsage> = HashMap::new();

    if let Some(path) = template_file_path {
        if let Some((template_paths, definition)) = read_templates_file(path, resolve_env)? {
            let file_source_label = if template_paths.is_empty() {
                format!("template file: {path}")
            } else if template_paths.len() == 1 {
                format!("template file: {}", template_paths[0].display())
            } else {
                format!(
                    "template files: {}",
                    template_paths
                        .iter()
                        .map(|template_path| template_path.display().to_string())
                        .collect::<Vec<String>>()
                        .join(", ")
                )
            };
            for template in &definition.templates {
                track_template_name(
                    &mut template_name_usage,
                    template.name.as_str(),
                    file_source_label.as_str(),
                );
            }
            merged_templates.extend(definition.templates);
            loaded_template_files.extend(template_paths.iter().map(|p| p.display().to_string()));
        }
    }

    if let Some(templates) = sources_inline_templates {
        for template in templates {
            track_template_name(
                &mut template_name_usage,
                template.name.as_str(),
                "source.yml inline templates",
            );
        }
        merged_templates.extend(templates.iter().cloned());
    }
    if let Some(templates) = mappings_inline_templates {
        for template in templates {
            track_template_name(
                &mut template_name_usage,
                template.name.as_str(),
                "mapping.yml inline templates",
            );
        }
        merged_templates.extend(templates.iter().cloned());
    }

    let mut duplicate_templates = template_name_usage
        .iter()
        .filter_map(|(name, usage)| {
            if usage.count > 1 {
                let mut sources = usage.sources.iter().cloned().collect::<Vec<String>>();
                sources.sort();
                Some(format!("{name} [{}]", sources.join(", ")))
            } else {
                None
            }
        })
        .collect::<Vec<String>>();
    if !duplicate_templates.is_empty() {
        duplicate_templates.sort();
        return info_err_res!(
            "Duplicate template names found across merged template sources: {}",
            duplicate_templates.join("; ")
        );
    }

    let files_used = if loaded_template_files.is_empty() {
        None
    } else {
        Some(loaded_template_files)
    };

    if merged_templates.is_empty() {
        return Ok(PreparedTemplateBundle {
            definition: None,
            prepared: None,
            files_used,
        });
    }

    let mut prepared_templates = merged_templates.clone();
    prepare_templates(&mut prepared_templates)?;

    Ok(PreparedTemplateBundle {
        definition: Some(TemplateDefinitionDto {
            templates: merged_templates,
        }),
        prepared: Some(prepared_templates),
        files_used,
    })
}

pub async fn read_app_config_dto(
    paths: &ConfigPaths,
    resolve_env: bool,
    include_computed: bool,
) -> Result<AppConfigDto, TuliproxError> {
    let config_file = paths.config_file_path.as_str();
    let sources_file = paths.sources_file_path.as_str();
    let api_proxy_file = paths.api_proxy_file_path.as_str();

    let config = read_config_file(config_file, resolve_env, include_computed)?;

    // Resolve effective paths for templates and mappings (with robust fallbacks)
    let (effective_template_path,effective_mapping_path) = resolve_template_and_mapping_paths(paths, config.template_path.as_deref(), config.mapping_path.as_deref());

    let mut sources = parse_sources_file_from_path(&PathBuf::from(sources_file), resolve_env).await?;
    let mut mappings = read_mappings_file_unprepared(effective_mapping_path.as_ref(), resolve_env)?
        .map(|(_, mapping)| mapping);

    let template_bundle = read_templates(
        Some(effective_template_path.as_ref()),
        resolve_env,
        sources.templates.as_deref(),
        mappings
            .as_ref()
            .and_then(|mapping| mapping.mappings.templates.as_deref()),
    )?;
    let templates = template_bundle.definition;
    let prepared_templates = template_bundle.prepared;

    if resolve_env {
        sources.prepare(
            include_computed,
            config.get_hdhr_device_overview().as_ref(),
            prepared_templates.as_deref(),
        )?;
        if let Some(mapping) = mappings.as_mut() {
            mapping
                .mappings
                .prepare(prepared_templates.as_deref())?;
        }
    }

    // Keep templates centralized in AppConfigDto.templates.
    sources.templates = None;
    if let Some(mapping) = mappings.as_mut() {
        mapping.mappings.templates = None;
    }

    let api_proxy = read_api_proxy_file(api_proxy_file, resolve_env).unwrap_or(None);

    Ok(AppConfigDto {
        config,
        sources,
        mappings,
        templates,
        api_proxy,
    })
}

fn apply_prepared_mappings(
    app_config: &mut AppConfig,
    paths: &mut ConfigPaths,
    mappings_file: &str,
    mapping_paths: Option<&Vec<PathBuf>>,
    mapping: &mut shared::model::MappingsDto,
    prepared_templates: Option<&[PatternTemplate]>,
) {
    match mapping.mappings.prepare(prepared_templates) {
        Ok(()) => {
            let mappings: crate::model::Mappings = crate::model::Mappings::from(&*mapping);
            app_config.set_mappings(mappings_file, &mappings);
            paths.mapping_files_used = mapping_paths.map(|items| {
                items
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<String>>()
            });
        }
        Err(err) => {
            error!(
                "Failed to prepare mapping '{mappings_file}': {err}. Skipping mapping registration."
            );
            paths.mapping_files_used = None;
        }
    }
}

pub async fn prepare_sources_batch(
    sources: &mut SourcesConfigDto,
    include_computed: bool,
) -> Result<(), TuliproxError> {
    let mut current_index = 0;
    let max_id_in_source = sources
        .inputs
        .iter()
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
        input.prepare_type()?;
        // we need to prepare epg after alias, because epg `auto` depends on the first input url.
        input.prepare_epg(include_computed)?;
    }
    Ok(())
}

pub async fn get_batch_aliases(
    input_type: InputType,
    url: &str,
) -> Result<Option<(PathBuf, Vec<ConfigInputAliasDto>)>, TuliproxError> {
    if input_type == InputType::M3uBatch || input_type == InputType::XtreamBatch {
        if url.starts_with(PROVIDER_SCHEME_PREFIX) {
            return info_err_res!(
                "Batch input type '{input_type}' does not support provider:// URLs. \
Use a local CSV path (absolute/relative) or file:// URL."
            );
        }

        return match csv_read_inputs(input_type, url).await {
            Ok((file_path, batch_aliases)) => Ok(Some((file_path, batch_aliases))),
            Err(err) => {
                info_err_res!("{err}")
            }
        };
    }
    Ok(None)
}

pub async fn prepare_users(
    app_config_dto: &mut AppConfigDto,
    app_config: &AppConfig,
) -> Result<(), TuliproxError> {
    let use_user_db = app_config_dto
        .api_proxy
        .as_ref()
        .is_some_and(|p| p.use_user_db);

    if use_user_db {
        let user_db_path = get_api_user_db_path(app_config);
        if user_db_path.exists() {
            match load_api_user(app_config).await {
                Ok(stored_users) => {
                    if let Some(api_proxy) = app_config_dto.api_proxy.as_mut() {
                        api_proxy
                            .user
                            .extend(stored_users.iter().map(TargetUserDto::from));
                    }
                }
                Err(err) => {
                    warn!(
                        "Failed to load users from DB at {}: {err}",
                        user_db_path.display()
                    );
                }
            }
        }
    }
    Ok(())
}

pub async fn read_initial_app_config(
    paths: &mut ConfigPaths,
    resolve_env: bool,
    include_computed: bool,
    server_mode: bool,
) -> Result<AppConfig, TuliproxError> {
    let config_path = paths.config_path.as_str();
    let config_file = paths.config_file_path.as_str();
    let sources_file = paths.sources_file_path.as_str();

    let config_dto = read_config_file(config_file, resolve_env, include_computed)?;

    if paths.mapping_file_path.is_none() {
        let mut path = config_dto.mapping_path.as_ref().map_or_else(
            || utils::get_default_mappings_path(config_path),
            ToString::to_string,
        );
        if resolve_env {
            path = resolve_env_var(&path);
        }
        paths.mapping_file_path.replace(path);
    }
    if paths.template_file_path.is_none() {
        let mut path = config_dto.template_path.as_ref().map_or_else(
            || utils::get_default_templates_path(config_path),
            ToString::to_string,
        );
        if resolve_env {
            path = resolve_env_var(&path);
        }
        paths.template_file_path.replace(path);
    }

    let mut sources_dto = parse_sources_file_from_path(&PathBuf::from(sources_file), resolve_env).await?;

    let (mapping_paths, mut mappings_dto) = if let Some(mappings_file) = &paths.mapping_file_path {
        match read_mappings_file_unprepared(mappings_file.as_str(), resolve_env) {
            Ok(Some((mapping_paths, mappings))) => (Some(mapping_paths), Some(mappings)),
            Ok(None) => (None, None),
            Err(err) => return Err(err),
        }
    } else {
        (None, None)
    };

    let template_bundle = read_templates(
        paths.template_file_path.as_deref().or(config_dto.template_path.as_deref()),
        resolve_env,
        sources_dto.templates.as_deref(),
        mappings_dto
            .as_ref()
            .and_then(|mapping| mapping.mappings.templates.as_deref()),
    )?;
    let prepared_templates = template_bundle.prepared;
    let template_files_used = template_bundle.files_used;

    prepare_sources_batch(&mut sources_dto, include_computed).await?;

    if resolve_env {
        sources_dto.prepare(
            include_computed,
            config_dto.get_hdhr_device_overview().as_ref(),
            prepared_templates.as_deref(),
        )?;
    }
    sources_dto.templates = None;

    let sources: SourcesConfig = SourcesConfig::try_from(sources_dto)?;
    let mut config: Config = Config::from(config_dto);
    config.prepare(config_path, paths.home_path.as_str())?;
    paths.storage_path = config.storage_dir.clone();
    config.update_runtime();

    let mut app_config = AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapAny::default()),
        api_proxy: Arc::new(ArcSwapAny::default()),
        paths: Arc::new(ArcSwap::from_pointee(paths.clone())),
        file_locks: Arc::new(FileLockManager::default()),
        custom_stream_response: Arc::new(ArcSwapAny::default()),
        access_token_secret: generate_default_access_secret(),
        encrypt_secret: generate_default_encrypt_secret(),
        ffprobe_available: Arc::new(OnceCell::new())
    };
    app_config.prepare(include_computed)?;
    //print_info(&app_config);

    if let Some(mappings_file) = paths.mapping_file_path.clone() {
        if let Some(mapping) = mappings_dto.as_mut() {
            apply_prepared_mappings(
                &mut app_config,
                paths,
                mappings_file.as_str(),
                mapping_paths.as_ref(),
                mapping,
                prepared_templates.as_deref(),
            );
        } else {
            info!("Mapping file: not used");
        }
    }

    paths.template_files_used = template_files_used;
    app_config.paths.store(Arc::new(paths.clone()));

    if server_mode {
        match read_api_proxy_config(&app_config, resolve_env).await {
            Ok(Some(api_proxy)) => app_config.set_api_proxy(api_proxy)?,
            Ok(None) => info!("Api-Proxy file: not used"),
            Err(err) => exit!("{err}"),
        }
    }

    Ok(app_config)
}

pub fn read_api_proxy_file(
    api_proxy_file: &str,
    resolve_env: bool,
) -> Result<Option<ApiProxyConfigDto>, TuliproxError> {
    open_file(&std::path::PathBuf::from(api_proxy_file)).map_or(Ok(None), |file| {
        let maybe_api_proxy: Result<ApiProxyConfigDto, _> =
            serde_saphyr::from_reader(config_file_reader(file, resolve_env));
        match maybe_api_proxy {
            Ok(mut api_proxy_dto) => {
                if resolve_env {
                    if let Err(err) = api_proxy_dto.prepare() {
                        exit!("can't read api-proxy-config file: {err}");
                    }
                }
                Ok(Some(api_proxy_dto))
            }
            Err(err) => {
                info_err_res!("can't read api-proxy-config file: {err}")
            }
        }
    })
}

pub async fn read_api_proxy(config: &AppConfig, resolve_env: bool) -> Option<ApiProxyConfig> {
    let paths = config.paths.load();
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

async fn write_config_file<T>(
    file_path: &str,
    backup_dir: &str,
    config: &T,
    default_name: &str,
) -> Result<(), TuliproxError>
where
    T: ?Sized + Serialize,
{
    let path = PathBuf::from(file_path);
    let filename = path.file_name().map_or(default_name.to_string(), |f| {
        f.to_string_lossy().to_string()
    });

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

    if path.exists() {
        let backup_path = PathBuf::from(backup_dir).join(format!("{filename}_{}", Local::now().format("%Y%m%d_%H%M%S")));

        match fs::copy(&path, &backup_path).await {
            Ok(_) => {}
            Err(err) => {
                error!("Could not backup file {}:{err}", &backup_path.to_str().unwrap_or("?"));
            }
        }
        info!("Saving file to {}", &path.to_str().unwrap_or("?"));
    }

    let parent_dir = path.parent().ok_or_else(|| { info_err!("Could not write file {}: missing parent directory", &path.to_str().unwrap_or("?"))})?;

    let dest_file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(default_name);

    let mut tmp_path = parent_dir.to_path_buf();
    tmp_path.push(format!(".{dest_file_name}.tmp-{}-{}", std::process::id(), Local::now().timestamp_nanos_opt().unwrap_or_default()));

    fs::write(&tmp_path, serialized).await.map_err(|err| { info_err!("Could not write temp file {}: {err}", &tmp_path.to_str().unwrap_or("?"))})?;

    match fs::rename(&tmp_path, &path).await {
        Ok(()) => Ok(()),
        Err(err) => {
            // Windows doesn't allow overwriting an existing file via rename.
            #[cfg(windows)]
            {
                if fs::remove_file(&path).await.is_ok() {
                    if fs::rename(&tmp_path, &path).await.is_ok() {
                        return Ok(());
                    }
                }
            }

            // Best-effort cleanup; if the temp file can't be removed, ignore it.
            let _ = fs::remove_file(&tmp_path).await;
            Err(info_err!(
                "Could not replace file {} with {}: {err}",
                &path.to_str().unwrap_or("?"),
                &tmp_path.to_str().unwrap_or("?")
            ))
        }
    }
}

pub async fn save_api_proxy(
    file_path: &str,
    backup_dir: &str,
    config: &ApiProxyConfigDto,
) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, "api-proxy.yml").await
}

pub async fn save_main_config(
    file_path: &str,
    backup_dir: &str,
    config: &ConfigDto,
) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, "config.yml").await
}

pub async fn save_sources_config<T>(
    file_path: &str,
    backup_dir: &str,
    config: &T,
) -> Result<(), TuliproxError>
where
    T: ?Sized + Serialize,
{
    write_config_file(file_path, backup_dir, config, "source.yml").await
}

pub async fn save_templates_config(
    file_path: &str,
    backup_dir: &str,
    config: &TemplateDefinitionDto,
) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, TEMPLATE_FILE).await
}

async fn build_templates_to_persist(
    app_state: &Arc<AppState>,
    dto: &SourcesConfigDto,
) -> Result<Option<TemplateDefinitionDto>, TuliproxError> {
    let mut new_dto = dto.clone();
    let config = app_state.app_config.config.load();
    let paths = app_state.app_config.paths.load();

    let existing_source_inline_templates = match parse_sources_file_from_path(
        Path::new(paths.sources_file_path.as_str()),
        true,
    )
    .await
    {
        Ok(existing_sources) => existing_sources.templates,
        Err(err) => {
            warn!(
                "Failed to read existing source.yml for template migration '{}': {err}",
                paths.sources_file_path
            );
            None
        }
    };

    let mapping_inline_templates = if let Some(mapping_file_path) = paths.mapping_file_path.as_ref() {
        read_mappings_file_unprepared(mapping_file_path, true)?
            .map(|(_, mapping)| mapping)
            .and_then(|mapping| mapping.mappings.templates)
    } else {
        None
    };

    let source_inline_templates = if new_dto.templates.is_some() {
        new_dto.templates.as_deref()
    } else {
        existing_source_inline_templates.as_deref()
    };

    let template_file_path = paths.template_file_path.as_deref().or(config.template_path.as_deref());
    let template_bundle = read_templates(
        template_file_path,
        true,
        source_inline_templates,
        mapping_inline_templates.as_deref(),
    )?;
    let prepared_templates = template_bundle.prepared;

    new_dto.prepare(
        true,
        config.get_hdhr_device_overview().as_ref(),
        prepared_templates.as_deref(),
    )?;

    if let Some(templates) = new_dto.templates.clone() {
        if templates.is_empty() {
            Ok(None)
        } else {
            Ok(Some(TemplateDefinitionDto { templates }))
        }
    } else {
        Ok(existing_source_inline_templates
            .filter(|templates| !templates.is_empty())
            .map(|templates| TemplateDefinitionDto { templates }))
    }
}

pub async fn validate_source_config_for_persist(
    app_state: &Arc<AppState>,
    dto: &SourcesConfigDto,
) -> Result<Option<TemplateDefinitionDto>, TuliproxError> {
    build_templates_to_persist(app_state, dto).await
}

pub async fn persist_templates_config(
    app_state: &Arc<AppState>,
    template_definition: &TemplateDefinitionDto,
) -> Result<(), TuliproxError> {
    let config = app_state.app_config.config.load();
    let paths = app_state.app_config.paths.load();
    let template_file = utils::resolve_template_persist_file_path(
        paths.template_file_path.as_deref().or(config.template_path.as_deref()),
        &paths.config_path,
    );
    save_templates_config(&template_file, config.get_backup_dir().as_ref(), template_definition).await
}

pub async fn persist_source_config(
    app_state: &Arc<AppState>,
    source_file_path: Option<&Path>,
    doc: SourcesConfigDto,
) -> Result<SourcesConfigDto, TuliproxError> {
    let source_file = {
        source_file_path.and_then(|p| p.to_str()).map_or_else(
            || {
                let paths = app_state.app_config.paths.load();
                paths.sources_file_path.clone()
            },
            ToString::to_string,
        )
    };
    let backup_dir = {
        let config = app_state.app_config.config.load();
        config.get_backup_dir().to_string()
    };

    let source_config = sanitize_sources_for_persist(doc.clone()).await;

    save_sources_config(&source_file, &backup_dir, &source_config).await?;
    Ok(doc)
}

pub async fn sanitize_sources_for_persist(mut source_config: SourcesConfigDto) -> SourcesConfigDto {
    source_config.templates = None;
    for input in &mut source_config.inputs {
        if input
            .panel_api
            .as_ref()
            .is_some_and(|panel| panel.alias_pool.is_some())
        {
            if let Some(aliases) = input.aliases.as_mut() {
                aliases.sort_by(|a, b| {
                    let a_ts = a.exp_date.unwrap_or(i64::MIN);
                    let b_ts = b.exp_date.unwrap_or(i64::MIN);
                    b_ts.cmp(&a_ts).then_with(|| a.name.cmp(&b.name))
                });
            }
        }
        if matches!(
            input.input_type,
            InputType::XtreamBatch | InputType::M3uBatch
        ) && is_csv_file(input.url.as_str())
        {
            if let Some(aliases) = &input.aliases {
                if let Err(err) = csv_write_inputs(input.url.as_str(), aliases).await {
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
    source_config
}

pub async fn validate_and_persist_source_config(
    app_state: &Arc<AppState>,
    dto: SourcesConfigDto,
) -> Result<SourcesConfigDto, TuliproxError> {
    let templates_to_persist = validate_source_config_for_persist(app_state, &dto).await?;

    if let Some(template_definition) = templates_to_persist.as_ref() {
        persist_templates_config(app_state, template_definition).await?;
    }

    persist_source_config(app_state, None, dto).await
}

pub fn resolve_env_var(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    CONSTANTS
        .re_env_var
        .replace_all(value, |caps: &regex::Captures| {
            let var_name = &caps["var"];
            env::var(var_name).unwrap_or_else(|e| {
                error!("Could not resolve env var '{var_name}': {e}");
                format!("${{env:{var_name}}}")
            })
        })
        .to_string()
}

pub async fn persist_messaging_templates(app_state: &Arc<AppState>, cfg: &mut ConfigDto) -> Result<(), TuliproxError> {
    let templates_dir = {
        let paths = app_state.app_config.paths.load();
        PathBuf::from(&paths.config_path).join("messaging_templates")
    };

    if let Some(messaging) = &mut cfg.messaging {
        // Discord
        if let Some(discord) = &mut messaging.discord {
            for (kind, template) in &mut discord.templates {
                *template = persist_single_template("discord", Some(kind), template, &templates_dir).await?;
            }
        }
        // Telegram
        if let Some(telegram) = &mut messaging.telegram {
            for (kind, template) in &mut telegram.templates {
                *template = persist_single_template("telegram", Some(kind), template, &templates_dir).await?;
            }
        }
        // Rest
        if let Some(rest) = &mut messaging.rest {
            for (kind, template) in &mut rest.templates {
                *template = persist_single_template("rest", Some(kind), template, &templates_dir).await?;
            }
        }
    }
    Ok(())
}


async fn persist_single_template(prefix: &str, kind: Option<&MsgKind>, template: &str, templates_dir: &Path) -> Result<String, TuliproxError> {
    if template.is_empty() || is_uri(template) {
        return Ok(template.to_string());
    }

    // Treat existing file paths as file URLs
    if tokio::fs::metadata(template).await.is_ok() {
        if let Ok(canonical_path) = tokio::fs::canonicalize(template).await {
            if let Ok(url) = Url::from_file_path(&canonical_path) {
                return Ok(url.to_string());
            }
        }
        return Ok(template.to_string());
    }

    // It's a raw string, persist it
    if !file_exists_async(templates_dir).await {
        tokio::fs::create_dir_all(templates_dir)
            .await
            .map_err(|e| info_err!("Messaging templates dir: failed to create dir: {} {e}", templates_dir.display()))?;
    }

    let filename = if let Some(k) = kind {
        k.template_filename(prefix)
    } else {
        concat_string!(prefix, "_default.templ")
    };

    let file_path = templates_dir.join(filename);
    fs::write(&file_path, template).await.map_err(|e| info_err!("Failed to write template file: {e}"))?;

    Url::from_file_path(&file_path)
        .map(|u| u.to_string())
        .map_err(|()| info_err!("Failed to convert persisted path to file URL: {}", file_path.display()))
}


#[cfg(test)]
mod tests {
    use crate::utils::{file::config_reader::get_batch_aliases, resolve_env_var};
    use shared::model::InputType;

    #[test]
    #[allow(clippy::manual_unwrap_or_default)]
    fn test_resolve() {
        // Use PATH which exists on both Windows and Unix
        let resolved = resolve_env_var("${env:PATH}");
        let expected = match std::env::var("PATH") {
            Ok(value) => value,
            Err(_) => String::new(),
        };
        assert_eq!(resolved, expected);
    }

    #[tokio::test]
    async fn batch_provider_scheme_returns_clear_error() {
        let result = get_batch_aliases(InputType::XtreamBatch, "provider://my_provider").await;
        let err = result.expect_err("provider:// must not be accepted for batch inputs");
        assert!(err
            .to_string()
            .contains("does not support provider:// URLs"));
    }

}
