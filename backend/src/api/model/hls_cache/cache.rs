use super::{ProxyMapId, ProxySessionId, TransientResourceId};
use std::{
    collections::{HashMap, HashSet},
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock as StdRwLock,
    },
    time::SystemTime,
};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::{Mutex, RwLock},
    time::{timeout_at, Instant},
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

    pub fn proxy_session_id(&self) -> &ProxySessionId { &self.session_id }

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
///
/// This key is not a provider-source identity. It keys the proxy-visible object for a concrete transient resource.
/// The concrete origin fetch URI is kept in `TransientResourceRef`; callers must not reconstruct it from this key or
/// force host-neutral cache hits across redirect/CDN contexts without a separate safe resource identity.
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

/// Typed source used to distinguish a decoded-object size violation from a filesystem failure.
#[derive(Debug, thiserror::Error)]
#[error("hls cache object exceeds configured size limit {limit}")]
pub(crate) struct HlsCacheObjectLimitError {
    limit: u64,
}

impl HlsCacheObjectLimitError {
    pub(crate) fn limit(&self) -> u64 { self.limit }
}

pub(crate) fn hls_cache_object_limit_from_io(error: &io::Error) -> Option<&HlsCacheObjectLimitError> {
    let mut source: &(dyn std::error::Error + 'static) = error.get_ref()?;
    loop {
        if let Some(limit_error) = source.downcast_ref::<HlsCacheObjectLimitError>() {
            return Some(limit_error);
        }
        source = source.source()?;
    }
}

fn cache_object_limit_error(limit: u64) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, HlsCacheObjectLimitError { limit })
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
    max_object_bytes: AtomicU64,
    max_cache_bytes: AtomicU64,
    max_session_bytes: AtomicU64,
    marker_path: StdRwLock<Option<PathBuf>>,
    // serialize commits for exact budgets; shard accounting only if measured commit contention requires it.
    capacity: Mutex<CacheCapacityState>,
}

#[derive(Default)]
struct CacheCapacityState {
    cache_path: PathBuf,
    initialized: bool,
    total_bytes: u64,
    session_bytes: HashMap<String, u64>,
}

impl HlsSegmentCache {
    pub fn new() -> Self { Self::with_cache_path(DEFAULT_HLS_CACHE_PATH) }

    pub fn with_cache_path(cache_path: impl Into<PathBuf>) -> Self {
        Self {
            cache_path: StdRwLock::new(cache_path.into()),
            active_temp_files: Arc::new(RwLock::new(HashSet::new())),
            max_object_bytes: AtomicU64::new(u64::MAX),
            max_cache_bytes: AtomicU64::new(u64::MAX),
            max_session_bytes: AtomicU64::new(u64::MAX),
            marker_path: StdRwLock::new(None),
            capacity: Mutex::new(CacheCapacityState::default()),
        }
    }

    pub fn cache_path(&self) -> PathBuf { self.cache_path_snapshot() }

    pub fn update_cache_path(&self, cache_path: impl Into<PathBuf>) -> bool {
        let cache_path = cache_path.into();
        let mut current = self.cache_path.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current == cache_path {
            return false;
        }
        *current = cache_path;
        *self.marker_path.write().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        true
    }

