//! `cache-image`: downloads through the plugin client into the existing
//! `LRUResourceCache` and returns a URL on the existing resource route (plan §7
//! *Image cache*).
//!
//! The cache is keyed by URL (`encode_base64_hash(url)`), not by content hash,
//! so a URL that is already cached is answered without a download. The bytes
//! are written to the path the cache itself names ([`LRUResourceCache::store_path`])
//! and then registered with `add_content`, so the resource route finds them.
//!
//! Quota accounting is per plugin and in memory only: a map from URL to the
//! bytes this plugin added. It is an approximation, on purpose:
//! - LRU evictions inside `LRUResourceCache` are not reflected, so a plugin
//!   whose images were evicted keeps counting them until the host restarts (a
//!   re-cache of the same URL replaces its entry rather than adding to it).
//! - A URL another plugin (or an earlier run) already cached costs no quota.
//! - Counts restart at zero with the process; the cache itself survives.
//!
//! The cache lock is a `tokio::sync::RwLock`. It is only ever held for the
//! in-memory bookkeeping calls, never across the download or the file write.

use crate::{http::PluginHttp, state::SealResourceUrl};
use arc_swap::ArcSwapOption;
use bytes::Bytes;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};
use tokio::sync::RwLock;
use tuliprox_core::utils::LRUResourceCache;

/// Largest image one `cache-image` call downloads.
pub(crate) const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
/// Bytes of cache one plugin may hold.
pub(crate) const IMAGE_QUOTA_BYTES: u64 = 64 * 1024 * 1024;

type SharedCache = Arc<ArcSwapOption<RwLock<LRUResourceCache>>>;

/// What every plugin's [`PluginImages`] shares: the cache, the sealer and the
/// quota accounting. Behind an `Arc` so it outlives registry reloads.
struct Inner {
    cache: SharedCache,
    seal: SealResourceUrl,
    accounting: Mutex<Accounting>,
}

/// The shared side: the application's resource cache and URL sealer.
pub(crate) struct ImageCache {
    inner: Arc<Inner>,
}

impl ImageCache {
    pub(crate) fn new(cache: SharedCache, seal: SealResourceUrl) -> Self {
        Self { inner: Arc::new(Inner { cache, seal, accounting: Mutex::new(Accounting::default()) }) }
    }
}

/// Usage of one plugin's share of the cache, for stats.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ImageUsage {
    pub(crate) used_bytes: u64,
    pub(crate) quota_bytes: u64,
}

/// Bytes each plugin added to the cache, by URL.
#[derive(Default)]
struct Accounting {
    plugins: HashMap<Arc<str>, PluginAccount>,
}

#[derive(Default)]
struct PluginAccount {
    urls: HashMap<String, u64>,
    used: u64,
}

impl Accounting {
    fn used(&self, plugin: &str) -> u64 { self.plugins.get(plugin).map_or(0, |account| account.used) }

    /// Count `size` bytes for `url` against `quota`, replacing what the same URL
    /// counted before. Check and insert are one step, so concurrent downloads
    /// cannot overshoot together.
    fn reserve(&mut self, plugin: &Arc<str>, url: &str, size: u64, quota: u64) -> Result<(), String> {
        let account = self.plugins.entry(Arc::clone(plugin)).or_default();
        let previous = account.urls.get(url).copied().unwrap_or(0);
        let used = account.used - previous + size;
        if used > quota {
            return Err(format!("image cache quota of {quota} bytes would be exceeded ({used} bytes)"));
        }
        account.urls.insert(url.to_string(), size);
        account.used = used;
        Ok(())
    }

    fn release(&mut self, plugin: &str, url: &str) {
        if let Some(account) = self.plugins.get_mut(plugin) {
            if let Some(size) = account.urls.remove(url) {
                account.used -= size;
            }
        }
    }
}

/// One plugin's view: quota accounting and the capability switch.
pub(crate) struct PluginImages {
    inner: Arc<Inner>,
    plugin: Arc<str>,
    granted: bool,
}

impl PluginImages {
    /// `granted` = the `image_cache` and `network` capabilities.
    pub(crate) fn new(cache: &ImageCache, plugin: &Arc<str>, granted: bool) -> Self {
        Self { inner: Arc::clone(&cache.inner), plugin: Arc::clone(plugin), granted }
    }

