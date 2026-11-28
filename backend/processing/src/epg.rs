use crate::{metadata_sink::MetadataUpdateSink, parser::xmltv::TVGuide, processor::PlaylistProcessingContext};
use log::{debug, warn};
use shared::{
    concat_string,
    error::TuliproxError,
    model::{EventSink, InputFetchMethod, PluginHost},
    utils::{hash_bytes, hex_encode, sanitize_sensitive_info, short_hash},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{ConfigInput, EpgConfig, EpgSource, EpgSourceType, PersistedEpgSource, PersistedEpgSourceKind},
    utils::{add_prefix_to_filename, prepare_file_path, request},
};
use tuliprox_repository::get_input_storage_path;

pub async fn get_input_raw_epg_file_path(
    epg_source: &EpgSource,
    input: &ConfigInput,
    storage_dir: &str,
) -> std::io::Result<PathBuf> {
    let cache_key = epg_cache_key(epg_source);
    let file_prefix = short_hash(&cache_key);
    let suffix = epg_source_cache_suffix(epg_source.source_type);
    let extension = epg_source_cache_extension(epg_source.source_type);

    if let Some(persist_path) = input.persist.as_deref() {
        if !persist_path.is_empty() {
            if let Some(path) = prepare_file_path(input.persist.as_deref(), storage_dir, "").map(|path| {
                add_prefix_to_filename(&path, concat_string!(&file_prefix, "_epg_").as_str(), Some(extension))
            }) {
                return Ok(path);
            }
        }
    }

    let download_path = get_input_storage_path(&input.name, storage_dir).await?;
    Ok(download_path.join(format!("{file_prefix}{suffix}")))
}

// Used only by the playlist API's tests, which build EPG cache paths directly.
// Published under `test-support` because those tests live in another crate.
#[cfg(any(test, feature = "test-support"))]
pub async fn get_input_raw_xmltv_file_path(
    url: &str,
    input: &ConfigInput,
    storage_dir: &str,
) -> std::io::Result<PathBuf> {
    let source = EpgSource {
        source_type: EpgSourceType::Xmltv,
        url: url.to_string(),
        priority: 0,
        logo_override: false,
        channel_id: None,
        channel_title: None,
        match_names: Vec::new(),
        ics: None,
    };
    get_input_raw_epg_file_path(&source, input, storage_dir).await
}

fn epg_cache_key(epg_source: &EpgSource) -> String {
    match epg_source.source_type {
        EpgSourceType::Xmltv => epg_source.url.clone(),
        EpgSourceType::Ics => {
            format!("ics|{}|{}", epg_source.url, epg_source.channel_id.as_deref().unwrap_or_default())
        }
    }
}

fn epg_source_cache_suffix(source_type: EpgSourceType) -> &'static str {
    match source_type {
        EpgSourceType::Xmltv => "_epg.xml",
        EpgSourceType::Ics => "_epg.ics",
    }
}

fn epg_source_cache_extension(source_type: EpgSourceType) -> &'static str {
    match source_type {
        EpgSourceType::Xmltv => "xml",
        EpgSourceType::Ics => "ics",
    }
}

async fn download_epg_file<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    epg_source: &EpgSource,
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &ConfigInput,
    headers: Option<&reqwest::header::HeaderMap>,
    storage_dir: &str,
) -> Result<PathBuf, TuliproxError> {
    debug!("Getting epg file path for url: {}", sanitize_sensitive_info(&epg_source.url));
    let persist_file_path = get_input_raw_epg_file_path(epg_source, input, storage_dir)
        .await
        .map_err(|e| TuliproxError::Io(format!("Could not access epg file download directory: {e}")))?;

    if input.cache_duration_seconds > 0 {
        let _cache_lock = ctx.config.file_locks.read_lock(&persist_file_path).await;
        if let Ok(metadata) = tokio::fs::metadata(&persist_file_path).await {
            if let Ok(modified) = metadata.modified() {
                if let Ok(elapsed) = std::time::SystemTime::now().duration_since(modified) {
                    if elapsed.as_secs() < input.cache_duration_seconds {
                        debug!("Using cached epg file: {}", persist_file_path.display());
                        return Ok(persist_file_path);
                    }
                }
            }
        }
    }

    let download_key = epg_download_key(epg_source, ctx, input, headers);
    let _download_lock = ctx.get_input_lock(&download_key).await;

    let cached_path = {
        let downloads = ctx.epg_downloads.lock().await;
        downloads.get(&download_key).cloned()
    };

    if let Some(downloaded_path) = cached_path {
        match link_or_copy_epg_file(&ctx.config.file_locks, &downloaded_path, &persist_file_path).await {
            Ok(true) => return Ok(persist_file_path),
            Ok(false) => {}
            Err(err) => {
                debug!("Failed to reuse cached epg file for input '{}' ({err}); falling back to download", input.name);
            }
        }
    }

    debug!("Downloading epg for input '{}'", input.name);
    let max_bytes = epg_download_limit(epg_source);
    let path = request::get_input_epg_content_as_file(
        &ctx.config,
        &ctx.client,
        input,
        request::InputEpgFileRequest {
            headers,
            storage_dir,
            url: &epg_source.url,
            persist_path: &persist_file_path,
            max_bytes,
        },
    )
    .await?;

    {
        let mut downloads = ctx.epg_downloads.lock().await;
        downloads.retain(|k, v| k == &download_key || v != &path);
        downloads.insert(download_key, path.clone());
    }
    Ok(path)
}

fn push_len_prefixed(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    buf.extend_from_slice(bytes);
}

