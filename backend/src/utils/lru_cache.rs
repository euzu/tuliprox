use crate::utils::{decode_base64_string, encode_base64_hash, encode_base64_string, traverse_dir};
use shared::utils::{human_readable_byte_size, sanitize_sensitive_info};
use log::{debug, error, info, trace};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use crate::utils::{trace_if_enabled};

const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

#[inline]
fn encode_cache_key(key: &str) -> String {
    encode_base64_hash(key)
}

/// `LRUResourceCache`
///
/// A least-recently-used (LRU) file-based resource cache that stores files in a directory on disk,
/// automatically managing their lifecycle based on a specified maximum cache size. The cache evicts
/// the least recently used files when the size limit is exceeded.
///
/// # Implementation note — O(1) LRU with lazy-deletion
///
/// Instead of an O(n) `VecDeque::position()` scan on every cache hit, we use a **lazy-deletion**
/// strategy:
/// - Each entry in `cache` stores a monotonic `generation` counter alongside its data.
/// - `usage_order` is a `VecDeque<(key, generation)>`. Promoting an entry to MRU costs O(1):
///   bump the generation in `cache` and push a new `(key, new_gen)` tuple to the back.
/// - The old tuple(s) in `usage_order` that carry a stale generation are left in place and
///   discarded during the next eviction sweep — hence "lazy deletion".
/// - Amortised cost per operation is O(1); worst-case eviction is proportional to the number
///   of stale entries that accumulated, which is bounded by total access count.
///
/// # Fields
/// - `capacity`: The maximum cache size in bytes.
/// - `cache_dir`: The directory where cached files are stored.
/// - `current_size`: The current total size of all files in the cache, in bytes.
/// - `cache`: Maps a unique key to `(path, mime_type, size_bytes, generation)`.
/// - `usage_order`: LRU deque of `(key, generation)` pairs; stale entries are skipped on eviction.
/// - `next_generation`: Monotonic counter for assigning unique generations.
pub struct LRUResourceCache {
    capacity: usize,  // Maximum size in bytes
    cache_dir: PathBuf,
    current_size: usize,  // Current size in bytes
    // (`path`, `mime_type`, `size_bytes`, `generation`)
    cache: HashMap<String, (PathBuf, Option<String>, usize, u64)>,
    // Lazy-deletion LRU queue: (`cache_key`, `generation_at_enqueue`)
    usage_order: VecDeque<(String, u64)>,
    next_generation: u64,
}

impl LRUResourceCache {
    ///   - Creates a new `LRUResourceCache` instance.
    ///   - Arguments:
    ///     - `capacity`: The maximum size of the cache in bytes.
    ///     - `cache_dir`: The directory path where cached files are stored.
    ///
    pub fn new(capacity: usize, cache_dir: &str) -> Self {
        // Estimate: assume average file size of 256KB
        let estimated_entries = (capacity / (256 * 1024)).clamp(64, 16384);
        let cache_dir = PathBuf::from(cache_dir);
        let cache_dir_clone = cache_dir.clone();
        tokio::task::spawn(async move {
            if tokio::fs::create_dir_all(&cache_dir_clone).await.is_ok() {
                let tag_path = cache_dir_clone.join(CACHEDIR_TAG);
                if !tag_path.exists() {
                    let _ = tokio::fs::write(
                        &tag_path,
                        "Signature: 8a477f597d28d172789f06886806bc55\n# This file marks the directory as a cache directory for backup programs.\n",
                    ).await;
                }
            }
        });
        Self {
            capacity,
            cache_dir,
            current_size: 0,
            cache: HashMap::<String, (PathBuf, Option<String>, usize, u64)>::with_capacity(estimated_entries),
            usage_order: VecDeque::new(),
            next_generation: 0,
        }
    }

    pub fn update_config(&mut self, capacity: usize, cache_dir: &str) {
        self.capacity = capacity;
        self.cache_dir = PathBuf::from(cache_dir);
    }

    /// - Scans the cache directory and populates the internal data structures with existing files and their sizes.
    /// - Updates the `current_size` and `usage_order` fields based on the scanned files.
    ///   The use/access order is not restored!!!
    pub fn scan(&mut self) -> std::io::Result<()> {
        let mut visit = |entry: &std::fs::DirEntry, metadata: &std::fs::Metadata| {
            let path = entry.path();
            if let Some(os_file_name) = path.file_name() {
                let file_name = String::from(os_file_name.to_string_lossy());
                if file_name == CACHEDIR_TAG {
                    return;
                }
                let (key, mime_type) = if let Some((part1, part2)) = file_name.split_once('.') {
                    (part1.to_string(), String::from_utf8(decode_base64_string(part2)).ok())
                } else {
                    (file_name.clone(), None)
                };

                let file_size = usize::try_from(metadata.len()).unwrap_or(0);
                // we need to duplicate because of closure we can't call insert_to_cache
                {  // insert_to_cache
                    let mut path = self.cache_dir.clone();
                    path.push(&file_name);
                    trace!("Added file to cache: {}", &path.to_string_lossy());
                    let gen = self.next_generation;
                    self.next_generation = self.next_generation.wrapping_add(1);
                    self.cache.insert(key.clone(), (path.clone(), mime_type, file_size, gen));
                    self.usage_order.push_back((key, gen));
                    self.current_size += file_size;
                }
            }
        };
        let result = traverse_dir(&self.cache_dir, &mut visit);
        info!("Cache scanned, current size {}", self.get_size_text());
        result
    }