    /// The local URL for `url`, or `Err` (the plugin keeps the upstream URL)
    /// when not granted, the cache is disabled, the quota is spent or the
    /// download fails.
    pub(crate) async fn cache_image(&self, http: &PluginHttp, url: &str) -> Result<String, String> {
        if !self.granted {
            return Err("cache-image needs the image_cache and network capabilities".to_string());
        }
        let Some(cache) = self.inner.cache.load_full() else {
            return Err("the resource cache is disabled".to_string());
        };
        let sealed = (self.inner.seal)(url);
        if sealed.is_empty() {
            return Err("resource URLs cannot be sealed".to_string());
        }

        // Already cached: no download, no accounting.
        if cache.write().await.get_content(url).is_some() {
            return Ok(sealed);
        }

        let (bytes, content_type) = http.fetch(url, MAX_IMAGE_BYTES).await?;
        let mime = image_mime(content_type.as_deref())?;
        self.admit(&cache, url, &mime, bytes, IMAGE_QUOTA_BYTES).await?;
        Ok(sealed)
    }

    pub(crate) fn usage(&self) -> ImageUsage {
        ImageUsage { used_bytes: self.accounting().used(&self.plugin), quota_bytes: IMAGE_QUOTA_BYTES }
    }

    fn accounting(&self) -> MutexGuard<'_, Accounting> {
        self.inner.accounting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reserve quota, write the file where the cache expects it, then register
    /// it. The reservation is undone if either step fails.
    async fn admit(
        &self,
        cache: &RwLock<LRUResourceCache>,
        url: &str,
        mime: &str,
        bytes: Bytes,
        quota: u64,
    ) -> Result<(), String> {
        if bytes.is_empty() {
            return Err("the image is empty".to_string());
        }
        let size = bytes.len();
        self.accounting().reserve(&self.plugin, url, size as u64, quota)?;

        let path = cache.read().await.store_path(url, Some(mime));
        if let Err(err) = write_file(path.clone(), bytes).await {
            self.accounting().release(&self.plugin, url);
            return Err(err);
        }

        let mut guard = cache.write().await;
        // Another call may have cached the same URL while this one downloaded;
        // adding it again would count its bytes twice in the cache.
        if guard.get_content(url).is_none() {
            if let Err(err) = guard.add_content(url, Some(mime.to_string()), size) {
                drop(guard);
                self.accounting().release(&self.plugin, url);
                remove_file(path).await;
                return Err(format!("the image could not be registered in the cache: {err}"));
            }
        }
        Ok(())
    }
}

/// The normalised `image/*` media type of a `Content-Type` header, or `Err`.
/// SVG is refused: served from the application's own origin it can run script.
fn image_mime(content_type: Option<&str>) -> Result<String, String> {
    let essence = content_type.and_then(|value| value.split(';').next()).map(|value| value.trim().to_ascii_lowercase());
    match essence {
        Some(mime) if mime.starts_with("image/") && mime.len() > "image/".len() => {
            if mime == "image/svg+xml" {
                Err("SVG images are not cached".to_string())
            } else {
                Ok(mime)
            }
        }
        Some(mime) if !mime.is_empty() => Err(format!("the response is not an image ({mime})")),
        _ => Err("the response has no content type".to_string()),
    }
}

/// Write on the blocking pool; a crashed write leaves nothing registered, and a
/// failed one is removed.
async fn write_file(path: PathBuf, bytes: Bytes) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| format!("cache directory: {err}"))?;
        }
        std::fs::write(&path, &bytes).map_err(|err| {
            let _ = std::fs::remove_file(&path);
            format!("cache file: {err}")
        })
    })
    .await
    .map_err(|err| format!("cache write task: {err}"))?
}