    pub fn update_cache_limits(&self, max_cache_bytes: u64, max_session_bytes: u64) {
        let max_cache_bytes = max_cache_bytes.max(1);
        let max_session_bytes = max_session_bytes.max(1);
        self.max_cache_bytes.store(max_cache_bytes, Ordering::Release);
        self.max_session_bytes.store(max_session_bytes, Ordering::Release);
        self.max_object_bytes.store(max_cache_bytes.min(max_session_bytes), Ordering::Release);
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

    pub async fn write_temp_and_commit_with_deadline<K, R>(
        &self,
        key: &K,
        mut reader: R,
        deadline: Instant,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        self.write_temp_and_commit_inner(key, &mut reader, Some(deadline)).await
    }

    pub async fn stage_temp_with_deadline<K, R>(
        &self,
        key: &K,
        mut reader: R,
        deadline: Instant,
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
        let result = self.commit_staged_inner(key, &staged).await;
        if result.is_err() {
            let _ = fs::remove_file(&staged.path).await;
        }
        self.unregister_temp_file(&staged.path).await;
        result
    }

    async fn commit_staged_inner<K>(&self, key: &K, staged: &StagedCacheObject) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
    {
        self.ensure_cache_root_marker().await?;
        let max_object_bytes = self.max_object_bytes.load(Ordering::Acquire);
        if staged.size > max_object_bytes {
            return Err(cache_object_limit_error(max_object_bytes));
        }
        let final_path = self.path_for_key(key);
        let Some(parent) = final_path.parent() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"));
        };
        fs::create_dir_all(parent).await?;
        let session_component = key.session_path_component();
        let cache_path = self.cache_path_snapshot();
        let mut capacity = self.capacity.lock().await;
        if !capacity.initialized || capacity.cache_path != cache_path {
            let (total_bytes, session_bytes) = scan_committed_cache_usage(&cache_path).await?;
            *capacity =
                CacheCapacityState { cache_path: cache_path.clone(), initialized: true, total_bytes, session_bytes };
        }
        let old_size = match fs::metadata(&final_path).await {
            Ok(metadata) => metadata.len(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err),
        };
        let session_size = capacity.session_bytes.get(&session_component).copied().unwrap_or_default();
        let projected_total = capacity.total_bytes.saturating_sub(old_size).saturating_add(staged.size);
        let projected_session = session_size.saturating_sub(old_size).saturating_add(staged.size);
        if projected_total > self.max_cache_bytes.load(Ordering::Acquire)
            || projected_session > self.max_session_bytes.load(Ordering::Acquire)
        {
            return Err(io::Error::other("hls cache capacity exceeded"));
        }
        if self.cache_path_snapshot() != cache_path {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "hls cache path changed during object write"));
        }
        fs::rename(&staged.path, &final_path).await?;
        capacity.total_bytes = projected_total;
        capacity.session_bytes.insert(session_component, projected_session);
        drop(capacity);
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
        deadline: Option<Instant>,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        let staged = self.stage_temp_inner(key, reader, deadline).await?;
        self.commit_staged(key, staged).await
    }

    async fn stage_temp_inner<K, R>(
        &self,
        key: &K,
        reader: &mut R,
        deadline: Option<Instant>,
    ) -> io::Result<StagedCacheObject>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"));
        }
        self.ensure_cache_root_marker().await?;
        let final_path = self.path_for_key(key);
        let Some(parent) = final_path.parent() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"));
        };
        fs::create_dir_all(parent).await?;

        let (temp_path, mut temp_file) = self.create_temp_file(key).await?;
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            drop(temp_file);
            let _ = fs::remove_file(&temp_path).await;
            self.unregister_temp_file(&temp_path).await;
            return Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"));
        }
        let copy = async {
            let max_object_bytes = self.max_object_bytes.load(Ordering::Acquire);
            let mut limited = reader.take(max_object_bytes.saturating_add(1));
            let size = tokio::io::copy(&mut limited, &mut temp_file).await?;
            if size > max_object_bytes {
                return Err(cache_object_limit_error(max_object_bytes));
            }
            temp_file.flush().await?;
            drop(temp_file);
            Ok::<u64, io::Error>(size)
        };
        let copy_result = if let Some(deadline) = deadline {
            if let Ok(result) = timeout_at(deadline, copy).await {
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
        let path = self.path_for_key(key);
        let mut capacity = self.capacity.lock().await;
        let size = fs::metadata(&path).await.map_or(0, |metadata| metadata.len());
        let result = match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
        if result.is_ok() && size > 0 && capacity.initialized && capacity.cache_path == self.cache_path_snapshot() {
            capacity.total_bytes = capacity.total_bytes.saturating_sub(size);
            let session_component = key.session_path_component();
            if let Some(session_bytes) = capacity.session_bytes.get_mut(&session_component) {
                *session_bytes = session_bytes.saturating_sub(size);
            }
        }
        result
    }

    pub fn object_path<K: HlsCacheObjectKey>(&self, key: &K) -> PathBuf { self.path_for_key(key) }

    pub async fn delete_temp_files_older_than(&self, cutoff: SystemTime) -> io::Result<usize> {
        let cache_path = self.cache_path_snapshot();
        let active_temp_files = self.active_temp_files.read().await.clone();
        delete_temp_files_older_than(&cache_path, cutoff, &active_temp_files).await
    }

    pub async fn delete_session_dir(&self, proxy_session_id: &ProxySessionId) -> io::Result<()> {
        let path = self.cache_path_snapshot().join(safe_session_path_component(proxy_session_id));
        let mut capacity = self.capacity.lock().await;
        let result = match fs::remove_dir_all(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
        if result.is_ok() && capacity.initialized && capacity.cache_path == self.cache_path_snapshot() {
            let session_component = safe_session_path_component(proxy_session_id);
            if let Some(bytes) = capacity.session_bytes.remove(&session_component) {
                capacity.total_bytes = capacity.total_bytes.saturating_sub(bytes);
            }
        }
        result
    }

    pub async fn delete_orphan_session_dirs(
        &self,
        active_session_ids: &HashSet<ProxySessionId>,
        freshness_cutoff: SystemTime,
    ) -> io::Result<usize> {
        let cache_path = self.cache_path_snapshot();
        ensure_safe_cache_root(&cache_path).await?;
        let active_paths = active_session_ids
            .iter()
            .map(|id| cache_path.join(safe_session_path_component(id)))
            .collect::<HashSet<_>>();
        let active_temp_files = self.active_temp_files.write().await;
        let mut entries = fs::read_dir(&cache_path).await?;
        let mut removed = 0_usize;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !entry.file_type().await?.is_dir()
                || active_paths.contains(&path)
                || active_temp_files.iter().any(|temp| temp.starts_with(&path))
            {
                continue;
            }
            // Freshness guard: skip directories committed after the GC took its
            // in-memory session snapshot. Their owning session may not yet be
            // visible in `active_session_ids`, so deleting them would race with
            // a concurrent segment write that just created the directory.
            let metadata = match entry.metadata().await {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            if let Ok(modified) = metadata.modified() {
                if modified > freshness_cutoff {
                    continue;
                }
            }
            match fs::remove_dir_all(&path).await {
                Ok(()) => removed = removed.saturating_add(1),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        drop(active_temp_files);
        if removed > 0 {
            self.capacity.lock().await.initialized = false;
        }
        Ok(removed)
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
        ensure_safe_cache_root(&cache_path).await?;
        fs::create_dir_all(&cache_path).await?;
        let mut entries = fs::read_dir(&cache_path).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_name() == REWRITE_SECRET_FINGERPRINT_FILE || entry.file_name() == HLS_CACHE_ROOT_MARKER_FILE {
                continue;
            }
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                fs::remove_dir_all(entry.path()).await?;
            } else {
                fs::remove_file(entry.path()).await?;
            }
        }
        self.capacity.lock().await.initialized = false;
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
        self.ensure_cache_root_marker().await?;
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

    async fn ensure_cache_root_marker(&self) -> io::Result<()> {
        let cache_path = self.cache_path_snapshot();
        if self.marker_path.read().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() == Some(&cache_path) {
            return Ok(());
        }
        ensure_not_root_like_cache_path(&cache_path)?;
        fs::create_dir_all(&cache_path).await?;
        fs::write(cache_path.join(HLS_CACHE_ROOT_MARKER_FILE), b"tuliprox-hls-cache\n").await?;
        *self.marker_path.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cache_path);
        Ok(())
    }

    fn cache_path_snapshot(&self) -> PathBuf {
        self.cache_path.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

const REWRITE_SECRET_FINGERPRINT_FILE: &str = ".rewrite_secret_fingerprint";
const HLS_CACHE_ROOT_MARKER_FILE: &str = ".tuliprox-hls-cache-root";

fn safe_session_path_component(proxy_session_id: &ProxySessionId) -> String {
    let value = &proxy_session_id.0;
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
        return value.clone();
    }
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn ensure_not_root_like_cache_path(cache_path: &Path) -> io::Result<()> {
    if cache_path.as_os_str().is_empty() || cache_path.parent().is_none() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to invalidate unsafe hls cache root"));
    }
    Ok(())
}

async fn ensure_safe_cache_root(cache_path: &Path) -> io::Result<()> {
    ensure_not_root_like_cache_path(cache_path)?;
    match fs::metadata(cache_path.join(HLS_CACHE_ROOT_MARKER_FILE)).await {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to invalidate hls cache root without marker file",
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to invalidate hls cache root without marker file",
        )),
        Err(err) => Err(err),
    }
}

