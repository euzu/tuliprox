pub mod runtime_config_report;

use arc_swap::{ArcSwap, ArcSwapAny};
use chrono::Local;
use log::{error, info, warn};
use serde::Serialize;
use shared::{
    concat_string,
    defaults::{generate_default_access_secret, generate_default_encrypt_secret, TEMPLATE_FILE},
    error::TuliproxError,
    foundation::prepare_templates,
    model::{
        ApiProxyConfigDto, AppConfigDto, ConfigDto, ConfigInputAliasDto, ConfigPaths, HdHomeRunDeviceOverview,
        InputType, MsgKind, PatternTemplate, PlansConfigDto, Prepare, SourcesConfigDto, TargetUserDto,
        TemplateDefinitionDto, UserPlanDto,
    },
    utils::PROVIDER_SCHEME_PREFIX,
};
use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::fs;
use tuliprox_core::{
    model::{ApiProxyConfig, AppConfig, Config, MediaToolCapabilities, SourcesConfig, UserPlan},
    utils,
    utils::{
        config_file_reader, exit, file_exists_async, open_file, read_mappings_file_unprepared, read_templates_file,
        request::is_uri, resolve_env_var, FileLockManager,
    },
};
use tuliprox_repository::{
    backup_api_user_db_file, csv_read_inputs, csv_write_inputs, get_api_user_db_path, is_csv_file, load_api_user,
    merge_api_user,
};
use url::Url;

/// Options controlling how configuration files are parsed.
///
/// Prefer constructing this struct over passing individual `bool` literals to
/// `read_config_file`, `read_sources_file_from_path`, etc. — bare `true`/`false`
/// arguments at call sites are hard to read without an IDE hover.
///
/// # Example
/// ```ignore
/// // Before (opaque):
/// read_sources_file_from_path(&path, true, true, hdhr).await?;
///
/// // After (self-documenting):
/// let opts = ReadConfigOptions::resolve_and_compute();
/// read_sources_file_from_path(&path, opts.resolve_env, opts.include_computed, hdhr).await?;
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ReadConfigOptions {
    // When `true`, `${ENV_VAR}` placeholders in config files are expanded.
    pub resolve_env: bool,
    // When `true`, computed / derived fields (e.g. auto EPG URLs) are filled in.
    pub include_computed: bool,
}

impl ReadConfigOptions {
    // Expand environment variables and include all computed fields — the typical
    // production default.
    #[must_use]
    pub const fn resolve_and_compute() -> Self { Self { resolve_env: true, include_computed: true } }

    // Parse the raw YAML without any variable expansion or computed fields —
    // useful when you need the literal, unexpanded content (e.g. before saving).
    #[must_use]
    pub const fn raw() -> Self { Self { resolve_env: false, include_computed: false } }
}

pub struct PreparedTemplateBundle {
    pub definition: Option<TemplateDefinitionDto>,
    pub prepared: Option<Vec<PatternTemplate>>,
    pub files_used: Option<Vec<String>>,
}

// `Read`-Trait for Either

pub async fn read_api_proxy_config(
    config: &AppConfig,
    resolve_env: bool,
) -> Result<Option<ApiProxyConfig>, TuliproxError> {
    let paths = config.paths.load();
    let api_proxy_file_path = paths.api_proxy_file_path.as_str();
    if let Some(api_proxy_dto) = read_api_proxy_file(api_proxy_file_path, resolve_env)? {
        let mut errors = vec![];
        let mut api_proxy: ApiProxyConfig = ApiProxyConfig::from(&api_proxy_dto);
        apply_authoritative_plans(config, &mut api_proxy, resolve_env).await;
        migrate_api_user(&mut api_proxy, config, &mut errors).await;
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
                Err(err) => Err(TuliproxError::ConfigSource(format!(
                    "Can't read the sources-config file: {}: {err}",
                    sources_file.display()
                ))),
            }
        }
        Err(err) => Err(TuliproxError::ConfigSource(format!(
            "Can't read the sources-config file: {}: {err}",
            sources_file.display()
        ))),
    })
    .await
    .map_err(|join_err| TuliproxError::ConfigSource(format!("Failed to read sources-config file: {join_err}")))?
}

pub fn resolve_template_and_mapping_paths(
    paths: &ConfigPaths,
    template_path: Option<&str>,
    mapping_path: Option<&str>,
) -> (String, String) {
    let effective_template_path = utils::resolve_template_file_path(
        paths.config_path.as_str(),
        paths.template_file_path.as_deref().or(template_path),
    );
    let effective_mapping_path = utils::resolve_mapping_file_path(
        paths.config_path.as_str(),
        paths.mapping_file_path.as_deref().or(mapping_path),
    );
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
            return Err(TuliproxError::Config(format!(
                "Can't read the sources-config file: {}: {err}",
                sources_file.display()
            )));
        }
    }
    Ok(sources)
}

