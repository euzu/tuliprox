use crate::{model::Config, utils::LRUResourceCache};
use log::{error, info};
use std::sync::Arc;
use tokio::sync::RwLock;

pub fn create_cache(config: &Config) -> Option<Arc<RwLock<LRUResourceCache>>> {
    let lru_cache = config.reverse_proxy.as_ref().and_then(|r| r.cache.as_ref()).and_then(|c| {
        if c.enabled {
            Some(LRUResourceCache::new(c.size, c.directory.as_str()))
        } else {
            None
        }
    });
    let cache_enabled = lru_cache.is_some();
    if cache_enabled {
        info!("Scanning cache");
        if let Some(res_cache) = lru_cache {
            let cache = Arc::new(RwLock::new(res_cache));
            let cache_scanner = Arc::clone(&cache);
            tokio::spawn(async move {
                let scan_result = tokio::task::spawn_blocking(move || {
                    let mut cache = cache_scanner.blocking_write();
                    cache.scan()
                })
                .await;
                match scan_result {
                    Ok(Err(err)) => error!("Failed to scan cache {err}"),
                    Err(err) => error!("Failed to join cache scan task: {err}"),
                    Ok(Ok(())) => {}
                }
            });
            return Some(cache);
        }
    }
    None
}