async fn scan_committed_cache_usage(cache_path: &Path) -> io::Result<(u64, HashMap<String, u64>)> {
    let mut total_bytes = 0_u64;
    let mut session_bytes = HashMap::new();
    let mut root_entries = match fs::read_dir(cache_path).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((0, session_bytes)),
        Err(err) => return Err(err),
    };
    while let Some(root_entry) = root_entries.next_entry().await? {
        if !root_entry.file_type().await?.is_dir() {
            continue;
        }
        let session_component = root_entry.file_name().to_string_lossy().into_owned();
        let mut bytes = 0_u64;
        let mut pending_dirs = vec![root_entry.path()];
        while let Some(dir) = pending_dirs.pop() {
            let mut entries = fs::read_dir(dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    pending_dirs.push(entry.path());
                } else if file_type.is_file() && !is_temp_cache_file(&entry.path()) {
                    bytes = bytes.saturating_add(entry.metadata().await?.len());
                }
            }
        }
        total_bytes = total_bytes.saturating_add(bytes);
        session_bytes.insert(session_component, bytes);
    }
    Ok((total_bytes, session_bytes))
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
    use super::{
        hls_cache_object_limit_from_io, CacheInvalidationOutcome, HlsSegmentCache, MapCacheKey, SegmentCacheKey,
        TransientObjectCacheKey,
    };
    use crate::api::model::{build_transient_resource_id, HlsOriginResourceFetchError, ProxySessionId};
    use std::{
        collections::HashSet,
        io,
        sync::Arc,
        time::{Duration, SystemTime},
    };
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

    #[test]
    fn transient_object_cache_key_keeps_redirect_hosts_distinct_without_leaking_urls() {
        let proxy_session_id = ProxySessionId("proxy_session".to_string());
        let first_resource =
            build_transient_resource_id("https://cdn-a.example.net/live/redirected/seg001.ts", b"secret");
        let second_resource =
            build_transient_resource_id("https://cdn-b.example.net/live/redirected/seg001.ts", b"secret");

        let first = TransientObjectCacheKey::new(proxy_session_id.clone(), first_resource, "ts");
        let second = TransientObjectCacheKey::new(proxy_session_id, second_resource, "ts");

        assert_ne!(first, second);
        assert_ne!(first.stable_value(), second.stable_value());
        for value in [first.stable_value(), second.stable_value()] {
            assert!(!value.contains("provider://"));
            assert!(!value.contains("cdn-a.example.net"));
            assert!(!value.contains("cdn-b.example.net"));
            assert!(!value.contains("/live/redirected/seg001.ts"));
        }
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
    async fn write_temp_and_commit_with_deadline_cleans_active_temp_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        let (_writer, reader) = tokio::io::duplex(64);

        let result = cache
            .write_temp_and_commit_with_deadline(&key, reader, tokio::time::Instant::now() + Duration::from_millis(1))
            .await;

        assert_eq!(result.expect_err("commit should time out").kind(), io::ErrorKind::TimedOut);
        assert!(!cache.has_active_temp_files().await);
        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
    }

    #[tokio::test]
    async fn exhausted_write_deadline_rejects_even_an_immediately_available_body() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();

        let result = cache.write_temp_and_commit_with_deadline(&key, &b"ready"[..], tokio::time::Instant::now()).await;

        assert_eq!(result.expect_err("expired deadline must time out").kind(), io::ErrorKind::TimedOut);
        assert!(!cache.has_active_temp_files().await);
        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
        assert_eq!(std::fs::read_dir(temp_dir.path()).expect("cache root reads").count(), 0);
    }

    #[tokio::test]
    async fn cache_object_write_rejects_bytes_above_the_configured_limit() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        cache.update_cache_limits(3, 3);
        let key = SegmentCacheKey::new(ProxySessionId("session".to_string()), 1, "ts");

        let result = cache.write_bytes_and_commit(&key, b"four").await;

        let error = result.expect_err("decoded object above the configured limit must fail");
        let limit_error = hls_cache_object_limit_from_io(&error).expect("typed object-limit source");
        assert_eq!(limit_error.limit(), 3);
        assert!(matches!(
            HlsOriginResourceFetchError::cache_body(&error),
            HlsOriginResourceFetchError::CacheObjectLimit { limit: 3 }
        ));
        assert!(cache.metadata(&key).await.expect("metadata").is_none());
        assert!(!cache.has_active_temp_files().await);
    }

    #[tokio::test]
    async fn orphan_session_cleanup_preserves_active_session_directories() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        cache.write_rewrite_secret_fingerprint("secret").await.expect("marker");
        let active = ProxySessionId("active".to_string());
        let orphan = ProxySessionId("orphan".to_string());
        tokio::fs::create_dir_all(temp_dir.path().join("active")).await.expect("active dir");
        tokio::fs::create_dir_all(temp_dir.path().join("orphan")).await.expect("orphan dir");
        let cutoff = SystemTime::now();

        let removed = cache.delete_orphan_session_dirs(&HashSet::from([active]), cutoff).await.expect("cleanup");

        assert_eq!(removed, 1);
        assert!(temp_dir.path().join("active").exists());
        assert!(!temp_dir.path().join(orphan.0).exists());
    }

    #[tokio::test]
    async fn orphan_session_cleanup_skips_directories_newer_than_freshness_cutoff() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        cache.write_rewrite_secret_fingerprint("secret").await.expect("marker");
        let stale = ProxySessionId("stale".to_string());
        let fresh = ProxySessionId("fresh".to_string());
        tokio::fs::create_dir_all(temp_dir.path().join(stale.0.clone())).await.expect("stale dir");
        tokio::fs::create_dir_all(temp_dir.path().join(fresh.0.clone())).await.expect("fresh dir");

        // Set the fresh dir's mtime to the future relative to the cutoff.
        let cutoff = SystemTime::now();
        let future = cutoff + std::time::Duration::from_secs(60);
        filetime::set_file_mtime(temp_dir.path().join(fresh.0.clone()), filetime::FileTime::from_system_time(future))
            .expect("set fresh mtime");

        let removed = cache.delete_orphan_session_dirs(&HashSet::new(), cutoff).await.expect("cleanup");

        assert_eq!(removed, 1, "stale orphan dir should be removed");
        assert!(!temp_dir.path().join(stale.0).exists());
        assert!(
            temp_dir.path().join(fresh.0).exists(),
            "directory newer than the freshness cutoff must be preserved to avoid racing concurrent session creation"
        );
    }

    #[tokio::test]
    async fn cache_commits_enforce_the_global_budget_across_sessions() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        cache.update_cache_limits(5, 5);
        let first = SegmentCacheKey::new(ProxySessionId("first".to_string()), 1, "ts");
        let second = SegmentCacheKey::new(ProxySessionId("second".to_string()), 1, "ts");

        assert!(cache.write_bytes_and_commit(&first, b"123").await.is_ok());
        assert!(cache.write_bytes_and_commit(&second, b"456").await.is_err());
        assert!(cache.metadata(&second).await.expect("metadata").is_none());
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
    async fn invalidate_all_refuses_unmarked_cache_root() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let unrelated_file = temp_dir.path().join("unrelated");
        tokio::fs::write(&unrelated_file, b"keep").await.expect("fixture should write");

        let err = cache.invalidate_all().await.expect_err("unmarked root should be refused");

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(unrelated_file.exists());
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