pub async fn read_sources_file_from_path_with_options(
    sources_file: &Path,
    opts: ReadConfigOptions,
    hdhr_config: Option<&HdHomeRunDeviceOverview>,
    prepared_templates: Option<&[shared::model::PatternTemplate]>,
) -> Result<SourcesConfigDto, TuliproxError> {
    read_sources_file_from_path_with_templates(
        sources_file,
        opts.resolve_env,
        opts.include_computed,
        hdhr_config,
        prepared_templates,
    )
    .await
}

pub async fn read_sources_file_from_path(
    sources_file: &Path,
    resolve_env: bool,
    include_computed: bool,
    hdhr_config: Option<&HdHomeRunDeviceOverview>,
) -> Result<SourcesConfigDto, TuliproxError> {
    read_sources_file_from_path_with_options(
        sources_file,
        ReadConfigOptions { resolve_env, include_computed },
        hdhr_config,
        None,
    )
    .await
}

pub async fn read_sources_file_with_options(
    sources_file: &str,
    opts: ReadConfigOptions,
    hdhr_config: Option<&HdHomeRunDeviceOverview>,
    prepared_templates: Option<&[shared::model::PatternTemplate]>,
) -> Result<SourcesConfigDto, TuliproxError> {
    read_sources_file_from_path_with_options(&PathBuf::from(sources_file), opts, hdhr_config, prepared_templates).await
}

pub async fn read_sources_file(
    sources_file: &str,
    resolve_env: bool,
    include_computed: bool,
    hdhr_config: Option<&HdHomeRunDeviceOverview>,
    prepared_templates: Option<&[shared::model::PatternTemplate]>,
) -> Result<SourcesConfigDto, TuliproxError> {
    read_sources_file_with_options(
        sources_file,
        ReadConfigOptions { resolve_env, include_computed },
        hdhr_config,
        prepared_templates,
    )
    .await
}

pub fn read_config_file_with_options(config_file: &str, opts: ReadConfigOptions) -> Result<ConfigDto, TuliproxError> {
    match open_file(&std::path::PathBuf::from(config_file)) {
        Ok(file) => {
            let maybe_config: Result<ConfigDto, _> =
                serde_saphyr::from_reader(config_file_reader(file, opts.resolve_env));
            match maybe_config {
                Ok(mut config) => {
                    if opts.resolve_env {
                        config.prepare(opts.include_computed)?;
                    }
                    Ok(config)
                }
                Err(err) => Err(TuliproxError::Config(format!("Can't read the config file: {config_file}: {err}"))),
            }
        }
        Err(err) => Err(TuliproxError::Config(format!("Can't read the config file: {config_file}: {err}"))),
    }
}

pub fn read_config_file(
    config_file: &str,
    resolve_env: bool,
    include_computed: bool,
) -> Result<ConfigDto, TuliproxError> {
    read_config_file_with_options(config_file, ReadConfigOptions { resolve_env, include_computed })
}

