use super::playlist_mem_cache::PlaylistStorageState;
use crate::{
    bplustree::{BPlusTree, BPlusTreeQuery},
    error_macros::{await_playlist_write, cant_read_result, cant_write_result},
    m3u_playlist_iterator::M3uPlaylistM3uTextIterator,
    playlist_backend::{ensure_storage_path, iter_raw_playlist, M3u, PlaylistBackend},
    playlist_repository::get_input_m3u_playlist_file_path,
    storage::{
        build_input_storage_path, existing_file_identity, file_digest, get_input_storage_path, get_target_storage_path,
        FileIdentity,
    },
    storage_const,
    xtream_repository::{
        target_publish::{publish_target_file_replacements, TargetEmptyReplacementMode, TargetFileReplacement},
        CategoryKey,
    },
    LockedReceiverStream,
};
use indexmap::IndexMap;
use log::warn;
use serde::{Deserialize, Serialize};
use shared::{
    concat_string,
    error::{str_to_io_error, string_to_io_error, TuliproxError},
    model::{
        ConfigTargetOptions, LiveStreamProperties, M3uPlaylistItem, PlaylistGroup, PlaylistItem, StreamProperties,
        XtreamCluster,
    },
    utils::{m3u_flussonic_live_file_index_key, m3u_stream_url_identity, PROVIDER_SCHEME_PREFIX},
};
use std::{
    collections::HashMap,
    io::{self, Error},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{fs, io::AsyncWriteExt, task};
use tuliprox_core::{
    model::{AppConfig, Config, ConfigInput, ConfigTarget, M3uTargetOutput, ProxyUserCredentials, SourcesConfig},
    utils::{async_file_writer, file_exists_async},
};

const M3U_TEXT_HEADER: &[u8] = b"#EXTM3U\n";

pub fn m3u_get_file_path_for_db(target_path: &Path) -> PathBuf {
    target_path.join(storage_const::PATH_M3U).join(concat_string!(
        storage_const::FILE_M3U,
        ".",
        storage_const::FILE_SUFFIX_DB
    ))
}

pub fn m3u_get_epg_file_path_for_target(target_path: &Path) -> PathBuf {
    target_path.join(storage_const::PATH_M3U).join(concat_string!("epg.", storage_const::FILE_SUFFIX_DB))
}

#[inline]
pub fn m3u_get_storage_path(cfg: &Config, target_name: &str) -> Option<PathBuf> { M3u::storage_path(cfg, target_name) }

#[inline]
pub async fn ensure_m3u_storage_path(cfg: &Config, target_name: &str) -> Result<PathBuf, TuliproxError> {
    ensure_storage_path::<M3u>(cfg, target_name).await
}

fn provider_m3u_filename(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
    let extension = path.extension().map(|ext| ext.to_string_lossy());
    let new_name = match extension {
        Some(ext) if !ext.is_empty() => concat_string!(&stem, "_provider.", &ext),
        _ => concat_string!(&stem, "_provider"),
    };
    path.with_file_name(new_name)
}

fn m3u_text_filename(config: &Config, output: &M3uTargetOutput) -> Option<PathBuf> {
    tuliprox_core::utils::get_file_path(&config.storage_dir, output.filename.as_deref().map(PathBuf::from))
}

struct M3uExportPath<'a> {
    path: &'a Path,
    file: Option<FileIdentity>,
    parent: Option<FileIdentity>,
}

impl<'a> M3uExportPath<'a> {
    fn new(path: &'a Path) -> io::Result<Self> {
        let parent = path.parent().and_then(|parent| existing_file_identity(parent).ok().flatten());
        Ok(Self { path, file: existing_file_identity(path)?, parent })
    }

    fn aliases(&self, other: &Path) -> bool {
        self.path == other
            || (self.file.is_some() && existing_file_identity(other).is_ok_and(|file| file == self.file))
            || (self.path.file_name() == other.file_name()
                && self.parent.is_some()
                && other.parent().and_then(|parent| existing_file_identity(parent).ok().flatten()) == self.parent)
    }
}

fn conflicting_export_target(
    candidate: &M3uExportPath<'_>,
    sources: &SourcesConfig,
    config: &Config,
    target_name: &str,
    is_companion: bool,
) -> io::Result<Option<String>> {
    for source in &sources.sources {
        for target in &source.targets {
            let same_target = target.name == target_name;
            let storage = get_target_storage_path(config, &target.name);
            if let Some(filename) = target.get_m3u_output().and_then(|output| m3u_text_filename(config, output)) {
                if (!same_target || is_companion) && candidate.aliases(&filename) {
                    return Ok(Some(target.name.clone()));
                }
                if !same_target {
                    let companion = provider_m3u_filename(&filename);
                    if candidate.aliases(&companion) {
                        let provider_active = sources.inputs.iter().any(|input| {
                            input.url.starts_with(PROVIDER_SCHEME_PREFIX) && source.inputs.contains(&input.name)
                        });
                        if provider_active {
                            return Ok(Some(target.name.clone()));
                        }
                        let legacy_owner = if let Some(storage) = &storage {
                            matches!(
                                provider_export_state(
                                    &companion,
                                    &m3u_get_file_path_for_db(storage),
                                    target.options.as_ref(),
                                    ProviderExportOwnership::Verify,
                                )?,
                                ProviderExportState::Owned(_)
                            )
                        } else {
                            false
                        };
                        if legacy_owner {
                            return Ok(Some(target.name.clone()));
                        }
                    }
                }
            }
            if let Some(storage) = &storage {
                if target.plugin_outputs().any(|plugin| candidate.aliases(&storage.join(plugin.file_name()))) {
                    return Ok(Some(target.name.clone()));
                }
            }
        }
    }
    Ok(None)
}