    pub fn get_size_text(&self) -> String {
        format!("{} / {}", human_readable_byte_size(self.current_size as u64), human_readable_byte_size(self.capacity as u64))
    }

    ///   - Adds a new file to the cache.
    ///   - Evicts the least recently used files if the cache size exceeds the capacity after the addition.
    ///   - Arguments:
    ///     - `url`: The unique identifier for the file.
    ///     - `file_size`: The size of the file in bytes.
    ///   - Returns:
    ///     - The `PathBuf` where the file is stored.
    pub fn add_content(&mut self, url: &str, mime_type: Option<String>, file_size: usize) -> std::io::Result<PathBuf> {
        let key = encode_cache_key(url);
        let path = self.insert_to_cache(key, mime_type, file_size);
        if self.current_size > self.capacity {
            self.evict_if_needed();
        }
        Ok(path)
    }

    fn insert_to_cache(&mut self, key: String, mime_type: Option<String>, file_size: usize) -> PathBuf {
        let path = self.get_store_path(&key, mime_type.as_deref());
        debug!("Added file to cache: {}", &path.to_string_lossy());
        let gen = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        self.cache.insert(key.clone(), (path.clone(), mime_type, file_size, gen));
        self.usage_order.push_back((key, gen));
        self.current_size += file_size;
        path
    }

    pub fn store_path(&self, url: &str, mime_type: Option<&str>) -> PathBuf {
        self.get_store_path(&encode_cache_key(url), mime_type)
    }

    fn get_store_path(&self, cache_key: &str, mime_type: Option<&str>) -> PathBuf {
        let key = if let Some(mime) = mime_type {
            format!("{cache_key}.{}", encode_base64_string(mime.as_bytes()))
        } else {
            cache_key.to_string()
        };
        let mut path = self.cache_dir.clone();
        path.push(&key);
        path
    }

    ///   - Retrieves a file from the cache if it exists.
    ///   - Moves the file's key to the end of the usage queue to mark it as recently used (O(1)).
    ///   - Arguments:
    ///     - `url`: The unique identifier for the file.
    ///   - Returns:
    ///     - The `PathBuf` of the file if it exists; `None` otherwise.
    pub fn get_content(&mut self, url: &str) -> Option<(PathBuf, Option<String>)> {
        let key = encode_cache_key(url);
        if let Some((path, mime_type, size, gen)) = self.cache.get_mut(&key) {
            if path.exists() {
                trace_if_enabled!("Responding resource from cache with key: {key} for url: {}", sanitize_sensitive_info(url));
                // O(1) MRU promotion: bump the generation and push a new entry to the back.
                // The old tuple in `usage_order` with the previous generation becomes stale
                // and will be lazily discarded during the next eviction sweep.
                let new_gen = self.next_generation;
                self.next_generation = self.next_generation.wrapping_add(1);
                *gen = new_gen;
                self.usage_order.push_back((key, new_gen));
                return Some((path.clone(), mime_type.clone()));
            }
            trace_if_enabled!("Cache inconsistency: file missing for key: {key}, url: {}", sanitize_sensitive_info(url));
            // this should not happen, someone deleted the file manually and the cache is not in sync
            self.current_size -= *size;
            self.cache.remove(&key);
        }
        None
    }

    fn evict_if_needed(&mut self) {
        // if the cache size is too small and one element exceeds the size then the cache won't work, we ignore this
        while self.current_size > self.capacity {
            // Pop from the front; skip stale entries (lazy-deletion).
            let Some((oldest_key, queued_gen)) = self.usage_order.pop_front() else {
                break;
            };

            // Check whether this queue entry is still the live generation.
            let live_gen = self.cache.get(&oldest_key).map(|e| e.3);
            if live_gen != Some(queued_gen) {
                // Stale entry — the key was promoted to a newer generation; skip it.
                continue;
            }

            if let Some((file, _mime_type, size, _gen)) = self.cache.remove(&oldest_key) {
                self.current_size -= size;
                if let Err(err) = fs::remove_file(&file) {
                    error!("Failed to delete cached file {} {err}", file.to_string_lossy());
                } else {
                    debug!("Removed file from cache: {}", file.to_string_lossy());
                }
            }
        }
    }
}