#[allow(clippy::too_many_lines)]
pub fn read_templates(
    template_file_path: Option<&str>,
    resolve_env: bool,
    sources_inline_templates: Option<&[PatternTemplate]>,
    mappings_inline_templates: Option<&[PatternTemplate]>,
) -> Result<PreparedTemplateBundle, TuliproxError> {
    struct TemplateNameUsage {
        count: usize,
        sources: HashSet<String>,
    }

    fn track_template_name(usage: &mut HashMap<String, TemplateNameUsage>, template_name: &str, source: &str) {
        let entry = usage
            .entry(template_name.to_string())
            .or_insert_with(|| TemplateNameUsage { count: 0, sources: HashSet::new() });
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
                track_template_name(&mut template_name_usage, template.name.as_str(), file_source_label.as_str());
            }
            merged_templates.extend(definition.templates);
            loaded_template_files.extend(template_paths.iter().map(|p| p.display().to_string()));
        }
    }

    if let Some(templates) = sources_inline_templates {
        for template in templates {
            track_template_name(&mut template_name_usage, template.name.as_str(), "source.yml inline templates");
        }
        merged_templates.extend(templates.iter().cloned());
    }
    if let Some(templates) = mappings_inline_templates {
        for template in templates {
            track_template_name(&mut template_name_usage, template.name.as_str(), "mapping.yml inline templates");
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
        return Err(TuliproxError::Config(format!(
            "Duplicate template names found across merged template sources: {}",
            duplicate_templates.join("; ")
        )));
    }

    let files_used = if loaded_template_files.is_empty() { None } else { Some(loaded_template_files) };

    if merged_templates.is_empty() {
        return Ok(PreparedTemplateBundle { definition: None, prepared: None, files_used });
    }

    let mut prepared_templates = merged_templates.clone();
    prepare_templates(&mut prepared_templates)?;

    Ok(PreparedTemplateBundle {
        definition: Some(TemplateDefinitionDto { templates: merged_templates }),
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
    let (effective_template_path, effective_mapping_path) =
        resolve_template_and_mapping_paths(paths, config.template_path.as_deref(), config.mapping_path.as_deref());

    let mut sources = parse_sources_file_from_path(&PathBuf::from(sources_file), resolve_env).await?;
    let mut mappings = read_mappings_file_unprepared(&effective_mapping_path, resolve_env)?.map(|(_, mapping)| mapping);

    let template_bundle = read_templates(
        Some(&effective_template_path),
        resolve_env,
        sources.templates.as_deref(),
        mappings.as_ref().and_then(|mapping| mapping.mappings.templates.as_deref()),
    )?;
    let templates = template_bundle.definition;
    let prepared_templates = template_bundle.prepared;

    if resolve_env {
        sources.prepare(include_computed, config.get_hdhr_device_overview().as_ref(), prepared_templates.as_deref())?;
        if let Some(mapping) = mappings.as_mut() {
            mapping.mappings.prepare(prepared_templates.as_deref())?;
        }
    }

    // Keep templates centralized in AppConfigDto.templates.
    sources.templates = None;
    if let Some(mapping) = mappings.as_mut() {
        mapping.mappings.templates = None;
    }

    let api_proxy = match read_api_proxy_file(api_proxy_file, resolve_env) {
        Ok(api_proxy) => api_proxy,
        Err(err) => {
            // Surface the fault instead of silently returning a config without api_proxy
            log::warn!("Failed to read api-proxy config '{api_proxy_file}': {err}");
            None
        }
    };

    Ok(AppConfigDto { config, sources, mappings, templates, api_proxy })
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
            let mappings: tuliprox_core::model::Mappings = tuliprox_core::model::Mappings::from(&*mapping);
            app_config.set_mappings(mappings_file, &mappings);
            paths.mapping_files_used =
                mapping_paths.map(|items| items.iter().map(|p| p.display().to_string()).collect::<Vec<String>>());
        }
        Err(err) => {
            error!("Failed to prepare mapping '{mappings_file}': {err}. Skipping mapping registration.");
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
            std::iter::once(item.id).chain(item.aliases.as_ref().into_iter().flatten().map(|alias| alias.id))
        })
        .max()
        .unwrap_or(0);

    current_index = std::cmp::max(current_index, max_id_in_source);

    for input in &mut sources.inputs {
        // Normalize batch type from URL before deciding whether aliases must be loaded from CSV.
        input.prepare_type()?;
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

pub async fn get_batch_aliases(
    input_type: InputType,
    url: &str,
) -> Result<Option<(PathBuf, Vec<ConfigInputAliasDto>)>, TuliproxError> {
    if matches!(input_type, InputType::M3uBatch | InputType::XtreamBatch | InputType::StalkerBatch) {
        if url.starts_with(PROVIDER_SCHEME_PREFIX) {
            return Err(TuliproxError::Config(format!(
                "Batch input type '{input_type}' does not support provider:// URLs. \
Use a batch:// URL or a local CSV path (absolute/relative)."
            )));
        }

        return match csv_read_inputs(input_type, url).await {
            Ok((file_path, batch_aliases)) => Ok(Some((file_path, batch_aliases))),
            Err(err) => Err(TuliproxError::Config(format!("{err}"))),
        };
    }
    Ok(None)
}

pub async fn prepare_users(app_config_dto: &mut AppConfigDto, app_config: &AppConfig) -> Result<(), TuliproxError> {
    let use_user_db = app_config_dto.api_proxy.as_ref().is_some_and(|p| p.use_user_db);

    if use_user_db {
        let user_db_path = get_api_user_db_path(app_config);
        if file_exists_async(&user_db_path).await {
            match load_api_user(app_config).await {
                Ok(stored_users) => {
                    if let Some(api_proxy) = app_config_dto.api_proxy.as_mut() {
                        api_proxy.user.extend(stored_users.iter().map(TargetUserDto::from));
                    }
                }
                Err(err) => {
                    warn!("Failed to load users from DB at {}: {err}", user_db_path.display());
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

    let configured_mapping_path = paths
        .mapping_file_path
        .as_deref()
        .or(config_dto.mapping_path.as_deref())
        .map(|path| if resolve_env { resolve_env_var(path) } else { path.to_string() });
    let configured_template_path = paths
        .template_file_path
        .as_deref()
        .or(config_dto.template_path.as_deref())
        .map(|path| if resolve_env { resolve_env_var(path) } else { path.to_string() });

    paths.mapping_file_path = Some(utils::resolve_mapping_file_path(config_path, configured_mapping_path.as_deref()));
    paths.template_file_path =
        Some(utils::resolve_template_file_path(config_path, configured_template_path.as_deref()));

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
        paths.template_file_path.as_deref(),
        resolve_env,
        sources_dto.templates.as_deref(),
        mappings_dto.as_ref().and_then(|mapping| mapping.mappings.templates.as_deref()),
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
    paths.storage_path.clone_from(&config.storage_dir);
    config.update_runtime();

    let mut app_config = AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapAny::default()),
        api_proxy: Arc::new(ArcSwapAny::default()),
        paths: Arc::new(ArcSwap::from_pointee(paths.clone())),
        file_locks: Arc::new(FileLockManager::default()),
        custom_stream_response: Arc::new(ArcSwapAny::default()),
        access_token_secret: generate_default_access_secret()?,
        encrypt_secret: generate_default_encrypt_secret()?,
        media_tools: Arc::new(MediaToolCapabilities::new()),
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

    let current_paths = app_config.paths.load_full();
    let mut merged_paths = current_paths.as_ref().clone();
    merged_paths.mapping_files_used = paths.mapping_files_used.clone();
    merged_paths.template_files_used = template_files_used;
    app_config.paths.store(Arc::new(merged_paths));

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
                    // A recoverable error keeps the last good config alive during hot reload
                    if let Err(err) = api_proxy_dto.prepare() {
                        return Err(TuliproxError::Config(format!("can't read api-proxy-config file: {err}")));
                    }
                }
                Ok(Some(api_proxy_dto))
            }
            Err(err) => Err(TuliproxError::Config(format!("can't read api-proxy-config file: {err}"))),
        }
    })
}