/// Computes an opaque hash key for deduplicating EPG downloads within a run.
///
/// The raw key includes credentials in the source URL and request headers, so it
/// is hashed before storage and never logged.
fn epg_download_key<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    epg_source: &EpgSource,
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &ConfigInput,
    headers: Option<&reqwest::header::HeaderMap>,
) -> Arc<str> {
    let custom_headers = headers
        .map(|h| h.iter().map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec())).collect::<HashMap<_, _>>());
    let (default_user_agent, disabled_headers) = {
        let config = ctx.config.config.load();
        (config.default_user_agent.clone(), config.get_disabled_headers())
    };
    let effective_headers = request::get_request_headers(
        Some(&input.headers),
        custom_headers.as_ref(),
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
    );
    let mut header_entries: Vec<(&str, &[u8])> =
        effective_headers.iter().map(|(name, value)| (name.as_str(), value.as_bytes())).collect();
    header_entries.sort_unstable();

    let mut key_bytes = Vec::new();
    key_bytes.extend_from_slice(b"tuliprox.epg-download-key.v1");
    key_bytes.push(match epg_source.source_type {
        EpgSourceType::Xmltv => 0,
        EpgSourceType::Ics => 1,
    });
    key_bytes.push(match input.method {
        InputFetchMethod::GET => 0,
        InputFetchMethod::POST => 1,
    });
    push_len_prefixed(&mut key_bytes, epg_source.url.as_bytes());
    key_bytes.extend_from_slice(&u64::try_from(header_entries.len()).unwrap_or(u64::MAX).to_be_bytes());
    for (name, value) in header_entries {
        push_len_prefixed(&mut key_bytes, name.as_bytes());
        push_len_prefixed(&mut key_bytes, value);
    }
    match epg_download_limit(epg_source) {
        None => key_bytes.push(0),
        Some(limit) => {
            key_bytes.push(1);
            key_bytes.extend_from_slice(&limit.to_be_bytes());
        }
    }

    let digest = hash_bytes(&key_bytes);
    Arc::from(format!("epg:{}", hex_encode(digest.as_ref())))
}

fn epg_cache_parent(path: &Path) -> &Path {
    path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."))
}

async fn copy_epg_file_atomically(source_path: &Path, dest_path: &Path) -> Result<bool, TuliproxError> {
    let parent = epg_cache_parent(dest_path);
    let temp_file = tempfile::Builder::new()
        .prefix(".tuliprox-epg-copy-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|e| TuliproxError::Io(format!("Could not create temp epg cache file: {e}")))?;
    if let Err(err) = tokio::fs::copy(source_path, temp_file.path()).await {
        if err.kind() == std::io::ErrorKind::NotFound {
            return Ok(false);
        }
        return Err(TuliproxError::Io(format!(
            "Could not copy cached epg file {} to {}: {err}",
            source_path.display(),
            dest_path.display()
        )));
    }
    temp_file.persist(dest_path).map_err(|e| {
        TuliproxError::Io(format!("Could not persist copied epg file {}: {}", dest_path.display(), e.error))
    })?;
    Ok(true)
}

#[cfg(test)]
tokio::task_local! {
    static TEST_HARD_LINK_FAILURE: bool;
}

async fn hard_link_to_temp(source_path: &Path, parent: &Path) -> std::io::Result<tempfile::TempPath> {
    #[cfg(test)]
    if TEST_HARD_LINK_FAILURE.try_with(|fail| *fail).unwrap_or(false) {
        return Err(std::io::Error::new(std::io::ErrorKind::CrossesDevices, "simulated hard-link failure"));
    }

    let source = source_path.to_path_buf();
    let parent_dir = parent.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let temp_file = tempfile::Builder::new()
            .prefix(".tuliprox-epg-link-")
            .suffix(".tmp")
            .make_in(&parent_dir, |temp_path| std::fs::hard_link(&source, temp_path))?;
        Ok(temp_file.into_temp_path())
    })
    .await
    .map_err(std::io::Error::other)?
}

async fn link_or_copy_epg_file(
    file_locks: &tuliprox_core::utils::FileLockManager,
    source_path: &Path,
    dest_path: &Path,
) -> Result<bool, TuliproxError> {
    if source_path == dest_path {
        let _read_lock = file_locks.read_lock(dest_path).await;
        return Ok(tokio::fs::metadata(dest_path).await.is_ok_and(|metadata| metadata.is_file()));
    }

    {
        let _source_lock = file_locks.read_lock(source_path).await;
        if !tokio::fs::metadata(source_path).await.is_ok_and(|metadata| metadata.is_file()) {
            return Ok(false);
        }
    }

    let _dest_lock = file_locks.write_lock(dest_path).await;
    let parent = epg_cache_parent(dest_path);
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| TuliproxError::Io(format!("Could not create epg cache directory {}: {e}", parent.display())))?;

    match hard_link_to_temp(source_path, parent).await {
        Ok(temp_link) => {
            let rename_result = tokio::fs::rename(&temp_link, dest_path).await;
            drop(temp_link);
            rename_result.map_err(|err| {
                TuliproxError::Io(format!("Could not replace existing epg cache file {}: {err}", dest_path.display()))
            })?;
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(link_err) => {
            debug!(
                "Hard-linking cached epg file {} to {} failed ({link_err}); falling back to copy",
                source_path.display(),
                dest_path.display()
            );
            copy_epg_file_atomically(source_path, dest_path).await
        }
    }
}

fn epg_download_limit(epg_source: &EpgSource) -> Option<u64> {
    match epg_source.source_type {
        EpgSourceType::Xmltv => None,
        EpgSourceType::Ics => epg_source.ics.as_ref().map(|config| config.max_download_bytes),
    }
}

async fn cleanup_unlisted_epg_files(
    file_locks: &tuliprox_core::utils::FileLockManager,
    keep_files: &[PathBuf],
    suffix: &str,
) -> std::io::Result<()> {
    let keep_set: HashSet<&Path> = keep_files.iter().map(PathBuf::as_path).collect();
    let directories: HashSet<&Path> = keep_files.iter().filter_map(|file| file.parent()).collect();

    for directory in directories {
        let mut entries = tokio::fs::read_dir(directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let suffix_matches =
                path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.ends_with(suffix));
            if keep_set.contains(path.as_path()) || !suffix_matches {
                continue;
            }

            let _file_lock = file_locks.write_lock(&path).await;
            if tokio::fs::metadata(&path).await.is_ok_and(|metadata| metadata.is_file()) {
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => log::trace!("Deleted {:?}", path.display()),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
        }
    }

    Ok(())
}

