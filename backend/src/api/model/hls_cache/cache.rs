use super::{ProxyMapId, ProxySessionId, TransientResourceId};
use std::{
    collections::HashSet,
    fmt, io,
    path::{Path, PathBuf},
    sync::{Arc, RwLock as StdRwLock},
    time::{Duration, SystemTime},
};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncRead, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::RwLock,
    time::timeout,
};

pub const DEFAULT_HLS_CACHE_PATH: &str = "/tmp/tuliprox/cache/hls";
pub const DEFAULT_HLS_CACHE_DURATION_SECS: u64 = 300;
const TEMP_CREATE_ATTEMPTS: usize = 8;

/// Stable cache key for one proxy-visible HLS segment.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct SegmentCacheKey {
    session_id: ProxySessionId,
    seq: u64,
    file_ext: String,
}

impl SegmentCacheKey {
    pub fn new(proxy_session_id: ProxySessionId, proxy_seq: u64, proxy_file_ext: impl Into<String>) -> Self {
        Self { session_id: proxy_session_id, seq: proxy_seq, file_ext: proxy_file_ext.into() }
    }

    pub fn stable_value(&self) -> String { format!("hls:{}:{:020}", self.session_id.0, self.seq) }

    pub fn proxy_seq(&self) -> u64 { self.seq }
}

impl fmt::Debug for SegmentCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentCacheKey")
            .field("session_id", &"<redacted>")
            .field("seq", &self.seq)
            .field("file_ext", &self.file_ext)
            .finish()
    }
}

/// Stable cache key for one proxy-visible HLS EXT-X-MAP object.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct MapCacheKey {
    session_id: ProxySessionId,
    map_id: ProxyMapId,
    file_ext: String,
}

impl MapCacheKey {
    pub fn new(
        proxy_session_id: ProxySessionId,
        proxy_map_id: impl Into<ProxyMapId>,
        proxy_file_ext: impl Into<String>,
    ) -> Self {
        Self { session_id: proxy_session_id, map_id: proxy_map_id.into(), file_ext: proxy_file_ext.into() }
    }

    pub fn stable_value(&self) -> String { format!("hls-map:{}:{:020}", self.session_id.0, self.map_id.0) }

    pub fn proxy_map_id(&self) -> ProxyMapId { self.map_id }
}

impl fmt::Debug for MapCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapCacheKey")
            .field("session_id", &"<redacted>")
            .field("map_id", &self.map_id.0)
            .field("file_ext", &self.file_ext)
            .finish()
    }
}

/// Stable cache key for one demand-cached transient passthrough object.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct TransientObjectCacheKey {
    session_id: ProxySessionId,
    resource_id: TransientResourceId,
    file_ext: String,
}

impl TransientObjectCacheKey {
    pub fn new(
        proxy_session_id: ProxySessionId,
        transient_resource_id: TransientResourceId,
        proxy_file_ext: impl Into<String>,
    ) -> Self {
        Self { session_id: proxy_session_id, resource_id: transient_resource_id, file_ext: proxy_file_ext.into() }
    }

    pub fn stable_value(&self) -> String { format!("hls-transient:{}:{}", self.session_id.0, self.resource_id.0) }

    pub fn transient_resource_id(&self) -> &TransientResourceId { &self.resource_id }
}

impl fmt::Debug for TransientObjectCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientObjectCacheKey")
            .field("session_id", &"<redacted>")
            .field("resource_id", &self.resource_id)
            .field("file_ext", &self.file_ext)
            .finish()
    }
}

/// Resolves a proxy cache object to a safe path below the configured HLS cache root.
pub trait HlsCacheObjectKey {
    fn session_path_component(&self) -> String;
    fn file_name(&self) -> String;
}

impl HlsCacheObjectKey for SegmentCacheKey {
    fn session_path_component(&self) -> String {
        let value = &self.session_id.0;
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
            return value.clone();
        }
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    fn file_name(&self) -> String { format!("{:06}.{}", self.seq, self.file_ext) }
}

impl HlsCacheObjectKey for MapCacheKey {
    fn session_path_component(&self) -> String {
        let value = &self.session_id.0;
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
            return value.clone();
        }
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    fn file_name(&self) -> String { format!("map/{:06}.{}", self.map_id.0, self.file_ext) }
}

