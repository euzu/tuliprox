use std::sync::{Arc, LazyLock};
use dashmap::DashMap;
use regex::Regex;
use crate::error::TuliproxError;
use crate::info_err;

pub static REGEX_CACHE: LazyLock<RegexCache> = LazyLock::new(RegexCache::new);

pub struct RegexCache {
    cache: DashMap<String, Arc<Regex>>,
}

impl Default for RegexCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RegexCache {
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    pub fn get_or_compile(
        &self,
        pattern: &str,
    ) -> Result<Arc<Regex>, TuliproxError> {
        Ok(self.cache.entry(pattern.to_owned())
            .or_insert_with(|| {
                Arc::new(Regex::new(pattern).map_err(|e| {
                    info_err!("can't parse regex: {pattern} {e}")
                }).unwrap())
            })
            .clone())
    }


    /// Removes regexes that are only held by the cache itself (strong_count == 1).
    pub fn sweep(&self) {
        self.cache.retain(|_k, v| Arc::strong_count(v) > 1);
    }
}