pub fn epg_source_label(raw_url: &str) -> String {
    if raw_url.eq_ignore_ascii_case("auto") {
        return "auto".to_string();
    }
    if let Some(file_path) = raw_url.strip_prefix("file://") {
        if let Some(name) = std::path::Path::new(file_path).file_name().and_then(|s| s.to_str()) {
            if !name.is_empty() {
                return name.to_string();
            }
        }
        return "file".to_string();
    }
    if let Ok(parsed) = reqwest::Url::parse(raw_url) {
        if let Some(host) = parsed.host_str() {
            return host.to_string();
        }
        if let Some(name) = std::path::Path::new(parsed.path()).file_name().and_then(|s| s.to_str()) {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    if let Some(name) = std::path::Path::new(raw_url).file_name().and_then(|s| s.to_str()) {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    "epg".to_string()
}

enum EpgFetchOutcome {
    Success { source_idx: usize, label: String, timestamp: u64 },
    Failure { source_idx: usize, label: String, timestamp: u64, error: String },
}

pub async fn get_xmltv<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &ConfigInput,
    headers: Option<&reqwest::header::HeaderMap>,
    storage_dir: &str,
) -> (Option<TVGuide>, Vec<TuliproxError>) {
    match &input.epg {
        None => (None, vec![]),
        Some(epg_config) if ctx.is_input_in_maintenance(&input.name) => {
            persisted_epg_for_maintenance(ctx, input, epg_config, storage_dir).await
        }
        Some(epg_config) => {
            let mut errors = vec![];
            let mut file_paths = vec![];
            let mut stored_file_paths = vec![];
            let mut fetch_outcomes = Vec::with_capacity(epg_config.sources.len());

            for (source_idx, epg_source) in epg_config.sources.iter().enumerate() {
                let label = epg_source_label(&epg_source.url);
                let now = shared::utils::current_time_secs();
                match download_epg_file(epg_source, ctx, input, headers, storage_dir).await {
                    Ok(file_path) => {
                        fetch_outcomes.push(EpgFetchOutcome::Success { source_idx, label, timestamp: now });
                        stored_file_paths.push(file_path.clone());
                        match persisted_source_from_config(epg_source, source_idx, file_path) {
                            Ok(persisted) => file_paths.push(persisted),
                            Err(err) => errors.push(err),
                        }
                    }
                    Err(err) => {
                        let err_str = err.to_string();
                        let sanitized = sanitize_sensitive_info(&err_str);
                        let short_err = sanitized.chars().take(200).collect::<String>();
                        fetch_outcomes.push(EpgFetchOutcome::Failure {
                            source_idx,
                            label,
                            timestamp: now,
                            error: short_err,
                        });
                        errors.push(err);
                    }
                }
            }

            let input_storage = crate::input_cache::resolve_input_storage_path(storage_dir, &input.name).await;
            // Cluster acquisition and input completion also modify this status file.
            let input_lock = ctx.get_input_lock(&input.name).await;
            let source_count = epg_config.sources.len();
            crate::input_cache::update_input_status(&input_storage, move |status| {
                for outcome in fetch_outcomes {
                    match outcome {
                        EpgFetchOutcome::Success { source_idx, label, timestamp } => {
                            crate::input_cache::update_epg_source_success(status, source_idx, label, timestamp);
                        }
                        EpgFetchOutcome::Failure { source_idx, label, timestamp, error } => {
                            crate::input_cache::update_epg_source_failure(status, source_idx, label, timestamp, error);
                        }
                    }
                }
                status.epg_sources.truncate(source_count);
                true
            })
            .await;
            drop(input_lock);

            for suffix in ["_epg.xml", "_epg.ics"] {
                if let Err(err) = cleanup_unlisted_epg_files(&ctx.config.file_locks, &stored_file_paths, suffix).await {
                    warn!("Failed to clean up stale {suffix} files: {err}");
                }
            }

            if file_paths.is_empty() {
                (None, errors)
            } else {
                (Some(TVGuide::new(file_paths).with_file_locks(std::sync::Arc::clone(&ctx.config.file_locks))), errors)
            }
        }
    }
}

/// The EPG files fetched last, without contacting any source and without cleanup:
/// deleting "unlisted" files here would throw away exactly what maintenance keeps.
async fn persisted_epg_for_maintenance<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &ConfigInput,
    epg_config: &EpgConfig,
    storage_dir: &str,
) -> (Option<TVGuide>, Vec<TuliproxError>) {
    debug!("Input '{}' is in maintenance; using its persisted epg", input.name);
    let mut errors = vec![];
    let mut file_paths = vec![];
    for (source_idx, epg_source) in epg_config.sources.iter().enumerate() {
        let Ok(path) = get_input_raw_epg_file_path(epg_source, input, storage_dir).await else {
            continue;
        };
        if tokio::fs::metadata(&path).await.is_ok_and(|metadata| metadata.is_file()) {
            match persisted_source_from_config(epg_source, source_idx, path) {
                Ok(persisted) => file_paths.push(persisted),
                Err(err) => errors.push(err),
            }
        }
    }
    if file_paths.is_empty() {
        (None, errors)
    } else {
        (Some(TVGuide::new(file_paths).with_file_locks(std::sync::Arc::clone(&ctx.config.file_locks))), errors)
    }
}

fn persisted_source_from_config(
    epg_source: &EpgSource,
    source_idx: usize,
    file_path: PathBuf,
) -> Result<PersistedEpgSource, TuliproxError> {
    let kind = match epg_source.source_type {
        EpgSourceType::Xmltv => PersistedEpgSourceKind::Xmltv,
        EpgSourceType::Ics => {
            let channel_id = epg_source
                .channel_id
                .clone()
                .ok_or_else(|| TuliproxError::ConfigEpg("channel_id is required for ICS EPG sources".to_string()))?;
            PersistedEpgSourceKind::Ics {
                channel_id,
                channel_title: epg_source.channel_title.clone(),
                match_names: epg_source.match_names.clone(),
                config: Box::new(epg_source.ics.clone().unwrap_or_default()),
            }
        }
    };

    Ok(PersistedEpgSource {
        file_path,
        priority: epg_source.priority,
        logo_override: epg_source.logo_override,
        kind,
        source_idx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::utils::Internable;
    use tempfile::tempdir;
    use tuliprox_core::{model::ConfigInput, utils::FileLockManager};

    fn source(source_type: EpgSourceType, url: &str, channel_id: Option<&str>) -> EpgSource {
        EpgSource {
            source_type,
            url: url.to_string(),
            priority: 0,
            logo_override: false,
            channel_id: channel_id.map(Internable::intern),
            channel_title: None,
            match_names: Vec::new(),
            ics: None,
        }
    }

    #[tokio::test]
    async fn epg_cache_path_uses_xmltv_suffix() {
        let dir = tempdir().expect("temp dir");
        let input = ConfigInput { name: "input".intern(), ..ConfigInput::default() };
        let path = get_input_raw_epg_file_path(
            &source(EpgSourceType::Xmltv, "https://example.com/xmltv.xml", None),
            &input,
            dir.path().to_string_lossy().as_ref(),
        )
        .await
        .expect("path");
        assert!(path.to_string_lossy().ends_with("_epg.xml"));
    }

    #[tokio::test]
    async fn epg_cache_path_uses_ics_suffix() {
        let dir = tempdir().expect("temp dir");
        let input = ConfigInput { name: "input".intern(), ..ConfigInput::default() };
        let path = get_input_raw_epg_file_path(
            &source(EpgSourceType::Ics, "https://example.com/f1.ics", Some("f1.calendar")),
            &input,
            dir.path().to_string_lossy().as_ref(),
        )
        .await
        .expect("path");
        assert!(path.to_string_lossy().ends_with("_epg.ics"));
    }

    #[tokio::test]
    async fn same_ics_url_with_different_channel_id_gets_different_cache_path() {
        let dir = tempdir().expect("temp dir");
        let input = ConfigInput { name: "input".intern(), ..ConfigInput::default() };
        let first = get_input_raw_epg_file_path(
            &source(EpgSourceType::Ics, "https://example.com/calendar.ics", Some("one")),
            &input,
            dir.path().to_string_lossy().as_ref(),
        )
        .await
        .expect("first path");
        let second = get_input_raw_epg_file_path(
            &source(EpgSourceType::Ics, "https://example.com/calendar.ics", Some("two")),
            &input,
            dir.path().to_string_lossy().as_ref(),
        )
        .await
        .expect("second path");
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn epg_cleanup_waits_for_readers_and_only_removes_unlisted_matching_files() {
        let dir = tempdir().expect("temp dir");
        let keep = dir.path().join("keep_epg.ics");
        let stale = dir.path().join("stale_epg.ics");
        let other = dir.path().join("other_epg.xml");
        tokio::fs::write(&keep, b"keep").await.expect("write kept file");
        tokio::fs::write(&stale, b"stale").await.expect("write stale file");
        tokio::fs::write(&other, b"other").await.expect("write non-matching file");
        let file_locks = FileLockManager::default();
        let read_guard = file_locks.read_lock(&stale).await;
        let keep_files = vec![keep.clone()];
        let cleanup = cleanup_unlisted_epg_files(&file_locks, &keep_files, "_epg.ics");
        tokio::pin!(cleanup);

        assert!(tokio::time::timeout(std::time::Duration::from_millis(25), cleanup.as_mut()).await.is_err());
        assert!(stale.exists());

        drop(read_guard);
        cleanup.await.expect("cleanup after reader release");
        assert!(keep.exists());
        assert!(!stale.exists());
        assert!(other.exists());
    }

    #[test]
    fn xmltv_source_does_not_apply_ics_download_limit() {
        let mut epg_source = source(EpgSourceType::Xmltv, "https://example.com/xmltv.xml", None);
        epg_source.ics = Some(tuliprox_core::model::IcsEpgSourceConfig {
            max_download_bytes: 123,
            ..tuliprox_core::model::IcsEpgSourceConfig::default()
        });

        assert_eq!(epg_download_limit(&epg_source), None);
    }

    #[test]
    fn ics_source_applies_configured_download_limit() {
        let mut epg_source = source(EpgSourceType::Ics, "https://example.com/calendar.ics", Some("calendar"));
        epg_source.ics = Some(tuliprox_core::model::IcsEpgSourceConfig {
            max_download_bytes: 123,
            ..tuliprox_core::model::IcsEpgSourceConfig::default()
        });

        assert_eq!(epg_download_limit(&epg_source), Some(123));
    }

    #[test]
    fn test_epg_error_truncation_multibyte_utf8() {
        // Multi-byte Unicode: Euro symbol € is 3 bytes (0xE2, 0x82, 0xAC).
        // 250 Euro characters = 750 bytes. Byte slicing at [..200] would split in the middle of a char and panic.
        let raw = "€".repeat(250);
        let truncated: String = raw.chars().take(200).collect();
        assert_eq!(truncated.chars().count(), 200);
        assert_eq!(truncated, "€".repeat(200));

        // Mixed 4-byte emoji and ASCII
        let mixed = format!("Error: {}", "🔥".repeat(250));
        let truncated_mixed: String = mixed.chars().take(200).collect();
        assert_eq!(truncated_mixed.chars().count(), 200);
        assert!(truncated_mixed.starts_with("Error: 🔥"));
    }

    struct MockEpgResponse {
        status: &'static str,
        body: String,
        wait_for: Option<tokio::sync::oneshot::Receiver<()>>,
        entered: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl MockEpgResponse {
        fn ok(body: impl Into<String>) -> Self {
            Self { status: "200 OK", body: body.into(), wait_for: None, entered: None }
        }

        fn error(status: &'static str) -> Self {
            Self { status, body: "error".to_string(), wait_for: None, entered: None }
        }
    }

    struct RecordingEpgServer {
        base_url: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl RecordingEpgServer {
        async fn start_with_responses(responses: Vec<MockEpgResponse>, default_body: String) -> Self {
            use std::collections::VecDeque;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind local epg test server");
            let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let requests_ref = std::sync::Arc::clone(&requests);
            let queue = std::sync::Arc::new(tokio::sync::Mutex::new(VecDeque::from(responses)));
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let requests_ref = std::sync::Arc::clone(&requests_ref);
                    let queue = std::sync::Arc::clone(&queue);
                    let default_body = default_body.clone();
                    tokio::spawn(async move {
                        let mut bytes = Vec::new();
                        loop {
                            let mut chunk = [0_u8; 1024];
                            let len = socket.read(&mut chunk).await.expect("read request");
                            if len == 0 {
                                return;
                            }
                            bytes.extend_from_slice(&chunk[..len]);
                            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let req_text = String::from_utf8_lossy(&bytes).into_owned();
                        let target = req_text
                            .lines()
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or_default()
                            .to_string();
                        requests_ref.lock().expect("lock requests").push(target);

                        let next = queue.lock().await.pop_front();
                        let (status, body) = match next {
                            Some(mut resp) => {
                                if let Some(entered) = resp.entered.take() {
                                    let _ = entered.send(());
                                }
                                if let Some(wait_for) = resp.wait_for.take() {
                                    let _ = wait_for.await;
                                }
                                (resp.status, resp.body)
                            }
                            None => ("200 OK", default_body),
                        };

                        let response = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            });
            Self { base_url, requests, task }
        }

        async fn start(body: impl Into<String>) -> Self { Self::start_with_responses(Vec::new(), body.into()).await }

        fn request_count(&self) -> usize { self.requests.lock().expect("lock requests").len() }
    }

    impl Drop for RecordingEpgServer {
        fn drop(&mut self) { self.task.abort(); }
    }

    fn test_epg_context(storage_dir: &Path) -> PlaylistProcessingContext<shared::model::NoopSink> {
        use arc_swap::{ArcSwap, ArcSwapOption};
        use shared::model::ConfigPaths;
        use std::sync::Arc;
        use tuliprox_core::model::{
            AppConfig, Config, MediaToolCapabilities, ResourceRetryConfig, ReverseProxyConfig, SourcesConfig,
        };

        let config = Config {
            storage_dir: storage_dir.to_string_lossy().into_owned(),
            reverse_proxy: Some(ReverseProxyConfig {
                resource_rewrite_disabled: false,
                rewrite_secret: [0; 16],
                resource_retry: ResourceRetryConfig { max_attempts: 1, ..ResourceRetryConfig::default() },
                disabled_header: None,
                stream: None,
                cache: None,
                rate_limit: None,
                geoip: None,
                stream_history: None,
                qos_aggregation: None,
                hls_cache: None,
                hls_origin_telemetry: None,
            }),
            ..Config::default()
        };
        config.update_runtime();

        let paths = ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: storage_dir.to_string_lossy().into_owned(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            failover_file_path: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        };
        let app_config = Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(config)),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            plugins: Arc::default(),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(paths)),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
            failover: Arc::default(),
        });
        let client = reqwest::Client::builder().no_proxy().build().expect("no-proxy client");
        PlaylistProcessingContext::for_epg_test(app_config, client)
    }

    fn xmltv_body(channel_id: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><tv><channel id=\"{channel_id}\"><display-name>{channel_id}</display-name></channel></tv>"
        )
    }

    fn xmltv_input(name: &str, url: &str) -> ConfigInput {
        ConfigInput {
            name: name.intern(),
            epg: Some(EpgConfig {
                sources: vec![source(EpgSourceType::Xmltv, url, None)],
                configured_sources: None,
                smart_match: None,
            }),
            ..ConfigInput::default()
        }
    }

    #[tokio::test]
    async fn same_epg_url_across_two_inputs_downloads_once_per_run() {
        let dir = tempdir().expect("temp dir");
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let body = xmltv_body("shared");
        let server = RecordingEpgServer::start(body.clone()).await;

        let mut dto_a = shared::model::ConfigInputDto {
            id: 1,
            name: "input-a".intern(),
            input_type: shared::model::InputType::Xtream,
            url: server.base_url.clone(),
            username: Some("u".to_string()),
            password: Some("p".to_string()),
            epg: Some(shared::model::EpgConfigDto {
                sources: Some(vec![shared::model::EpgSourceDto { url: "auto".to_string(), ..Default::default() }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        dto_a.prepare_epg(true).expect("prepare auto epg");
        let input_a = ConfigInput::from(&dto_a);
        let expanded_url = format!("{}/xmltv.php?username=u&password=p", server.base_url);
        let input_b = xmltv_input("input-b", &expanded_url);

        let ctx = test_epg_context(dir.path());
        let (guide_a, err_a) = get_xmltv(&ctx, &input_a, None, &storage_dir).await;
        assert!(err_a.is_empty(), "unexpected errors for input_a: {err_a:?}");
        assert!(guide_a.is_some());

        let (guide_b, err_b) = get_xmltv(&ctx, &input_b, None, &storage_dir).await;
        assert!(err_b.is_empty(), "unexpected errors for input_b: {err_b:?}");
        assert!(guide_b.is_some());

        assert_eq!(server.request_count(), 1);

        let path_a = get_input_raw_epg_file_path(&input_a.epg.as_ref().unwrap().sources[0], &input_a, &storage_dir)
            .await
            .expect("path_a");
        let path_b = get_input_raw_epg_file_path(&input_b.epg.as_ref().unwrap().sources[0], &input_b, &storage_dir)
            .await
            .expect("path_b");
        assert_ne!(path_a, path_b);
        assert!(path_a.is_file());
        assert!(path_b.is_file());
        let bytes_a = tokio::fs::read(&path_a).await.expect("read path_a");
        let bytes_b = tokio::fs::read(&path_b).await.expect("read path_b");
        assert_eq!(bytes_a, body.as_bytes());
        assert_eq!(bytes_a, bytes_b);
    }

    #[tokio::test]
    async fn different_request_headers_download_separately() {
        let dir = tempdir().expect("temp dir");
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let server = RecordingEpgServer::start(xmltv_body("hdr")).await;
        let url = format!("{}/guide.xml", server.base_url);

        let mut input_a = xmltv_input("input-a", &url);
        input_a.headers.insert("X-Token".to_string(), "alpha".to_string());
        let mut input_b = xmltv_input("input-b", &url);
        input_b.headers.insert("X-Token".to_string(), "beta".to_string());

        let ctx = test_epg_context(dir.path());
        let (guide_a, err_a) = get_xmltv(&ctx, &input_a, None, &storage_dir).await;
        let (guide_b, err_b) = get_xmltv(&ctx, &input_b, None, &storage_dir).await;
        assert!(err_a.is_empty(), "{err_a:?}");
        assert!(err_b.is_empty(), "{err_b:?}");
        assert!(guide_a.is_some() && guide_b.is_some());
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn fresh_cache_duration_skips_download_without_populating_run_cache() {
        let dir = tempdir().expect("temp dir");
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let fresh_body = xmltv_body("cached-a");
        let server_body = xmltv_body("live-b");
        let server = RecordingEpgServer::start(server_body.clone()).await;
        let url = format!("{}/guide.xml", server.base_url);

        let mut input_a = xmltv_input("input-a", &url);
        input_a.cache_duration_seconds = 3600;
        let path_a = get_input_raw_epg_file_path(&input_a.epg.as_ref().unwrap().sources[0], &input_a, &storage_dir)
            .await
            .expect("path_a");
        tokio::fs::write(&path_a, fresh_body.as_bytes()).await.expect("seed cache_a");

        let input_b = xmltv_input("input-b", &url);

        let ctx = test_epg_context(dir.path());
        let (guide_a, err_a) = get_xmltv(&ctx, &input_a, None, &storage_dir).await;
        assert!(err_a.is_empty(), "{err_a:?}");
        assert!(guide_a.is_some());
        assert_eq!(server.request_count(), 0);

        let (guide_b, err_b) = get_xmltv(&ctx, &input_b, None, &storage_dir).await;
        assert!(err_b.is_empty(), "{err_b:?}");
        assert!(guide_b.is_some());
        assert_eq!(server.request_count(), 1);

        let path_b = get_input_raw_epg_file_path(&input_b.epg.as_ref().unwrap().sources[0], &input_b, &storage_dir)
            .await
            .expect("path_b");
        assert_eq!(tokio::fs::read(&path_a).await.expect("read path_a"), fresh_body.as_bytes());
        assert_eq!(tokio::fs::read(&path_b).await.expect("read path_b"), server_body.as_bytes());
    }

    #[tokio::test]
    async fn failed_first_download_is_retried_by_second_input() {
        let dir = tempdir().expect("temp dir");
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let body = xmltv_body("recovered");
        let server = RecordingEpgServer::start_with_responses(
            vec![MockEpgResponse::error("500 Internal Server Error"), MockEpgResponse::ok(body.clone())],
            body.clone(),
        )
        .await;
        let url = format!("{}/guide.xml", server.base_url);

        let input_a = xmltv_input("input-a", &url);
        let input_b = xmltv_input("input-b", &url);

        let ctx = test_epg_context(dir.path());
        let (guide_a, err_a) = get_xmltv(&ctx, &input_a, None, &storage_dir).await;
        assert!(guide_a.is_none());
        assert_eq!(err_a.len(), 1);
        assert_eq!(server.request_count(), 1);

        let (guide_b, err_b) = get_xmltv(&ctx, &input_b, None, &storage_dir).await;
        assert!(err_b.is_empty(), "{err_b:?}");
        assert!(guide_b.is_some());
        assert_eq!(server.request_count(), 2);

        let path_b = get_input_raw_epg_file_path(&input_b.epg.as_ref().unwrap().sources[0], &input_b, &storage_dir)
            .await
            .expect("path_b");
        assert_eq!(tokio::fs::read(&path_b).await.expect("read path_b"), body.as_bytes());
    }

    #[tokio::test]
    async fn parallel_acquisition_with_same_epg_url_downloads_once() {
        let dir = tempdir().expect("temp dir");
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let body = xmltv_body("parallel");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = RecordingEpgServer::start_with_responses(
            vec![MockEpgResponse {
                status: "200 OK",
                body: body.clone(),
                wait_for: Some(release_rx),
                entered: Some(entered_tx),
            }],
            body.clone(),
        )
        .await;
        let url = format!("{}/guide.xml", server.base_url);

        let input_a = xmltv_input("input-a", &url);
        let input_b = xmltv_input("input-b", &url);
        let ctx_a = test_epg_context(dir.path());
        let ctx_b = ctx_a.clone();

        let mut handle_a = tokio::spawn({
            let ctx_a = ctx_a.clone();
            let input_a = input_a.clone();
            let storage_dir = storage_dir.clone();
            async move { get_xmltv(&ctx_a, &input_a, None, &storage_dir).await }
        });

        // Wait until input_a has sent the single HTTP request and is inside the server handler.
        entered_rx.await.expect("input_a entered server");

        let mut handle_b = tokio::spawn({
            let ctx_b = ctx_b.clone();
            let input_b = input_b.clone();
            let storage_dir = storage_dir.clone();
            async move { get_xmltv(&ctx_b, &input_b, None, &storage_dir).await }
        });

        // input_b must block on the singleflight lock rather than issuing a second request or finishing early.
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), &mut handle_b).await.is_err());
        assert_eq!(server.request_count(), 1);

        release_tx.send(()).expect("release server response");

        let (guide_a, err_a) = (&mut handle_a).await.expect("join input_a");
        let (guide_b, err_b) = (&mut handle_b).await.expect("join input_b");
        assert!(err_a.is_empty(), "{err_a:?}");
        assert!(err_b.is_empty(), "{err_b:?}");
        assert!(guide_a.is_some() && guide_b.is_some());
        assert_eq!(server.request_count(), 1);

        let path_a = get_input_raw_epg_file_path(&input_a.epg.as_ref().unwrap().sources[0], &input_a, &storage_dir)
            .await
            .expect("path_a");
        let path_b = get_input_raw_epg_file_path(&input_b.epg.as_ref().unwrap().sources[0], &input_b, &storage_dir)
            .await
            .expect("path_b");
        assert_eq!(tokio::fs::read(&path_a).await.expect("read path_a"), body.as_bytes());
        assert_eq!(tokio::fs::read(&path_b).await.expect("read path_b"), body.as_bytes());
    }

    #[tokio::test]
    async fn copy_epg_file_atomically_replaces_existing_destination() {
        let dir = tempdir().expect("temp dir");
        let source = dir.path().join("source_epg.xml");
        let dest = dir.path().join("dest_epg.xml");
        tokio::fs::write(&source, b"fresh-epg").await.expect("write source");
        tokio::fs::write(&dest, b"stale-epg").await.expect("write stale dest");

        assert!(copy_epg_file_atomically(&source, &dest).await.expect("copy succeeds"));
        assert_eq!(tokio::fs::read(&dest).await.expect("read dest"), b"fresh-epg");
    }

    #[tokio::test]
    async fn different_ics_download_limits_download_separately() {
        let dir = tempdir().expect("temp dir");
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let ics_body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:1\r\nDTSTART:20261128T120000Z\r\nDTEND:20261128T130000Z\r\nSUMMARY:Race\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let server = RecordingEpgServer::start(ics_body).await;
        let url = format!("{}/cal.ics", server.base_url);

        let mut source_a = source(EpgSourceType::Ics, &url, Some("chan.a"));
        source_a.ics = Some(tuliprox_core::model::IcsEpgSourceConfig {
            max_download_bytes: 4096,
            ..tuliprox_core::model::IcsEpgSourceConfig::default()
        });
        let mut source_b = source(EpgSourceType::Ics, &url, Some("chan.b"));
        source_b.ics = Some(tuliprox_core::model::IcsEpgSourceConfig {
            max_download_bytes: 8192,
            ..tuliprox_core::model::IcsEpgSourceConfig::default()
        });

        let input_a = ConfigInput {
            name: "input-a".intern(),
            epg: Some(EpgConfig { sources: vec![source_a], configured_sources: None, smart_match: None }),
            ..ConfigInput::default()
        };
        let input_b = ConfigInput {
            name: "input-b".intern(),
            epg: Some(EpgConfig { sources: vec![source_b], configured_sources: None, smart_match: None }),
            ..ConfigInput::default()
        };

        let ctx = test_epg_context(dir.path());
        let (_, err_a) = get_xmltv(&ctx, &input_a, None, &storage_dir).await;
        let (_, err_b) = get_xmltv(&ctx, &input_b, None, &storage_dir).await;
        assert!(err_a.is_empty(), "{err_a:?}");
        assert!(err_b.is_empty(), "{err_b:?}");
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn same_ics_url_with_different_channels_downloads_once() {
        let dir = tempdir().expect("temp dir");
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let ics_body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:1\r\nDTSTART:20261128T120000Z\r\nDTEND:20261128T130000Z\r\nSUMMARY:Race\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let server = RecordingEpgServer::start(ics_body).await;
        let url = format!("{}/cal.ics", server.base_url);

        let mut source_a = source(EpgSourceType::Ics, &url, Some("chan.a"));
        source_a.channel_title = Some("Channel A".intern());
        source_a.ics = Some(tuliprox_core::model::IcsEpgSourceConfig {
            max_download_bytes: 4096,
            ..tuliprox_core::model::IcsEpgSourceConfig::default()
        });
        let mut source_b = source(EpgSourceType::Ics, &url, Some("chan.b"));
        source_b.channel_title = Some("Channel B".intern());
        source_b.ics = Some(tuliprox_core::model::IcsEpgSourceConfig {
            max_download_bytes: 4096,
            ..tuliprox_core::model::IcsEpgSourceConfig::default()
        });

        let input_a = ConfigInput {
            name: "input-a".intern(),
            epg: Some(EpgConfig { sources: vec![source_a], configured_sources: None, smart_match: None }),
            ..ConfigInput::default()
        };
        let input_b = ConfigInput {
            name: "input-b".intern(),
            epg: Some(EpgConfig { sources: vec![source_b], configured_sources: None, smart_match: None }),
            ..ConfigInput::default()
        };

        let ctx = test_epg_context(dir.path());
        let (guide_a, err_a) = get_xmltv(&ctx, &input_a, None, &storage_dir).await;
        let (guide_b, err_b) = get_xmltv(&ctx, &input_b, None, &storage_dir).await;
        assert!(err_a.is_empty(), "{err_a:?}");
        assert!(err_b.is_empty(), "{err_b:?}");
        assert!(guide_a.is_some() && guide_b.is_some());
        assert_eq!(server.request_count(), 1);

        let path_a = get_input_raw_epg_file_path(&input_a.epg.as_ref().unwrap().sources[0], &input_a, &storage_dir)
            .await
            .expect("path_a");
        let path_b = get_input_raw_epg_file_path(&input_b.epg.as_ref().unwrap().sources[0], &input_b, &storage_dir)
            .await
            .expect("path_b");
        assert_ne!(path_a, path_b);
        assert!(path_a.is_file());
        assert!(path_b.is_file());
        let bytes_a = tokio::fs::read(&path_a).await.expect("read path_a");
        let bytes_b = tokio::fs::read(&path_b).await.expect("read path_b");
        assert_eq!(bytes_a, ics_body.as_bytes());
        assert_eq!(bytes_b, ics_body.as_bytes());
    }

    async fn epg_temp_files_in(directory: &Path) -> Vec<PathBuf> {
        let mut temp_files = Vec::new();
        let mut entries = tokio::fs::read_dir(directory).await.expect("read directory");
        while let Some(entry) = entries.next_entry().await.expect("read directory entry") {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".tuliprox-epg-") {
                temp_files.push(entry.path());
            }
        }
        temp_files
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copy_fallback_failure_preserves_existing_destination() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().expect("temp dir");
        let source = dir.path().join("source_epg.xml");
        let dest = dir.path().join("dest_epg.xml");
        tokio::fs::write(&source, b"fresh-epg").await.expect("write source");
        tokio::fs::write(&dest, b"stale-epg").await.expect("write dest");
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o000)).expect("chmod source 000");

        let file_locks = FileLockManager::default();
        let result = TEST_HARD_LINK_FAILURE.scope(true, link_or_copy_epg_file(&file_locks, &source, &dest)).await;

        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).expect("restore source mode");
        assert!(result.is_err(), "expected copy fallback failure when source is unreadable");
        assert_eq!(tokio::fs::read(&dest).await.expect("existing dest stays readable"), b"stale-epg");
        assert_eq!(epg_temp_files_in(dir.path()).await, Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn link_or_copy_replaces_existing_destination_via_link_without_temp_files() {
        let dir = tempdir().expect("temp dir");
        let source = dir.path().join("source_epg.xml");
        let dest = dir.path().join("dest_epg.xml");
        tokio::fs::write(&source, b"fresh-epg").await.expect("write source");
        tokio::fs::write(&dest, b"stale-epg").await.expect("write stale dest");

        let file_locks = FileLockManager::default();
        assert!(link_or_copy_epg_file(&file_locks, &source, &dest).await.expect("replace via link"));
        assert_eq!(tokio::fs::read(&dest).await.expect("read dest"), b"fresh-epg");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let source_ino = tokio::fs::metadata(&source).await.expect("source meta").ino();
            let dest_ino = tokio::fs::metadata(&dest).await.expect("dest meta").ino();
            assert_eq!(dest_ino, source_ino, "dest must be hard-linked to source");
        }
        assert_eq!(epg_temp_files_in(dir.path()).await, Vec::<PathBuf>::new());

        // Replacing when dest already shares the source inode must not leave a temp hard-link behind.
        let _ = link_or_copy_epg_file(&file_locks, &source, &dest).await;
        assert_eq!(tokio::fs::read(&dest).await.expect("read dest"), b"fresh-epg");
        assert_eq!(epg_temp_files_in(dir.path()).await, Vec::<PathBuf>::new());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hit_path_link_or_copy_error_falls_back_to_download() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().expect("temp dir");
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let body = xmltv_body("fallback");
        let server = RecordingEpgServer::start(body.clone()).await;
        let url = format!("{}/guide.xml", server.base_url);

        let input_a = xmltv_input("input-a", &url);
        let input_b = xmltv_input("input-b", &url);

        let ctx = test_epg_context(dir.path());
        let (guide_a, err_a) = get_xmltv(&ctx, &input_a, None, &storage_dir).await;
        assert!(err_a.is_empty(), "{err_a:?}");
        assert!(guide_a.is_some());
        assert_eq!(server.request_count(), 1);

        let path_a = get_input_raw_epg_file_path(&input_a.epg.as_ref().unwrap().sources[0], &input_a, &storage_dir)
            .await
            .expect("path_a");
        std::fs::set_permissions(&path_a, std::fs::Permissions::from_mode(0o000)).expect("chmod path_a 000");

        let (guide_b, err_b) = TEST_HARD_LINK_FAILURE.scope(true, get_xmltv(&ctx, &input_b, None, &storage_dir)).await;

        std::fs::set_permissions(&path_a, std::fs::Permissions::from_mode(0o600)).expect("restore path_a mode");
        assert!(err_b.is_empty(), "hit-path error must fall back to download without failing input: {err_b:?}");
        assert!(guide_b.is_some());
        assert_eq!(server.request_count(), 2);

        let path_b = get_input_raw_epg_file_path(&input_b.epg.as_ref().unwrap().sources[0], &input_b, &storage_dir)
            .await
            .expect("path_b");
        assert_eq!(tokio::fs::read(&path_b).await.expect("read path_b"), body.as_bytes());
    }

    #[tokio::test]
    async fn provider_url_and_equivalent_plain_url_produce_different_download_keys() {
        let dir = tempdir().expect("temp dir");
        let ctx = test_epg_context(dir.path());
        let provider = tuliprox_core::model::ConfigProvider::from(&shared::model::ConfigProviderDto {
            name: "cdn".into(),
            urls: vec!["http://cdn.example.com".into()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });

        let mut provider_input = xmltv_input("provider-input", "provider://cdn/guide.xml");
        provider_input.provider_configs = Some(vec![Arc::new(provider)]);
        let plain_input = xmltv_input("plain-input", "http://cdn.example.com/guide.xml");

        let provider_key =
            epg_download_key(&provider_input.epg.as_ref().unwrap().sources[0], &ctx, &provider_input, None);
        let plain_key = epg_download_key(&plain_input.epg.as_ref().unwrap().sources[0], &ctx, &plain_input, None);
        assert_ne!(provider_key, plain_key);
    }

    #[tokio::test]
    async fn shared_persist_path_overwritten_by_different_headers_invalidates_earlier_download_cache() {
        let dir = tempdir().expect("temp dir");
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let body_a = xmltv_body("content-a");
        let body_b = xmltv_body("content-b");
        let server = RecordingEpgServer::start_with_responses(
            vec![
                MockEpgResponse::ok(body_a.clone()),
                MockEpgResponse::ok(body_b.clone()),
                MockEpgResponse::ok(body_a.clone()),
            ],
            body_a.clone(),
        )
        .await;
        let url = format!("{}/guide.xml", server.base_url);

        let mut input_a = xmltv_input("input-a", &url);
        input_a.persist = Some("shared.m3u".to_string());
        input_a.headers.insert("X-Token".to_string(), "alpha".to_string());

        let mut input_b = xmltv_input("input-b", &url);
        input_b.persist = Some("shared.m3u".to_string());
        input_b.headers.insert("X-Token".to_string(), "beta".to_string());

        let mut input_c = xmltv_input("input-c", &url);
        input_c.headers.insert("X-Token".to_string(), "alpha".to_string());

        let ctx = test_epg_context(dir.path());

        let source_a = &input_a.epg.as_ref().unwrap().sources[0];
        let path_a = get_input_raw_epg_file_path(source_a, &input_a, &storage_dir).await.expect("path_a");
        let source_b = &input_b.epg.as_ref().unwrap().sources[0];
        let path_b = get_input_raw_epg_file_path(source_b, &input_b, &storage_dir).await.expect("path_b");
        let source_c = &input_c.epg.as_ref().unwrap().sources[0];
        let path_c = get_input_raw_epg_file_path(source_c, &input_c, &storage_dir).await.expect("path_c");

        assert_eq!(path_a, path_b);
        assert_ne!(path_a, path_c);

        let (guide_a, err_a) = get_xmltv(&ctx, &input_a, None, &storage_dir).await;
        assert!(err_a.is_empty(), "{err_a:?}");
        assert!(guide_a.is_some());
        assert_eq!(server.request_count(), 1);
        assert_eq!(tokio::fs::read(&path_a).await.expect("read path_a"), body_a.as_bytes());

        let (guide_b, err_b) = get_xmltv(&ctx, &input_b, None, &storage_dir).await;
        assert!(err_b.is_empty(), "{err_b:?}");
        assert!(guide_b.is_some());
        assert_eq!(server.request_count(), 2);
        assert_eq!(tokio::fs::read(&path_b).await.expect("read path_b"), body_b.as_bytes());

        let (guide_c, err_c) = get_xmltv(&ctx, &input_c, None, &storage_dir).await;
        assert!(err_c.is_empty(), "{err_c:?}");
        assert!(guide_c.is_some());

        let bytes_c = tokio::fs::read(&path_c).await.expect("read path_c");
        assert_eq!(server.request_count(), 3, "Input C must trigger its own download");
        assert_eq!(bytes_c, body_a.as_bytes(), "Input C must receive content A");
        assert_ne!(bytes_c, body_b.as_bytes(), "Input C must never receive content B");
    }
}