impl HlsCacheObjectKey for TransientObjectCacheKey {
    fn session_path_component(&self) -> String {
        let value = &self.session_id.0;
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
            return value.clone();
        }
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    fn file_name(&self) -> String { format!("r/{}.{}", self.resource_id.0, self.file_ext) }
}

/// Filesystem metadata for a committed HLS cache object.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CachedSegmentMetadata {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StagedCacheObject {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CacheInvalidationOutcome {
    Invalidated,
    DeferredActiveTempFiles,
}

/// File-backed cache for committed HLS segment objects.
pub struct HlsSegmentCache {
    cache_path: StdRwLock<PathBuf>,
    active_temp_files: Arc<RwLock<HashSet<PathBuf>>>,
}

impl HlsSegmentCache {
    pub fn new() -> Self { Self::with_cache_path(DEFAULT_HLS_CACHE_PATH) }

    pub fn with_cache_path(cache_path: impl Into<PathBuf>) -> Self {
        Self { cache_path: StdRwLock::new(cache_path.into()), active_temp_files: Arc::new(RwLock::new(HashSet::new())) }
    }

    pub fn cache_path(&self) -> PathBuf { self.cache_path_snapshot() }

    pub fn update_cache_path(&self, cache_path: impl Into<PathBuf>) -> bool {
        let cache_path = cache_path.into();
        let mut current = self.cache_path.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current == cache_path {
            return false;
        }
        *current = cache_path;
        true
    }

    pub async fn metadata<K: HlsCacheObjectKey>(&self, key: &K) -> io::Result<Option<CachedSegmentMetadata>> {
        let path = self.path_for_key(key);
        match fs::metadata(&path).await {
            Ok(metadata) => Ok(Some(CachedSegmentMetadata { path, size: metadata.len() })),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn open_range<K: HlsCacheObjectKey>(&self, key: &K, start: u64) -> io::Result<File> {
        let mut file = File::open(self.path_for_key(key)).await?;
        file.seek(SeekFrom::Start(start)).await?;
        Ok(file)
    }

    pub async fn write_temp_and_commit<K, R>(&self, key: &K, mut reader: R) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        self.write_temp_and_commit_inner(key, &mut reader, None).await
    }

    pub async fn write_temp_and_commit_with_timeout<K, R>(
        &self,
        key: &K,
        mut reader: R,
        deadline: Duration,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        self.write_temp_and_commit_inner(key, &mut reader, Some(deadline)).await
    }

    pub async fn stage_temp_with_timeout<K, R>(
        &self,
        key: &K,
        mut reader: R,
        deadline: Duration,
    ) -> io::Result<StagedCacheObject>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        self.stage_temp_inner(key, &mut reader, Some(deadline)).await
    }