async fn checked_m3u_text_paths(
    app: &AppConfig,
    config: Arc<Config>,
    target: &ConfigTarget,
    filename: PathBuf,
    provider_active: bool,
    check_main: bool,
) -> Result<(PathBuf, Option<PathBuf>), TuliproxError> {
    let target_name = target.name.clone();
    let sources = app.sources.load_full();
    task::spawn_blocking(move || {
        if check_main {
            if let Some(owner) = M3uExportPath::new(&filename)
                .and_then(|candidate| conflicting_export_target(&candidate, &sources, &config, &target_name, false))
                .map_err(|err| cant_read_result!(RepositoryM3u, "text export", &filename, err))?
            {
                return Err(TuliproxError::RepositoryM3u(format!(
                    "M3U export path {} is also used by target '{owner}'",
                    filename.display()
                )));
            }
        }
        let path = provider_m3u_filename(&filename);
        let read_error = |err| cant_read_result!(RepositoryM3u, "provider export", &path, err);
        let candidate = M3uExportPath::new(&path).map_err(read_error)?;
        if candidate.file.is_none() && !provider_active {
            return Ok((filename, None));
        }
        match conflicting_export_target(&candidate, &sources, &config, &target_name, true).map_err(read_error)? {
            Some(owner) if provider_active => Err(TuliproxError::RepositoryM3u(format!(
                "Provider M3U export path {} is also used by target '{owner}'",
                path.display()
            ))),
            Some(_) => Ok((filename, None)),
            None => Ok((filename, Some(path))),
        }
    })
    .await
    .map_err(|err| TuliproxError::RepositoryM3u(format!("Failed to check M3U export paths: {err}")))?
}

#[derive(Serialize, Deserialize)]
struct ProviderExportOwner {
    path: PathBuf,
    digest: String,
}

