use crate::{parser::xmltv::TVGuide, processor::PlaylistProcessingContext};
use log::{debug, warn};
use shared::{
    concat_string,
    error::TuliproxError,
    model::EventSink,
    utils::{sanitize_sensitive_info, short_hash},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use tuliprox_core::{
    model::{ConfigInput, EpgSource, EpgSourceType, PersistedEpgSource, PersistedEpgSourceKind},
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

async fn download_epg_file<E: EventSink + Clone + 'static>(
    epg_source: &EpgSource,
    ctx: &PlaylistProcessingContext<E>,
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

    let lock_key: std::sync::Arc<str> = persist_file_path.display().to_string().into();
    let _input_lock = ctx.get_input_lock(&lock_key).await;

    if ctx.is_input_downloaded(&lock_key).await {
        let _cache_lock = ctx.config.file_locks.read_lock(&persist_file_path).await;
        if tokio::fs::metadata(&persist_file_path).await.is_ok_and(|metadata| metadata.is_file()) {
            return Ok(persist_file_path);
        }
    }

    debug!("Downloading epg for input '{}'", input.name);
    let max_bytes = epg_download_limit(epg_source);
    match request::get_input_epg_content_as_file(
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
    .await
    {
        Ok(path) => {
            ctx.mark_input_downloaded(lock_key.clone()).await;
            Ok(path)
        }
        Err(err) => Err(err),
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

pub async fn get_xmltv<E: EventSink + Clone + 'static>(
    ctx: &PlaylistProcessingContext<E>,
    input: &ConfigInput,
    headers: Option<&reqwest::header::HeaderMap>,
    storage_dir: &str,
) -> (Option<TVGuide>, Vec<TuliproxError>) {
    match &input.epg {
        None => (None, vec![]),
        Some(epg_config) => {
            let mut errors = vec![];
            let mut file_paths = vec![];
            let mut stored_file_paths = vec![];

            for epg_source in &epg_config.sources {
                match download_epg_file(epg_source, ctx, input, headers, storage_dir).await {
                    Ok(file_path) => {
                        stored_file_paths.push(file_path.clone());
                        match persisted_source_from_config(epg_source, file_path) {
                            Ok(persisted) => file_paths.push(persisted),
                            Err(err) => errors.push(err),
                        }
                    }
                    Err(err) => {
                        errors.push(err);
                    }
                }
            }

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

fn persisted_source_from_config(
    epg_source: &EpgSource,
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

    Ok(PersistedEpgSource { file_path, priority: epg_source.priority, logo_override: epg_source.logo_override, kind })
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
}