    pub async fn commit_staged<K>(&self, key: &K, staged: StagedCacheObject) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
    {
        let final_path = self.path_for_key(key);
        let Some(parent) = final_path.parent() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"));
        };
        fs::create_dir_all(parent).await?;
        match fs::rename(&staged.path, &final_path).await {
            Ok(()) => {}
            Err(err) => {
                let _ = fs::remove_file(&staged.path).await;
                self.unregister_temp_file(&staged.path).await;
                return Err(err);
            }
        }
        self.unregister_temp_file(&staged.path).await;
        sync_parent_directory(parent).await;
        self.metadata(key).await?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "committed segment cache file is missing after atomic rename")
        })
    }

    pub async fn remove_staged(&self, staged: StagedCacheObject) -> io::Result<()> {
        let result = match fs::remove_file(&staged.path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
        self.unregister_temp_file(&staged.path).await;
        result
    }

    async fn write_temp_and_commit_inner<K, R>(
        &self,
        key: &K,
        reader: &mut R,
        deadline: Option<Duration>,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        let final_path = self.path_for_key(key);
        let Some(parent) = final_path.parent() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"));
        };
        fs::create_dir_all(parent).await?;

        let (temp_path, mut temp_file) = self.create_temp_file(key).await?;
        let commit = async {
            tokio::io::copy(reader, &mut temp_file).await?;
            temp_file.flush().await?;
            temp_file.sync_all().await?;
            drop(temp_file);

            match fs::rename(&temp_path, &final_path).await {
                Ok(()) => Ok(()),
                Err(err) => {
                    let _ = fs::remove_file(&temp_path).await;
                    Err(err)
                }
            }
        };
        let commit_result = if let Some(deadline) = deadline {
            if let Ok(result) = timeout(deadline, commit).await {
                result
            } else {
                let _ = fs::remove_file(&temp_path).await;
                Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"))
            }
        } else {
            commit.await
        };
        self.unregister_temp_file(&temp_path).await;
        commit_result?;
        sync_parent_directory(parent).await;

        self.metadata(key).await?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "committed segment cache file is missing after atomic rename")
        })
    }

    async fn stage_temp_inner<K, R>(
        &self,
        key: &K,
        reader: &mut R,
        deadline: Option<Duration>,
    ) -> io::Result<StagedCacheObject>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        let final_path = self.path_for_key(key);
        let Some(parent) = final_path.parent() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"));
        };
        fs::create_dir_all(parent).await?;

        let (temp_path, mut temp_file) = self.create_temp_file(key).await?;
        let copy = async {
            let size = tokio::io::copy(reader, &mut temp_file).await?;
            temp_file.flush().await?;
            temp_file.sync_all().await?;
            drop(temp_file);
            Ok::<u64, io::Error>(size)
        };
        let copy_result = if let Some(deadline) = deadline {
            if let Ok(result) = timeout(deadline, copy).await {
                result
            } else {
                let _ = fs::remove_file(&temp_path).await;
                self.unregister_temp_file(&temp_path).await;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"));
            }
        } else {
            copy.await
        };
        match copy_result {
            Ok(size) => Ok(StagedCacheObject { path: temp_path, size }),
            Err(err) => {
                let _ = fs::remove_file(&temp_path).await;
                self.unregister_temp_file(&temp_path).await;
                Err(err)
            }
        }
    }

    pub async fn write_bytes_and_commit<K>(&self, key: &K, bytes: &[u8]) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
    {
        self.write_temp_and_commit(key, bytes).await
    }

    pub async fn delete<K: HlsCacheObjectKey>(&self, key: &K) -> io::Result<()> {
        match fs::remove_file(self.path_for_key(key)).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub fn object_path<K: HlsCacheObjectKey>(&self, key: &K) -> PathBuf { self.path_for_key(key) }

    pub async fn delete_temp_files_older_than(&self, cutoff: SystemTime) -> io::Result<usize> {
        let cache_path = self.cache_path_snapshot();
        let active_temp_files = self.active_temp_files.read().await.clone();
        delete_temp_files_older_than(&cache_path, cutoff, &active_temp_files).await
    }

    pub async fn delete_session_dir(&self, proxy_session_id: &ProxySessionId) -> io::Result<()> {
        let path = self.cache_path_snapshot().join(safe_session_path_component(proxy_session_id));
        match fs::remove_dir_all(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub async fn has_active_temp_files_for_session(&self, proxy_session_id: &ProxySessionId) -> bool {
        let session_path = self.cache_path_snapshot().join(safe_session_path_component(proxy_session_id));
        self.active_temp_files.read().await.iter().any(|path| path.starts_with(&session_path))
    }

    pub async fn has_active_temp_files(&self) -> bool { !self.active_temp_files.read().await.is_empty() }

    pub async fn invalidate_all_if_no_active_temp_files(&self) -> io::Result<CacheInvalidationOutcome> {
        let active_temp_files = self.active_temp_files.write().await;
        if !active_temp_files.is_empty() {
            return Ok(CacheInvalidationOutcome::DeferredActiveTempFiles);
        }
        self.invalidate_all_unchecked().await?;
        Ok(CacheInvalidationOutcome::Invalidated)
    }

    pub async fn invalidate_all(&self) -> io::Result<()> { self.invalidate_all_unchecked().await }

    async fn invalidate_all_unchecked(&self) -> io::Result<()> {
        let cache_path = self.cache_path_snapshot();
        ensure_safe_cache_root(&cache_path)?;
        fs::create_dir_all(&cache_path).await?;
        let mut entries = fs::read_dir(&cache_path).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_name() == REWRITE_SECRET_FINGERPRINT_FILE {
                continue;
            }
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                fs::remove_dir_all(entry.path()).await?;
            } else {
                fs::remove_file(entry.path()).await?;
            }
        }
        Ok(())
    }

    pub async fn read_rewrite_secret_fingerprint(&self) -> io::Result<Option<String>> {
        match fs::read_to_string(self.rewrite_secret_fingerprint_path()).await {
            Ok(value) => Ok(Some(value.trim().to_string())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn write_rewrite_secret_fingerprint(&self, fingerprint: &str) -> io::Result<()> {
        fs::create_dir_all(self.cache_path_snapshot()).await?;
        fs::write(self.rewrite_secret_fingerprint_path(), fingerprint).await
    }

    fn path_for_key<K: HlsCacheObjectKey>(&self, key: &K) -> PathBuf {
        self.cache_path_snapshot().join(key.session_path_component()).join(key.file_name())
    }

    async fn create_temp_file<K: HlsCacheObjectKey>(&self, key: &K) -> io::Result<(PathBuf, File)> {
        let mut active_temp_files = self.active_temp_files.write().await;
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let temp_path = self.temp_path_for_key(key);
            match OpenOptions::new().write(true).create_new(true).open(&temp_path).await {
                Ok(file) => {
                    active_temp_files.insert(temp_path.clone());
                    return Ok((temp_path, file));
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not create unique hls cache temp file"))
    }

    fn temp_path_for_key<K: HlsCacheObjectKey>(&self, key: &K) -> PathBuf {
        let suffix = fastrand::u64(..);
        self.cache_path_snapshot()
            .join(key.session_path_component())
            .join(format!("{}.tmp.{suffix:016x}", key.file_name()))
    }

    async fn unregister_temp_file(&self, temp_path: &Path) { self.active_temp_files.write().await.remove(temp_path); }

    fn rewrite_secret_fingerprint_path(&self) -> PathBuf {
        self.cache_path_snapshot().join(REWRITE_SECRET_FINGERPRINT_FILE)
    }

    fn cache_path_snapshot(&self) -> PathBuf {
        self.cache_path.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

async fn sync_parent_directory(parent: &Path) {
    if let Ok(parent_dir) = File::open(parent).await {
        let _ = parent_dir.sync_all().await;
    }
}

const REWRITE_SECRET_FINGERPRINT_FILE: &str = ".rewrite_secret_fingerprint";

fn safe_session_path_component(proxy_session_id: &ProxySessionId) -> String {
    let value = &proxy_session_id.0;
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
        return value.clone();
    }
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn ensure_safe_cache_root(cache_path: &Path) -> io::Result<()> {
    if cache_path.as_os_str().is_empty() || cache_path.parent().is_none() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to invalidate unsafe hls cache root"));
    }
    Ok(())
}

async fn delete_temp_files_older_than(
    root: &Path,
    cutoff: SystemTime,
    active_temp_files: &HashSet<PathBuf>,
) -> io::Result<usize> {
    let mut deleted = 0_usize;
    let mut pending_dirs = vec![root.to_path_buf()];
    while let Some(dir) = pending_dirs.pop() {
        let mut entries = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                pending_dirs.push(path);
                continue;
            }
            if !is_temp_cache_file(&path) || active_temp_files.contains(&path) {
                continue;
            }
            let metadata = entry.metadata().await?;
            if metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH) >= cutoff {
                continue;
            }
            match fs::remove_file(&path).await {
                Ok(()) => deleted = deleted.saturating_add(1),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(deleted)
}

fn is_temp_cache_file(path: &Path) -> bool {
    path.file_name().and_then(|file_name| file_name.to_str()).is_some_and(|file_name| file_name.contains(".tmp."))
}

impl Default for HlsSegmentCache {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::{CacheInvalidationOutcome, HlsSegmentCache, MapCacheKey, SegmentCacheKey};
    use crate::api::model::ProxySessionId;
    use std::{io, sync::Arc, time::Duration};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn cache_key() -> SegmentCacheKey { SegmentCacheKey::new(ProxySessionId("proxy_session".to_string()), 123, "ts") }

    #[test]
    fn segment_cache_key_contains_no_origin_data() {
        let key = cache_key();

        assert_eq!(key.stable_value(), "hls:proxy_session:00000000000000000123");
    }

    #[test]
    fn cache_key_debug_redacts_proxy_session_id() {
        let key = SegmentCacheKey::new(ProxySessionId("secretToken".to_string()), 123, "ts");
        let debug = format!("{key:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secretToken"));
        assert!(!debug.contains(&key.stable_value()));
    }

    #[tokio::test]
    async fn write_temp_and_commit_creates_final_segment_cache_file_with_proxy_layout() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();

        let metadata = cache.write_bytes_and_commit(&key, b"segment-body").await.expect("commit should succeed");

        assert_eq!(metadata.size, 12);
        assert!(metadata.path.exists());
        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), Some(metadata.clone()));
        assert!(metadata.path.ends_with("proxy_session/000123.ts"));
    }

    #[tokio::test]
    async fn write_temp_and_commit_creates_final_map_cache_file_with_proxy_layout() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = MapCacheKey::new(ProxySessionId("proxy_session".to_string()), 0, "mp4");

        let metadata = cache.write_bytes_and_commit(&key, b"map-body").await.expect("commit should succeed");

        assert_eq!(metadata.size, 8);
        assert!(metadata.path.ends_with("proxy_session/map/000000.mp4"));
        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), Some(metadata));
    }

    #[tokio::test]
    async fn write_temp_and_commit_with_timeout_cleans_active_temp_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        let (_writer, reader) = tokio::io::duplex(64);

        let result = cache.write_temp_and_commit_with_timeout(&key, reader, Duration::from_millis(1)).await;

        assert_eq!(result.expect_err("commit should time out").kind(), io::ErrorKind::TimedOut);
        assert!(!cache.has_active_temp_files().await);
        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
    }

    #[tokio::test]
    async fn open_range_reads_from_requested_offset() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        cache.write_bytes_and_commit(&key, b"0123456789").await.expect("commit should succeed");

        let file = cache.open_range(&key, 4).await.expect("range should open");
        let mut body = Vec::new();
        file.take(3).read_to_end(&mut body).await.expect("range should read");

        assert_eq!(body, b"456");
    }

    #[tokio::test]
    async fn temp_file_collision_does_not_overwrite_existing_temp_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        let parent = temp_dir.path().join("proxy_session");
        tokio::fs::create_dir_all(&parent).await.expect("parent should be created");
        let existing_temp = parent.join("000123.ts.tmp.0000000000000000");
        tokio::fs::write(&existing_temp, b"existing").await.expect("temp fixture should write");

        cache.write_bytes_and_commit(&key, b"segment-body").await.expect("commit should succeed");

        assert_eq!(tokio::fs::read(&existing_temp).await.expect("existing temp should remain"), b"existing");
    }

    #[tokio::test]
    async fn invalidate_all_if_no_active_temp_files_deletes_only_when_no_temp_write_is_active() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let committed_key = cache_key();
        cache.write_bytes_and_commit(&committed_key, b"segment-body").await.expect("commit should succeed");

        let outcome = cache.invalidate_all_if_no_active_temp_files().await.expect("invalidation should succeed");

        assert_eq!(outcome, CacheInvalidationOutcome::Invalidated);
        assert_eq!(cache.metadata(&committed_key).await.expect("metadata should read"), None);

        cache.write_bytes_and_commit(&committed_key, b"segment-body").await.expect("second commit should succeed");
        let active_key = SegmentCacheKey::new(ProxySessionId("proxy_session".to_string()), 124, "ts");
        let (mut writer, reader) = tokio::io::duplex(64);
        let task_cache = Arc::clone(&cache);
        let task_key = active_key.clone();
        let write_task = tokio::spawn(async move { task_cache.write_temp_and_commit(&task_key, reader).await });
        for _ in 0..50 {
            if cache.has_active_temp_files().await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(cache.has_active_temp_files().await);

        let outcome =
            cache.invalidate_all_if_no_active_temp_files().await.expect("deferred invalidation should succeed");

        assert_eq!(outcome, CacheInvalidationOutcome::DeferredActiveTempFiles);
        assert!(cache.metadata(&committed_key).await.expect("metadata should read").is_some());
        writer.write_all(b"done").await.expect("write temp body");
        drop(writer);
        write_task.await.expect("temp write task joins").expect("temp write commits");
    }

    #[tokio::test]
    async fn delete_removes_committed_file_idempotently() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        cache.write_bytes_and_commit(&key, b"segment-body").await.expect("commit should succeed");

        cache.delete(&key).await.expect("delete should succeed");
        cache.delete(&key).await.expect("second delete should be idempotent");

        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
    }
}