enum ProviderExportState {
    Missing,
    Owned(blake3::Hash),
    Unverified,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProviderExportOwnership {
    Verify,
    AdoptLegacy,
}

fn provider_export_owner_path(database: &Path) -> PathBuf { database.with_file_name("provider-export.json") }

fn provider_export_state(
    path: &Path,
    database: &Path,
    target_options: Option<&ConfigTargetOptions>,
    ownership: ProviderExportOwnership,
) -> io::Result<ProviderExportState> {
    use ProviderExportState::{Missing, Owned, Unverified};
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(Unverified),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Missing),
        Err(error) => return Err(error),
    }
    let actual = file_digest(path)?;
    match std::fs::read(provider_export_owner_path(database)) {
        Ok(bytes) => {
            let owner: ProviderExportOwner = serde_json::from_slice(&bytes)?;
            return Ok(if owner.path == path && owner.digest == actual.to_hex().as_str() {
                Owned(actual)
            } else {
                Unverified
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Ordinary updates historically overwrote this target's companion path.
    if ownership == ProviderExportOwnership::AdoptLegacy {
        return Ok(Owned(actual));
    }

    // Legacy exports predate the ownership record. Only an exact reproduction of the
    // target database with unresolved provider URLs establishes ownership here.
    if !database.try_exists()? {
        return Ok(Unverified);
    }
    let index = crate::get_file_path_for_db_index(database);
    let entries =
        crate::open_playlist_reader::<u32, M3uPlaylistItem, u32>(database, &index, None).map_err(io::Error::other)?;
    let mut expected = blake3::Hasher::new();
    expected.update(M3U_TEXT_HEADER);
    let mut has_provider_url = false;
    for entry in entries {
        let (_, item) = entry?;
        has_provider_url |= m3u_effective_url(&item).starts_with(PROVIDER_SCHEME_PREFIX);
        expected.update(item.to_m3u(target_options, false).as_bytes()).update(b"\n");
    }
    Ok(if has_provider_url && expected.finalize() == actual { Owned(actual) } else { Unverified })
}

async fn write_m3u_text_file<F>(
    m3u_filename: &Path,
    m3u_playlist: &[M3uPlaylistItem],
    mut build_line: F,
) -> Result<(), TuliproxError>
where
    F: FnMut(&M3uPlaylistItem) -> Result<String, TuliproxError>,
{
    let file = await_playlist_write!(
        RepositoryM3u,
        fs::File::create(m3u_filename),
        "Can't write m3u plain playlist {} - {}",
        m3u_filename.display()
    );
    // Larger buffer for sequential writes to reduce syscalls
    let mut writer = async_file_writer(file);
    await_playlist_write!(
        RepositoryM3u,
        writer.write_all(M3U_TEXT_HEADER),
        "Failed to write header to {} - {}",
        m3u_filename.display()
    );

    for m3u in m3u_playlist {
        let line = build_line(m3u)?;
        let bytes = line.as_bytes();
        await_playlist_write!(
            RepositoryM3u,
            writer.write_all(bytes),
            "Failed to write entry to {} - {}",
            m3u_filename.display()
        );
        await_playlist_write!(
            RepositoryM3u,
            writer.write_all(b"\n"),
            "Failed to write newline to {} - {}",
            m3u_filename.display()
        );
    }

    await_playlist_write!(RepositoryM3u, writer.flush(), "Failed to flush {} - {}", m3u_filename.display());

    Ok(())
}

fn temp_m3u_filename(path: &Path) -> PathBuf {
    let file_name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    path.with_file_name(format!("{file_name}.tmp"))
}

async fn write_m3u_text_file_atomic<F>(
    m3u_filename: &Path,
    m3u_playlist: &[M3uPlaylistItem],
    build_line: F,
) -> Result<(), TuliproxError>
where
    F: FnMut(&M3uPlaylistItem) -> Result<String, TuliproxError>,
{
    let tmp_path = temp_m3u_filename(m3u_filename);
    write_m3u_text_file(&tmp_path, m3u_playlist, build_line).await?;
    await_playlist_write!(
        RepositoryM3u,
        fs::rename(&tmp_path, m3u_filename),
        "Failed to replace {} - {}",
        m3u_filename.display()
    );
    Ok(())
}

fn replace_m3u_url_line(line: &mut String, url: &str) {
    let trimmed = line.trim_end_matches(|c: char| c == '\n' || c.is_whitespace());
    let mut rebuilt = if let Some((meta, _old_url)) = trimmed.rsplit_once('\n') {
        // Preserve metadata line and a single newline separator.
        let mut out = String::with_capacity(meta.len() + 1 + url.len());
        out.push_str(meta);
        out.push('\n');
        out
    } else {
        let mut out = String::with_capacity(trimmed.len() + 1 + url.len());
        out.push_str(trimmed);
        out.push('\n');
        out
    };

    rebuilt.push_str(url);
    *line = rebuilt;
}

fn m3u_provider_inputs(app_config: &AppConfig, target: &ConfigTarget) -> HashMap<Arc<str>, Arc<ConfigInput>> {
    let sources = app_config.sources.load();
    if let Some(source) = sources.sources.iter().find(|source| source.targets.iter().any(|t| t.name == target.name)) {
        sources
            .inputs
            .iter()
            .filter(|input| input.url.starts_with(PROVIDER_SCHEME_PREFIX))
            .filter(|input| source.inputs.contains(&input.name))
            .map(|input| (Arc::clone(&input.name), Arc::clone(input)))
            .collect()
    } else {
        HashMap::new()
    }
}

fn m3u_effective_url(item: &M3uPlaylistItem) -> &str {
    if item.t_stream_url.is_empty() {
        &item.url
    } else {
        &item.t_stream_url
    }
}

fn resolved_m3u_line(
    m3u: &M3uPlaylistItem,
    target_options: Option<&ConfigTargetOptions>,
    provider_input_by_name: &HashMap<Arc<str>, Arc<ConfigInput>>,
) -> Result<String, TuliproxError> {
    let effective_url = m3u_effective_url(m3u);
    if !effective_url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return Ok(m3u.to_m3u(target_options, false));
    }

    let input = provider_input_by_name.get(&m3u.input_name).ok_or_else(|| {
        TuliproxError::RepositoryM3u(format!("Input '{}' not found for provider URL resolution", m3u.input_name))
    })?;

    let resolved = input.resolve_url(effective_url)?;
    if resolved.as_ref() == effective_url {
        return Ok(m3u.to_m3u(target_options, false));
    }

    let mut line = m3u.to_m3u(target_options, false);
    replace_m3u_url_line(&mut line, resolved.as_ref());
    Ok(line)
}

fn m3u_items(playlist: &[PlaylistGroup]) -> Vec<M3uPlaylistItem> {
    playlist
        .iter()
        .flat_map(|group| &group.channels)
        .filter(|item| !item.header.item_type.is_series_info())
        .map(M3uPlaylistItem::from)
        .collect()
}

fn store_m3u_tree(path: &Path, items: Vec<M3uPlaylistItem>) -> Result<crate::PlaylistM3uStorage, TuliproxError> {
    let mut tree = BPlusTree::new();
    for item in items {
        tree.insert(item.virtual_id.get(), item);
    }
    tree.store_with_index(path, |item| item.source_ordinal)
        .map_err(|err| cant_write_result!(RepositoryM3u, "m3u", path, err))?;
    Ok(tree)
}

async fn persist_m3u_playlist_as_text(
    app_config: &AppConfig,
    target: &ConfigTarget,
    target_output: &M3uTargetOutput,
    database: &Path,
    m3u_playlist: &[M3uPlaylistItem],
) -> Result<(), TuliproxError> {
    let cfg = app_config.config.load_full();
    let Some(m3u_filename) = m3u_text_filename(&cfg, target_output) else {
        return Ok(());
    };

    let target_options = target.options.as_ref();
    let provider_input_by_name = m3u_provider_inputs(app_config, target);

    if provider_input_by_name.is_empty() {
        write_m3u_text_file_atomic(&m3u_filename, m3u_playlist, |m3u| Ok(m3u.to_m3u(target_options, false))).await?;
    } else {
        let (m3u_filename, provider) =
            checked_m3u_text_paths(app_config, cfg, target, m3u_filename, true, false).await?;
        let mut files = stage_m3u_text_exports(
            (&m3u_filename, provider),
            database,
            m3u_playlist,
            target_options,
            &provider_input_by_name,
            ProviderExportOwnership::AdoptLegacy,
            uuid::Uuid::new_v4().simple(),
        )
        .await?;
        task::spawn_blocking(move || {
            publish_target_file_replacements(&mut files, &TargetEmptyReplacementMode::Persist)
        })
        .await
        .map_err(|err| cant_write_result!(RepositoryM3u, "text export", &m3u_filename, err))?
        .map_err(|err| cant_write_result!(RepositoryM3u, "text export", &m3u_filename, err))?;
    }

    Ok(())
}

pub async fn m3u_write_playlist(
    cfg: &AppConfig,
    target: &ConfigTarget,
    target_output: &M3uTargetOutput,
    target_path: &Path,
    new_playlist: &[PlaylistGroup],
    allow_empty_publication: bool,
) -> Result<(), TuliproxError> {
    if new_playlist.is_empty() && !allow_empty_publication {
        return Ok(());
    }

    let config = cfg.config.load();
    let _m3u_path = ensure_m3u_storage_path(&config, target.name.as_str()).await?;

    let m3u_path = m3u_get_file_path_for_db(target_path);
    let m3u_playlist = m3u_items(new_playlist);

    let file_lock = cfg.file_locks.write_lock(&m3u_path).await;

    persist_m3u_playlist_as_text(cfg, target, target_output, &m3u_path, &m3u_playlist).await?;

    let m3u_path_clone = m3u_path.clone();

    // Move all B+Tree building and I/O to spawn_blocking
    task::spawn_blocking(move || -> Result<(), TuliproxError> {
        let _guard = file_lock;
        store_m3u_tree(&m3u_path_clone, m3u_playlist).map(|_| ())
    })
    .await
    .map_err(|err| {
        TuliproxError::RepositoryM3u(format!("failed to write m3u playlist: {} - {err}", m3u_path.display()))
    })??;

    Ok(())
}

async fn stage_m3u_text_exports(
    (filename, provider_filename): (&Path, Option<PathBuf>),
    database: &Path,
    items: &[M3uPlaylistItem],
    target_options: Option<&ConfigTargetOptions>,
    provider_inputs: &HashMap<Arc<str>, Arc<ConfigInput>>,
    ownership: ProviderExportOwnership,
    token: uuid::fmt::Simple,
) -> Result<Vec<TargetFileReplacement>, TuliproxError> {
    let mut files = Vec::new();
    if let Some(path) = provider_filename {
        let previous = {
            let (path, database, options) = (path.clone(), database.to_path_buf(), target_options.cloned());
            task::spawn_blocking(move || provider_export_state(&path, &database, options.as_ref(), ownership)).await
        }
        .map_err(|err| cant_read_result!(RepositoryM3u, "provider export", &path, err))?
        .map_err(|err| cant_read_result!(RepositoryM3u, "provider export", &path, err))?;
        let owner_path = provider_export_owner_path(database);
        if provider_inputs.is_empty() {
            if let ProviderExportState::Owned(digest) = previous {
                let mut removal = TargetFileReplacement::new(&path, None, token);
                removal.expect_previous_file(Some(digest));
                files.push(removal);
                files.push(TargetFileReplacement::new(&owner_path, None, token));
            }
        } else {
            let digest = match previous {
                ProviderExportState::Missing => None,
                ProviderExportState::Owned(digest) => Some(digest),
                ProviderExportState::Unverified => {
                    return Err(TuliproxError::RepositoryM3u(format!(
                        "Cannot verify ownership of provider M3U: {}",
                        path.display()
                    )));
                }
            };
            let mut provider = TargetFileReplacement::staged_file(&path, token);
            provider.expect_previous_file(digest);
            write_m3u_text_file(provider.staging_path(), items, |item| Ok(item.to_m3u(target_options, false))).await?;
            let staged_path = provider.staging_path().to_path_buf();
            let digest = task::spawn_blocking(move || file_digest(&staged_path))
                .await
                .map_err(|err| cant_read_result!(RepositoryM3u, "staged provider export", &path, err))?
                .map_err(|err| cant_read_result!(RepositoryM3u, "staged provider export", &path, err))?;
            let owner = TargetFileReplacement::staged_file(&owner_path, token);
            let bytes = serde_json::to_vec(&ProviderExportOwner { path, digest: digest.to_hex().to_string() })
                .map_err(|err| cant_write_result!(RepositoryM3u, "provider export owner", &owner_path, err))?;
            fs::write(owner.staging_path(), bytes)
                .await
                .map_err(|err| cant_write_result!(RepositoryM3u, "provider export owner", &owner_path, err))?;
            files.extend([provider, owner]);
        }
    }

    let text = TargetFileReplacement::staged_file(filename, token);
    write_m3u_text_file(text.staging_path(), items, |item| {
        if provider_inputs.is_empty() {
            Ok(item.to_m3u(target_options, false))
        } else {
            resolved_m3u_line(item, target_options, provider_inputs)
        }
    })
    .await?;
    files.push(text);
    Ok(files)
}

pub(crate) async fn stage_m3u_playlist(
    app: &AppConfig,
    target: &ConfigTarget,
    output: &M3uTargetOutput,
    target_path: &Path,
    playlist: &[PlaylistGroup],
    token: uuid::fmt::Simple,
) -> Result<(crate::PlaylistM3uStorage, Vec<TargetFileReplacement>), TuliproxError> {
    let config = app.config.load_full();
    ensure_m3u_storage_path(&config, &target.name).await?;
    let database = m3u_get_file_path_for_db(target_path);
    let staging = database.with_file_name(format!(".m3u-replace-{token}.db"));
    let mut files = vec![
        TargetFileReplacement::database(&database, &staging, token)
            .map_err(|err| cant_write_result!(RepositoryM3u, "staging", &staging, err))?,
        TargetFileReplacement::new(
            &crate::get_file_path_for_db_index(&database),
            Some(&crate::get_file_path_for_db_index(&staging)),
            token,
        ),
    ];
    let provider_inputs = m3u_provider_inputs(app, target);
    let text_paths = if let Some(filename) = m3u_text_filename(&config, output) {
        Some(checked_m3u_text_paths(app, config, target, filename, !provider_inputs.is_empty(), true).await?)
    } else {
        None
    };
    let target_options = target.options.clone();
    let items = m3u_items(playlist);
    // Let an in-flight text or database write finish before its file owners can drop.
    tokio::spawn(async move {
        if let Some((filename, provider_filename)) = text_paths {
            files.extend(
                stage_m3u_text_exports(
                    (&filename, provider_filename),
                    &database,
                    &items,
                    target_options.as_ref(),
                    &provider_inputs,
                    ProviderExportOwnership::Verify,
                    token,
                )
                .await?,
            );
        }
        crate::xtream_repository::target_publish::write_target_staging(files, move || store_m3u_tree(&staging, items))
            .await
            .map(|(files, tree)| (tree, files))
    })
    .await
    .map_err(|error| TuliproxError::RepositoryM3u(format!("Failed to join M3U staging: {error}")))?
}

pub async fn m3u_load_rewrite_playlist(
    cfg: &AppConfig,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
) -> Result<M3uPlaylistM3uTextIterator, TuliproxError> {
    M3uPlaylistM3uTextIterator::new(cfg, target, user).await
}

pub async fn m3u_get_item_for_stream_id(
    stream_id: u32,
    app_config: &AppConfig,
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
) -> Result<M3uPlaylistItem, Error> {
    if stream_id == 0 {
        return Err(str_to_io_error("id should start with 1"));
    }
    {
        if let Some(playlist) = playlists.data.read().await.get(target.name.as_str()) {
            if let Some(m3u_playlist) = playlist.m3u.as_ref() {
                if let Some(item) = m3u_playlist.query(&stream_id) {
                    return Ok(item.clone());
                }
                // fall through to disk lookup on cache miss
            }
        }

        let cfg: &AppConfig = app_config;
        let target_path = get_target_storage_path(&cfg.config.load(), target.name.as_str())
            .ok_or_else(|| string_to_io_error(format!("Could not find path for target {}", target.name)))?;
        let m3u_path = m3u_get_file_path_for_db(&target_path);
        let file_lock = cfg.file_locks.read_lock(&m3u_path).await;
        let m3u_path_clone = m3u_path.clone();

        task::spawn_blocking(move || -> Result<M3uPlaylistItem, Error> {
            let _guard = file_lock;
            let mut query = BPlusTreeQuery::<u32, M3uPlaylistItem>::try_new(&m3u_path_clone)?;
            match query.query_zero_copy(&stream_id) {
                Ok(Some(item)) => Ok(item),
                Ok(None) => Err(string_to_io_error(format!("Item not found: {stream_id}"))),
                Err(err) => Err(string_to_io_error(format!("Query failed for {stream_id}: {err}"))),
            }
        })
        .await
        .map_err(|err| string_to_io_error(format!("Query task failed for {stream_id}: {err}")))?
    }
}

/// Keep items belonging to `cluster`; keep everything when `None`.
///
/// Returned as an opaque closure so `iter_raw_playlist` monomorphizes over it
/// rather than taking a boxed predicate.
fn m3u_cluster_filter(cluster: Option<XtreamCluster>) -> impl Fn(&M3uPlaylistItem) -> bool + Send + 'static {
    move |item| cluster.is_none_or(|wanted| item.item_type.is_cluster(wanted))
}

pub async fn iter_raw_m3u_target_playlist(
    config: &AppConfig,
    target: &ConfigTarget,
    cluster: Option<XtreamCluster>,
) -> Option<LockedReceiverStream<Result<M3uPlaylistItem, TuliproxError>>> {
    let target_path = get_target_storage_path(&config.config.load(), target.name.as_str())?;
    let m3u_path = m3u_get_file_path_for_db(&target_path);

    iter_raw_playlist::<M3u, u32, _>(config, &m3u_path, m3u_cluster_filter(cluster)).await
}

pub async fn iter_raw_m3u_input_playlist(
    app_config: &AppConfig,
    input: &ConfigInput,
    cluster: Option<XtreamCluster>,
) -> Option<LockedReceiverStream<Result<M3uPlaylistItem, TuliproxError>>> {
    let cfg = app_config.config.load();
    let storage_path = get_input_storage_path(&input.name, &cfg.storage_dir).await.ok()?;
    let m3u_path = get_input_m3u_playlist_file_path(&storage_path, &input.name);

    iter_raw_playlist::<M3u, Arc<str>, _>(app_config, &m3u_path, m3u_cluster_filter(cluster)).await
}

pub async fn persist_input_m3u_playlist(
    app_config: &Arc<AppConfig>,
    m3u_path: &Path,
    playlist: &[PlaylistGroup],
) -> Result<(), TuliproxError> {
    let file_lock = app_config.file_locks.write_lock(m3u_path).await;
    let m3u_path_clone = m3u_path.to_path_buf();
    let stream_url_index_path = get_m3u_stream_url_index_file_path(m3u_path);

    let mut playlist_items: Vec<M3uPlaylistItem> =
        playlist.iter().flat_map(|pg| &pg.channels).map(M3uPlaylistItem::from).collect();

    task::spawn_blocking(move || -> Result<(), TuliproxError> {
        let _guard = file_lock;
        if let Err(err) = merge_preserved_m3u_live_metadata(&m3u_path_clone, &mut playlist_items) {
            warn!(
                "Failed to preserve learned Live metadata during M3U playlist rewrite for {}: {err}",
                m3u_path_clone.display()
            );
        }
        let mut tree = BPlusTree::new();
        for m3u in &playlist_items {
            tree.insert(m3u.provider_id.clone(), m3u.clone());
        }
        tree.store(&m3u_path_clone).map_err(|err| cant_write_result!(RepositoryM3u, "m3u", &m3u_path_clone, err))?;

        let mut indexed_urls: HashMap<Arc<str>, Option<Arc<str>>> = HashMap::new();
        let mut index_url = |url: &Arc<str>| {
            for key in [m3u_stream_url_identity(url), m3u_flussonic_live_file_index_key(url)].into_iter().flatten() {
                if let Some(indexed_url) = indexed_urls.get_mut(key.as_str()) {
                    if indexed_url.as_ref().is_some_and(|indexed| indexed.as_ref() != url.as_ref()) {
                        *indexed_url = None;
                    }
                } else {
                    indexed_urls.insert(key.into(), Some(Arc::clone(url)));
                }
            }
        };
        for item in &playlist_items {
            index_url(&item.url);
            if let Some(StreamProperties::Series(series)) = &item.additional_properties {
                if let Some(episodes) = series.details.as_ref().and_then(|details| details.episodes.as_ref()) {
                    for episode in episodes {
                        index_url(&episode.direct_source);
                    }
                }
            }
        }

        let mut stream_url_index = BPlusTree::new();
        for (identity, url) in indexed_urls {
            if let Some(url) = url {
                stream_url_index.insert(identity, url);
            }
        }
        stream_url_index
            .store(&stream_url_index_path)
            .map_err(|err| cant_write_result!(RepositoryM3u, "m3u stream URL index", &stream_url_index_path, err))?;
        Ok(())
    })
    .await
    .map_err(|err| {
        TuliproxError::RepositoryM3u(format!("failed to write m3u playlist: {} - {err}", m3u_path.display()))
    })??;

    Ok(())
}

pub fn get_m3u_stream_url_index_file_path(m3u_path: &Path) -> PathBuf {
    let mut path = m3u_path.to_path_buf();
    path.set_extension("stream_urls.db");
    path
}

pub async fn load_input_m3u_stream_url(
    app_config: &Arc<AppConfig>,
    input_name: &Arc<str>,
    requested_stream_url: &str,
) -> Result<Option<Arc<str>>, TuliproxError> {
    let Some(identity) = m3u_stream_url_identity(requested_stream_url) else { return Ok(None) };
    load_first_input_m3u_stream_url_by_keys(app_config, input_name, vec![identity]).await
}

/// Resolves the first index key present in the input's M3U stream URL index.
///
/// All keys are queried against one opened index, so callers probing several sibling
/// URLs pay a single lock and blocking task.
pub async fn load_first_input_m3u_stream_url_by_keys(
    app_config: &Arc<AppConfig>,
    input_name: &Arc<str>,
    keys: Vec<String>,
) -> Result<Option<Arc<str>>, TuliproxError> {
    let identities: Vec<Arc<str>> = keys.into_iter().map(Arc::from).collect();
    if identities.is_empty() {
        return Ok(None);
    }
    let cfg = app_config.config.load();
    let storage_path = build_input_storage_path(input_name, &cfg.storage_dir);
    let m3u_path = get_input_m3u_playlist_file_path(&storage_path, input_name);
    let index_path = get_m3u_stream_url_index_file_path(&m3u_path);
    if !file_exists_async(&index_path).await {
        return Ok(None);
    }

    // The playlist writer publishes the playlist and its URL index while holding the
    // playlist-path lock, so readers use the same lock as their generation boundary.
    let file_lock = app_config.file_locks.read_lock(&m3u_path).await;
    task::spawn_blocking(move || {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<Arc<str>, Arc<str>>::try_new(&index_path).map_err(|err| {
            TuliproxError::RepositoryM3u(format!("failed to open M3U stream URL index {}: {err}", index_path.display()))
        })?;
        for identity in &identities {
            let resolved = query.query(identity).map_err(|err| {
                TuliproxError::RepositoryM3u(format!(
                    "failed to read M3U stream URL index {}: {err}",
                    index_path.display()
                ))
            })?;
            if resolved.is_some() {
                return Ok(resolved);
            }
        }
        Ok(None)
    })
    .await
    .map_err(|err| TuliproxError::RepositoryM3u(format!("failed to join M3U stream URL lookup task: {err}")))?
}

fn merge_preserved_m3u_live_metadata(m3u_path: &Path, playlist_items: &mut [M3uPlaylistItem]) -> Result<(), String> {
    if !m3u_path.exists() {
        return Ok(());
    }

    let mut stored = BPlusTreeQuery::<Arc<str>, M3uPlaylistItem>::try_new(m3u_path)
        .map_err(|err| format!("failed to open existing input playlist: {err}"))?;

    for item in playlist_items {
        if item.provider_id.is_empty() {
            continue;
        }
        let previous = stored
            .query(&item.provider_id)
            .map_err(|err| format!("failed to read existing input playlist item: {err}"))?;
        let Some(StreamProperties::Live(previous)) = previous.and_then(|item| item.additional_properties) else {
            continue;
        };

        match item.additional_properties.as_mut() {
            Some(StreamProperties::Live(current)) => {
                current.merge_learned_metadata_from(&previous);
            }
            None if item.item_type.is_live() => {
                let mut current = LiveStreamProperties::default();
                if current.merge_learned_metadata_from(&previous) {
                    item.additional_properties = Some(StreamProperties::Live(Box::new(current)));
                }
            }
            Some(StreamProperties::Video(_) | StreamProperties::Series(_) | StreamProperties::Episode(_)) | None => {}
        }
    }

    Ok(())
}

pub async fn load_input_m3u_playlist(
    app_config: &Arc<AppConfig>,
    m3u_path: &Path,
) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    if !file_exists_async(m3u_path).await {
        return Ok(Vec::new());
    }

    let file_lock = app_config.file_locks.read_lock(m3u_path).await;
    let m3u_path = m3u_path.to_path_buf();
    let m3u_path_err = m3u_path.clone();

    let groups = task::spawn_blocking(move || -> Result<Vec<PlaylistGroup>, TuliproxError> {
        let _guard = file_lock;
        let mut groups: IndexMap<CategoryKey, PlaylistGroup> = IndexMap::new();
        let mut query = BPlusTreeQuery::<Arc<str>, M3uPlaylistItem>::try_new(&m3u_path)
            .map_err(|error| TuliproxError::RepositoryM3u(error.to_string()))?;
        let mut group_cnt = 0;
        for entry in query.iter() {
            let (_, item) = entry.map_err(|error| TuliproxError::RepositoryM3u(error.to_string()))?;
            let cluster = item.item_type.cluster();
            let key = (cluster, item.group.clone());
            groups
                .entry(key)
                .or_insert_with(|| {
                    group_cnt += 1;
                    PlaylistGroup {
                        id: group_cnt,
                        title: item.group.clone(),
                        channels: Vec::new(),
                        xtream_cluster: cluster,
                    }
                })
                .channels
                .push(PlaylistItem::from(&item));
        }
        Ok(groups.into_values().collect())
    })
    .await
    .map_err(|err| cant_read_result!(RepositoryM3u, "m3u", &m3u_path_err, err))??;

    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::{load_input_m3u_stream_url, persist_input_m3u_playlist, replace_m3u_url_line};
    use crate::{
        get_input_m3u_playlist_file_path, get_input_storage_path,
        test_config::{app_config, app_config_in},
        BPlusTreeQuery,
    };
    use shared::{
        model::{
            LiveStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
            SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamProperties,
            StreamProperties, XtreamCluster,
        },
        utils::Internable,
    };
    use std::sync::Arc;

    fn live_playlist(properties: Option<LiveStreamProperties>) -> Vec<PlaylistGroup> {
        vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    id: "provider-1".intern(),
                    input_stream_id: "provider-1".intern(),
                    name: "Channel".intern(),
                    group: "Live".intern(),
                    item_type: PlaylistItemType::Live,
                    xtream_cluster: XtreamCluster::Live,
                    additional_properties: properties.map(|properties| StreamProperties::Live(Box::new(properties))),
                    ..PlaylistItemHeader::default()
                },
            }],
            xtream_cluster: XtreamCluster::Live,
        }]
    }

    #[test]
    fn replace_m3u_url_line_trims_trailing_whitespace() {
        let mut line =
            "#EXTINF:-1 tvg-id=\"1\" tvg-name=\"Test\" group-title=\"G\",Title\nhttp://old\n\n  \t".to_string();
        replace_m3u_url_line(&mut line, "http://new");
        assert_eq!(line, "#EXTINF:-1 tvg-id=\"1\" tvg-name=\"Test\" group-title=\"G\",Title\nhttp://new");
    }

    #[test]
    fn replace_m3u_url_line_handles_crlf() {
        let mut line = "#EXTINF:-1,Title\r\nhttp://old\r\n".to_string();
        replace_m3u_url_line(&mut line, "http://new");
        assert_eq!(line, "#EXTINF:-1,Title\r\nhttp://new");
    }

    #[test]
    fn replace_m3u_url_line_handles_no_metadata() {
        let mut line = "http://old".to_string();
        replace_m3u_url_line(&mut line, "http://new");
        assert_eq!(line, "http://old\nhttp://new");
    }

    #[tokio::test]
    async fn persist_input_m3u_playlist_preserves_learned_live_metadata_across_full_rewrite() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("input.db");
        let app_config = app_config();
        let learned = LiveStreamProperties {
            video: Some("learned-video".intern()),
            audio: Some("learned-audio".intern()),
            last_probed_timestamp: Some(100),
            last_success_timestamp: Some(90),
            bitrate: 2_500_000,
            ..LiveStreamProperties::default()
        };

        persist_input_m3u_playlist(&app_config, &path, &live_playlist(Some(learned)))
            .await
            .expect("initial playlist should persist");
        persist_input_m3u_playlist(&app_config, &path, &live_playlist(None))
            .await
            .expect("rewritten playlist should persist");

        let mut query = BPlusTreeQuery::<Arc<str>, shared::model::M3uPlaylistItem>::try_new(&path)
            .expect("rewritten playlist should open");
        let item = query
            .query(&"provider-1".intern())
            .expect("rewritten playlist should be readable")
            .expect("live item should remain present");
        let Some(StreamProperties::Live(properties)) = item.additional_properties else {
            panic!("expected live properties");
        };

        assert_eq!(properties.video.as_deref(), Some("learned-video"));
        assert_eq!(properties.audio.as_deref(), Some("learned-audio"));
        assert_eq!(properties.last_probed_timestamp, Some(100));
        assert_eq!(properties.last_success_timestamp, Some(90));
        assert_eq!(properties.bitrate, 2_500_000);
    }

    #[tokio::test]
    async fn persisted_stream_url_index_resolves_series_episode_alias_token() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let app_config = app_config_in(temp.path());

        let alias_name = "backup-account".intern();
        let storage_path = get_input_storage_path(&alias_name, &app_config.config.load().storage_dir)
            .await
            .expect("alias storage should be created");
        let playlist_path = get_input_m3u_playlist_file_path(&storage_path, &alias_name);
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    id: "series-1".intern(),
                    url: "".intern(),
                    item_type: PlaylistItemType::SeriesInfo,
                    xtream_cluster: XtreamCluster::Series,
                    additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                        details: Some(SeriesStreamDetailProperties::new(
                            None,
                            Vec::new(),
                            Some(vec![SeriesStreamDetailEpisodeProperties {
                                direct_source: "http://stream.example/vod/episode.mkv?token=backup-stream-token"
                                    .intern(),
                                ..SeriesStreamDetailEpisodeProperties::default()
                            }]),
                        )),
                        ..SeriesStreamProperties::default()
                    }))),
                    ..PlaylistItemHeader::default()
                },
            }],
            xtream_cluster: XtreamCluster::Series,
        }];
        persist_input_m3u_playlist(&app_config, &playlist_path, &playlist)
            .await
            .expect("alias playlist should persist");

        let resolved = load_input_m3u_stream_url(
            &app_config,
            &alias_name,
            "http://stream.example/vod/episode.mkv?token=primary-stream-token",
        )
        .await
        .expect("alias episode URL lookup should succeed");

        assert_eq!(resolved.as_deref(), Some("http://stream.example/vod/episode.mkv?token=backup-stream-token"));
    }

    #[tokio::test]
    async fn persisted_stream_url_index_resolves_account_specific_alias_token() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let app_config = app_config_in(temp.path());

        let alias_name = "backup-account".intern();
        let storage_path = get_input_storage_path(&alias_name, &app_config.config.load().storage_dir)
            .await
            .expect("alias storage should be created");
        let playlist_path = get_input_m3u_playlist_file_path(&storage_path, &alias_name);
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    id: "channel-323".intern(),
                    input_stream_id: "channel-323".intern(),
                    url: "http://stream.example:4000/323/mono.m3u8?token=backup-stream-token".intern(),
                    item_type: PlaylistItemType::Live,
                    xtream_cluster: XtreamCluster::Live,
                    ..PlaylistItemHeader::default()
                },
            }],
            xtream_cluster: XtreamCluster::Live,
        }];
        persist_input_m3u_playlist(&app_config, &playlist_path, &playlist)
            .await
            .expect("alias playlist should persist");

        let resolved = load_input_m3u_stream_url(
            &app_config,
            &alias_name,
            "http://stream.example:4000/323/mono.m3u8?token=primary-stream-token",
        )
        .await
        .expect("alias URL lookup should succeed");

        assert_eq!(resolved.as_deref(), Some("http://stream.example:4000/323/mono.m3u8?token=backup-stream-token"));
    }
}