pub async fn read_api_proxy(config: &AppConfig, resolve_env: bool) -> Option<ApiProxyConfig> {
    let paths = config.paths.load();
    match read_api_proxy_file(paths.api_proxy_file_path.as_str(), resolve_env) {
        Ok(Some(api_proxy_dto)) => {
            let mut errors = vec![];
            let mut api_proxy: ApiProxyConfig = ApiProxyConfig::from(&api_proxy_dto);
            apply_authoritative_plans(config, &mut api_proxy, resolve_env).await;
            migrate_api_user(&mut api_proxy, config, &mut errors).await;
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
    let filename = path.file_name().map_or(default_name.to_string(), |f| f.to_string_lossy().to_string());

    let mut serialized = String::new();
    let options = serde_saphyr::ser_options! {prefer_block_scalars: false};
    serde_saphyr::to_fmt_writer_with_options(&mut serialized, &config, options)
        .map_err(|err| TuliproxError::Config(format!("Could not serialize config: {err}")))?;

    if file_exists_async(&path).await {
        if let Ok(existing) = fs::read_to_string(&path).await {
            if existing == serialized {
                // info!("File {} unchanged, skipping write", path.display());
                return Ok(());
            }
        }
    }

    if file_exists_async(&path).await {
        fs::create_dir_all(backup_dir)
            .await
            .map_err(|err| TuliproxError::Config(format!("Could not create backup directory {backup_dir}: {err}")))?;
        let backup_path =
            PathBuf::from(backup_dir).join(format!("{filename}_{}", Local::now().format("%Y%m%d_%H%M%S%9f")));

        fs::copy(&path, &backup_path)
            .await
            .map_err(|err| TuliproxError::Config(format!("Could not backup file {}: {err}", backup_path.display())))?;
        info!("Saving file to {}", path.to_str().unwrap_or("?"));
    }

    let parent_dir = path.parent().ok_or_else(|| {
        TuliproxError::Config(format!(
            "Could not write file {}: missing parent directory",
            path.to_str().unwrap_or("?")
        ))
    })?;

    let dest_file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or(default_name);

    let mut tmp_path = parent_dir.to_path_buf();
    tmp_path.push(format!(
        ".{dest_file_name}.tmp-{}-{}",
        std::process::id(),
        Local::now().timestamp_nanos_opt().unwrap_or_default()
    ));

    fs::write(&tmp_path, serialized).await.map_err(|err| {
        TuliproxError::Config(format!("Could not write temp file {}: {err}", tmp_path.to_str().unwrap_or("?")))
    })?;

    match fs::rename(&tmp_path, &path).await {
        Ok(()) => Ok(()),
        Err(err) => {
            // Windows doesn't allow overwriting an existing file via rename.
            #[cfg(windows)]
            {
                if replace_file_windows(&tmp_path, &path).is_ok() {
                    return Ok(());
                }
            }

            // Best-effort cleanup; if the temp file can't be removed, ignore it.
            let _ = fs::remove_file(&tmp_path).await;
            Err(TuliproxError::Config(format!(
                "Could not replace file {} with {}: {err}",
                path.to_str().unwrap_or("?"),
                tmp_path.to_str().unwrap_or("?")
            )))
        }
    }
}

#[cfg(windows)]
fn replace_file_windows(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::{io, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

    let mut source_wide: Vec<u16> = source.as_os_str().encode_wide().collect();
    source_wide.push(0);
    let mut target_wide: Vec<u16> = target.as_os_str().encode_wide().collect();
    target_wide.push(0);

    // SAFETY: both paths are NUL-terminated UTF-16 buffers that remain alive for
    // the call. `MOVEFILE_REPLACE_EXISTING` leaves the target in place on error.
    let ok = unsafe {
        MoveFileExW(source_wide.as_ptr(), target_wide.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub async fn save_api_proxy(
    file_path: &str,
    backup_dir: &str,
    config: &ApiProxyConfigDto,
) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, "api-proxy.yml").await
}

/// Path of the standalone plans file, sibling to api-proxy.yml.
pub fn plans_file_path(api_proxy_file_path: &str) -> PathBuf {
    let parent = Path::new(api_proxy_file_path).parent();
    let candidates = ["plans.yml", "plans.yaml", "plan.yml", "plan.yaml"];
    if let Some(dir) = parent {
        for candidate in candidates {
            let p = dir.join(candidate);
            if p.exists() {
                return p;
            }
        }
        dir.join("plans.yml")
    } else {
        for candidate in candidates {
            let p = PathBuf::from(candidate);
            if p.exists() {
                return p;
            }
        }
        PathBuf::from("plans.yml")
    }
}

pub fn read_plans_file(plans_file: &str, resolve_env: bool) -> Result<Option<PlansConfigDto>, TuliproxError> {
    open_file(&PathBuf::from(plans_file)).map_or(Ok(None), |file| {
        let parsed: Result<PlansConfigDto, _> = serde_saphyr::from_reader(config_file_reader(file, resolve_env));
        match parsed {
            Ok(mut dto) => {
                if resolve_env {
                    if let Err(err) = dto.prepare() {
                        return Err(TuliproxError::Config(format!("can't read plans file: {err}")));
                    }
                }
                Ok(Some(dto))
            }
            Err(err) => Err(TuliproxError::Config(format!("can't read plans file: {err}"))),
        }
    })
}

pub async fn save_plans(file_path: &str, backup_dir: &str, config: &PlansConfigDto) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, "plans.yml").await
}

/// Load plans from the standalone plans.yml onto the runtime api-proxy config
/// and re-resolve users. Migrates legacy plans embedded in api-proxy.yml to
/// plans.yml the first time it runs.
async fn apply_authoritative_plans(config: &AppConfig, api_proxy: &mut ApiProxyConfig, resolve_env: bool) {
    let plans_path = {
        let paths = config.paths.load();
        plans_file_path(paths.api_proxy_file_path.as_str())
    };
    let plans_path_str = plans_path.to_string_lossy().to_string();
    match read_plans_file(&plans_path_str, resolve_env) {
        Ok(Some(plans_dto)) => {
            let plans = plans_dto.plans.iter().map(|plan| Arc::new(UserPlan::from(plan))).collect();
            api_proxy.set_plans(plans);
        }
        Ok(None) => {
            if let Some(legacy_plans) = read_legacy_api_proxy_plans(config, resolve_env) {
                let migrated = PlansConfigDto { plans: legacy_plans.clone() };
                let backup_dir = {
                    let cfg = config.config.load();
                    cfg.get_backup_dir().to_string()
                };
                match save_plans(&plans_path_str, &backup_dir, &migrated).await {
                    Ok(()) => {
                        info!("Migrated {} user plan(s) from api-proxy.yml to {plans_path_str}", migrated.plans.len());
                    }
                    Err(err) => error!("Failed to migrate plans to {plans_path_str}: {err}"),
                }
                let plans = legacy_plans.iter().map(|plan| Arc::new(UserPlan::from(plan))).collect();
                api_proxy.set_plans(plans);
            }
        }
        Err(err) => error!("{err}"),
    }
}

/// Read legacy plans still embedded in api-proxy.yml (pre plans.yml split).
fn read_legacy_api_proxy_plans(config: &AppConfig, resolve_env: bool) -> Option<Vec<UserPlanDto>> {
    #[derive(serde::Deserialize)]
    struct LegacyApiProxyPlans {
        #[serde(default)]
        plans: Vec<UserPlanDto>,
    }
    let api_proxy_file = {
        let paths = config.paths.load();
        paths.api_proxy_file_path.clone()
    };
    let file = open_file(&PathBuf::from(api_proxy_file.as_str())).ok()?;
    let parsed: Result<LegacyApiProxyPlans, _> = serde_saphyr::from_reader(config_file_reader(file, resolve_env));
    match parsed {
        Ok(dto) if !dto.plans.is_empty() => Some(dto.plans),
        _ => None,
    }
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

pub async fn save_templates_config(
    file_path: &str,
    backup_dir: &str,
    config: &TemplateDefinitionDto,
) -> Result<(), TuliproxError> {
    write_config_file(file_path, backup_dir, config, TEMPLATE_FILE).await
}

async fn build_templates_to_persist(
    app_config: &Arc<AppConfig>,
    dto: &SourcesConfigDto,
) -> Result<Option<TemplateDefinitionDto>, TuliproxError> {
    let mut new_dto = dto.clone();
    let config = app_config.config.load();
    let paths = app_config.paths.load();

    let existing_source_inline_templates =
        match parse_sources_file_from_path(Path::new(paths.sources_file_path.as_str()), true).await {
            Ok(existing_sources) => existing_sources.templates,
            Err(err) => {
                warn!("Failed to read existing source.yml for template migration '{}': {err}", paths.sources_file_path);
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
    let template_bundle =
        read_templates(template_file_path, true, source_inline_templates, mapping_inline_templates.as_deref())?;
    let prepared_templates = template_bundle.prepared;

    new_dto.prepare(true, config.get_hdhr_device_overview().as_ref(), prepared_templates.as_deref())?;

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
    app_config: &Arc<AppConfig>,
    dto: &SourcesConfigDto,
) -> Result<Option<TemplateDefinitionDto>, TuliproxError> {
    build_templates_to_persist(app_config, dto).await
}

pub async fn persist_templates_config(
    app_config: &Arc<AppConfig>,
    template_definition: &TemplateDefinitionDto,
) -> Result<(), TuliproxError> {
    let config = app_config.config.load();
    let paths = app_config.paths.load();
    let template_file = utils::resolve_template_persist_file_path(
        paths.template_file_path.as_deref().or(config.template_path.as_deref()),
        &paths.config_path,
    );
    save_templates_config(&template_file, config.get_backup_dir().as_ref(), template_definition).await
}

pub async fn persist_source_config(
    app_config: &Arc<AppConfig>,
    source_file_path: Option<&Path>,
    doc: SourcesConfigDto,
) -> Result<SourcesConfigDto, TuliproxError> {
    let source_file = {
        source_file_path.and_then(|p| p.to_str()).map_or_else(
            || {
                let paths = app_config.paths.load();
                paths.sources_file_path.clone()
            },
            ToString::to_string,
        )
    };
    let backup_dir = {
        let config = app_config.config.load();
        config.get_backup_dir().to_string()
    };

    let source_config = sanitize_sources_for_persist(doc.clone()).await;

    save_sources_config(&source_file, &backup_dir, &source_config).await?;
    Ok(doc)
}

pub async fn sanitize_sources_for_persist(mut source_config: SourcesConfigDto) -> SourcesConfigDto {
    source_config.templates = None;
    for input in &mut source_config.inputs {
        if input.panel_api.as_ref().is_some_and(|panel| panel.alias_pool.is_some()) {
            if let Some(aliases) = input.aliases.as_mut() {
                aliases.sort_by(|a, b| {
                    let a_ts = a.exp_date.unwrap_or(i64::MIN);
                    let b_ts = b.exp_date.unwrap_or(i64::MIN);
                    b_ts.cmp(&a_ts).then_with(|| a.name.cmp(&b.name))
                });
            }
        }
        if matches!(input.input_type, InputType::XtreamBatch | InputType::M3uBatch | InputType::StalkerBatch)
            && is_csv_file(input.url.as_str())
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
    app_config: &Arc<AppConfig>,
    dto: SourcesConfigDto,
) -> Result<SourcesConfigDto, TuliproxError> {
    let templates_to_persist = validate_source_config_for_persist(app_config, &dto).await?;

    if let Some(template_definition) = templates_to_persist.as_ref() {
        persist_templates_config(app_config, template_definition).await?;
    }

    persist_source_config(app_config, None, dto).await
}

pub async fn persist_messaging_templates(
    app_config: &Arc<AppConfig>,
    cfg: &mut ConfigDto,
) -> Result<(), TuliproxError> {
    let templates_dir = {
        let paths = app_config.paths.load();
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

async fn persist_single_template(
    prefix: &str,
    kind: Option<&MsgKind>,
    template: &str,
    templates_dir: &Path,
) -> Result<String, TuliproxError> {
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
        tokio::fs::create_dir_all(templates_dir).await.map_err(|e| {
            TuliproxError::Config(format!(
                "Messaging templates dir: failed to create dir: {} {e}",
                templates_dir.display()
            ))
        })?;
    }

    let filename =
        if let Some(k) = kind { k.template_filename(prefix) } else { concat_string!(prefix, "_default.templ") };

    let file_path = templates_dir.join(filename);
    fs::write(&file_path, template)
        .await
        .map_err(|e| TuliproxError::Config(format!("Failed to write template file: {e}")))?;

    Url::from_file_path(&file_path).map(|u| u.to_string()).map_err(|()| {
        TuliproxError::Config(format!("Failed to convert persisted path to file URL: {}", file_path.display()))
    })
}

// ---- API user migration (config file <-> user database) ----
//
// Was a method on `ApiProxyConfig`, then briefly a repository module. It writes
// the api-proxy config file, so it belongs with the rest of the configuration
// bootstrap; living in `repository` made that layer call back into this one.
fn serialize_api_proxy_config(config: &ApiProxyConfigDto) -> Result<String, String> {
    let mut serialized = String::new();
    let options = serde_saphyr::ser_options! {prefer_block_scalars: false};
    serde_saphyr::to_fmt_writer_with_options(&mut serialized, config, options)
        .map_err(|err| format!("Could not serialize api proxy config: {err}"))?;
    Ok(serialized)
}

async fn api_proxy_file_would_change(api_proxy_file: &str, config: &ApiProxyConfigDto) -> Result<bool, String> {
    let serialized = serialize_api_proxy_config(config)?;
    match tokio::fs::read_to_string(api_proxy_file).await {
        Ok(existing) => Ok(existing != serialized),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(true),
        Err(err) => Err(format!("Could not read api proxy file {api_proxy_file}: {err}")),
    }
}

async fn backfill_output_clusters_to_file(api_proxy: &ApiProxyConfig, cfg: &AppConfig, errors: &mut Vec<String>) {
    if api_proxy.user.is_empty() {
        return;
    }
    let paths = ArcSwap::load(&cfg.paths);
    let api_proxy_file = paths.api_proxy_file_path.as_str();
    let dto = ApiProxyConfigDto::from(api_proxy);
    match api_proxy_file_would_change(api_proxy_file, &dto).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            errors.push(err);
            return;
        }
    }
    let config = ArcSwap::load(&cfg.config);
    let backup_dir = config.get_backup_dir();
    if let Err(err) = save_api_proxy(api_proxy_file, backup_dir.as_ref(), &dto).await {
        errors.push(format!("Error saving api proxy file: {err}"));
    }
}

// we have the option to store user in the config file or in the user_db
// When we switch from one to other we need to migrate the existing data.
/// # Panics
pub async fn migrate_api_user(api_proxy: &mut ApiProxyConfig, cfg: &AppConfig, errors: &mut Vec<String>) {
    let paths = ArcSwap::load(&cfg.paths);
    let api_proxy_file = paths.api_proxy_file_path.as_str();
    if api_proxy.use_user_db {
        // we have user defined in config file.
        // we migrate them to the db and delete them from the config file
        if !&api_proxy.user.is_empty() {
            if let Err(err) = merge_api_user(cfg, &api_proxy.user).await {
                errors.push(err.to_string());
            } else {
                let config = ArcSwap::load(&cfg.config);
                let backup_dir = config.get_backup_dir();
                api_proxy.user = vec![];
                if let Err(err) =
                    save_api_proxy(api_proxy_file, backup_dir.as_ref(), &ApiProxyConfigDto::from(&*api_proxy)).await
                {
                    errors.push(format!("Error saving api proxy file: {err}"));
                }
            }
        }
        match load_api_user(cfg).await {
            Ok(users) => {
                let mut users = users;
                api_proxy.resolve_target_users(&mut users);
                api_proxy.user = users;
            }
            Err(err) => {
                println!("{err}");
                errors.push(err.to_string());
            }
        }
    } else {
        backfill_output_clusters_to_file(api_proxy, cfg, errors).await;
        let user_db_path = get_api_user_db_path(cfg);
        if file_exists_async(&user_db_path).await {
            // we can't have user defined in db file.
            // we need to load them and save them into the config file
            if let Ok(stored_users) = load_api_user(cfg).await {
                let mut stored_users = stored_users;
                api_proxy.resolve_target_users(&mut stored_users);
                for stored_user in stored_users {
                    if let Some(target_user) = api_proxy.user.iter_mut().find(|t| t.target == stored_user.target) {
                        for stored_credential in &stored_user.credentials {
                            if !target_user.credentials.iter().any(|c| c.username == stored_credential.username) {
                                target_user.credentials.push(stored_credential.clone());
                            }
                        }
                    } else {
                        api_proxy.user.push(stored_user);
                    }
                }
            }

            let config = ArcSwap::load(&cfg.config);
            let backup_dir = config.get_backup_dir();
            let dto = ApiProxyConfigDto::from(&*api_proxy);
            match api_proxy_file_would_change(api_proxy_file, &dto).await {
                Ok(true) => {
                    if let Err(err) = save_api_proxy(api_proxy_file, backup_dir.as_ref(), &dto).await {
                        errors.push(format!("Error saving api proxy file: {err}"));
                    } else {
                        backup_api_user_db_file(cfg, &user_db_path).await;
                        let _ = tokio::fs::remove_file(&user_db_path).await;
                    }
                }
                Ok(false) => {
                    backup_api_user_db_file(cfg, &user_db_path).await;
                    let _ = tokio::fs::remove_file(&user_db_path).await;
                }
                Err(err) => errors.push(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{get_batch_aliases, prepare_sources_batch, sanitize_sources_for_persist, write_config_file};
    use shared::{
        model::{ConfigInputAliasDto, ConfigInputDto, InputType, SourcesConfigDto},
        utils::Internable,
    };
    use tempfile::tempdir;
    use tuliprox_core::utils::resolve_env_var;

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
        assert!(err.to_string().contains("does not support provider:// URLs"));
    }

    #[tokio::test]
    async fn prepare_sources_batch_skips_csv_for_non_batch_url_even_with_batch_type() {
        let mut sources = SourcesConfigDto {
            inputs: vec![ConfigInputDto {
                name: "input_http_aliases".intern(),
                input_type: InputType::XtreamBatch,
                url: "http://localhost:3001".to_string(),
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                ..ConfigInputDto::default()
            }],
            ..SourcesConfigDto::default()
        };

        prepare_sources_batch(&mut sources, false)
            .await
            .expect("non-batch URL must not be treated as CSV batch source");

        assert_eq!(sources.inputs[0].input_type, InputType::Xtream);
    }

    #[tokio::test]
    async fn stalker_batch_loads_and_persists_csv_aliases() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("stalker.csv");
        std::fs::write(&path, "#name;username;password;url;max_connections\nalias;user;pass;http://portal.example;1\n")
            .expect("write csv");

        let (_, aliases) = get_batch_aliases(InputType::StalkerBatch, path.to_string_lossy().as_ref())
            .await
            .expect("read batch")
            .expect("stalker batch aliases");
        assert_eq!(aliases.len(), 1);

        let sources = SourcesConfigDto {
            inputs: vec![ConfigInputDto {
                input_type: InputType::StalkerBatch,
                url: path.to_string_lossy().into_owned(),
                aliases: Some(vec![ConfigInputAliasDto { name: "updated".intern(), ..Default::default() }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let sanitized = sanitize_sources_for_persist(sources).await;
        assert!(sanitized.inputs[0].aliases.is_none());
        assert!(std::fs::read_to_string(path).expect("read persisted csv").contains("updated"));
    }

    #[tokio::test]
    async fn config_write_stops_when_backup_fails() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let config_path = dir.path().join("source.yml");
        let invalid_backup_dir = dir.path().join("not-a-directory");
        tokio::fs::write(&config_path, "old").await?;
        tokio::fs::write(&invalid_backup_dir, "file").await?;

        let result = write_config_file(
            config_path.to_string_lossy().as_ref(),
            invalid_backup_dir.to_string_lossy().as_ref(),
            &"new",
            "source.yml",
        )
        .await;

        assert!(result.is_err());
        assert_eq!(tokio::fs::read_to_string(config_path).await?, "old");
        Ok(())
    }
}