async fn remove_file(path: impl AsRef<Path> + Send + 'static) {
    let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path.as_ref())).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Outbound;

    fn plugin(id: &str) -> Arc<str> { Arc::from(id) }

    fn sealer() -> SealResourceUrl { Arc::new(|url: &str| format!("/resource/{url}")) }

    fn disabled() -> ImageCache { ImageCache::new(Arc::new(ArcSwapOption::empty()), sealer()) }

    fn enabled(dir: &Path, capacity: usize) -> (ImageCache, Arc<RwLock<LRUResourceCache>>) {
        let cache = Arc::new(RwLock::new(LRUResourceCache::new(capacity, dir.to_str().unwrap())));
        let shared = Arc::new(ArcSwapOption::new(Some(Arc::clone(&cache))));
        (ImageCache::new(shared, sealer()), cache)
    }

    fn http(id: &Arc<str>, granted: bool) -> PluginHttp {
        PluginHttp::new(&Outbound::new(reqwest::Client::new()), id, granted).unwrap()
    }

    #[test]
    fn image_mime_accepts_images_and_normalises_the_header() {
        assert_eq!(image_mime(Some("image/png")).unwrap(), "image/png");
        assert_eq!(image_mime(Some(" Image/JPEG ; charset=binary")).unwrap(), "image/jpeg");
        assert!(image_mime(Some("text/html")).is_err());
        assert!(image_mime(Some("image/")).is_err());
        assert!(image_mime(Some("image/svg+xml")).is_err());
        assert!(image_mime(Some("")).is_err());
        assert!(image_mime(None).is_err());
    }

    #[test]
    fn quota_is_checked_per_plugin_and_replaces_by_url() {
        let (a, b) = (plugin("a"), plugin("b"));
        let mut accounting = Accounting::default();
        accounting.reserve(&a, "u1", 60, 100).unwrap();
        assert_eq!(accounting.used("a"), 60);
        assert!(accounting.reserve(&a, "u2", 41, 100).is_err());
        assert_eq!(accounting.used("a"), 60, "a refused reservation counts nothing");
        accounting.reserve(&a, "u2", 40, 100).unwrap();
        assert_eq!(accounting.used("a"), 100);
        // The same URL again replaces its entry instead of adding to it.
        accounting.reserve(&a, "u1", 50, 100).unwrap();
        assert_eq!(accounting.used("a"), 90);
        // Another plugin has its own quota.
        accounting.reserve(&b, "u1", 100, 100).unwrap();
        assert_eq!(accounting.used("b"), 100);
        accounting.release("a", "u1");
        accounting.release("a", "u1");
        assert_eq!(accounting.used("a"), 40);
        assert_eq!(accounting.used("unknown"), 0);
    }

    #[tokio::test]
    async fn a_plugin_without_the_capabilities_is_refused() {
        let id = plugin("p");
        let images = PluginImages::new(&disabled(), &id, false);
        let err = images.cache_image(&http(&id, true), "http://127.0.0.1:1/a.png").await.unwrap_err();
        assert!(err.contains("capabilities"), "{err}");
    }

    #[tokio::test]
    async fn a_disabled_resource_cache_is_an_error() {
        let id = plugin("p");
        let images = PluginImages::new(&disabled(), &id, true);
        let err = images.cache_image(&http(&id, true), "http://127.0.0.1:1/a.png").await.unwrap_err();
        assert!(err.contains("disabled"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_network_grant_is_denied_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let (cache, _) = enabled(dir.path(), 1024 * 1024);
        let id = plugin("p");
        let images = PluginImages::new(&cache, &id, true);
        let err = images.cache_image(&http(&id, false), "http://127.0.0.1:1/a.png").await.unwrap_err();
        assert!(err.to_ascii_lowercase().contains("denied"), "{err}");
        assert_eq!(images.usage().used_bytes, 0);
    }

    #[tokio::test]
    async fn an_already_cached_url_is_answered_without_a_download_or_quota() {
        let dir = tempfile::tempdir().unwrap();
        let (cache, shared) = enabled(dir.path(), 1024 * 1024);
        let id = plugin("p");
        let url = "http://127.0.0.1:1/cached.png";
        let path = shared.read().await.store_path(url, Some("image/png"));
        std::fs::write(&path, b"png").unwrap();
        shared.write().await.add_content(url, Some("image/png".to_string()), 3).unwrap();

        // Not granted `network`: any download attempt would fail.
        let images = PluginImages::new(&cache, &id, true);
        let local = images.cache_image(&http(&id, false), url).await.unwrap();
        assert_eq!(local, format!("/resource/{url}"));
        assert_eq!(images.usage().used_bytes, 0);
    }

    #[tokio::test]
    async fn admit_writes_the_file_registers_it_and_counts_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let (cache, shared) = enabled(dir.path(), 1024 * 1024);
        let id = plugin("p");
        let images = PluginImages::new(&cache, &id, true);
        let url = "http://127.0.0.1:1/new.png";

        images.admit(&shared, url, "image/png", Bytes::from_static(b"12345"), 10).await.unwrap();
        let (path, mime) = shared.write().await.get_content(url).expect("registered");
        assert_eq!(mime.as_deref(), Some("image/png"));
        assert_eq!(std::fs::read(path).unwrap(), b"12345");
        assert_eq!(images.usage().used_bytes, 5);

        // The same URL again neither double counts nor double registers.
        images.admit(&shared, url, "image/png", Bytes::from_static(b"12345"), 10).await.unwrap();
        assert_eq!(images.usage().used_bytes, 5);

        // The quota refuses the next image and leaves no trace of it.
        let other = "http://127.0.0.1:1/other.png";
        let err = images.admit(&shared, other, "image/png", Bytes::from_static(b"123456"), 10).await.unwrap_err();
        assert!(err.contains("quota"), "{err}");
        assert_eq!(images.usage().used_bytes, 5);
        assert!(shared.write().await.get_content(other).is_none());
        assert!(images.admit(&shared, other, "image/png", Bytes::new(), 10).await.is_err());
    }
}
