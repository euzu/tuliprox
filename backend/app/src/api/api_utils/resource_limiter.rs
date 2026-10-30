use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(super) const RESOURCE_HOST_CONCURRENCY_LIMIT: usize = 6;
pub(super) const RESOURCE_HOST_MAP_PRUNE_THRESHOLD: usize = 256;

static RESOURCE_HOST_LIMITERS: LazyLock<Mutex<HashMap<String, Arc<Semaphore>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) async fn acquire_resource_host_permit(host: &str) -> Option<OwnedSemaphorePermit> {
    let semaphore = {
        let Ok(mut guard) = RESOURCE_HOST_LIMITERS.lock() else {
            return None;
        };
        if let Some(semaphore) = guard.get(host) {
            semaphore.clone()
        } else {
            if guard.len() > RESOURCE_HOST_MAP_PRUNE_THRESHOLD {
                guard.retain(|_, semaphore| Arc::strong_count(semaphore) > 1);
            }
            let semaphore = Arc::new(Semaphore::new(RESOURCE_HOST_CONCURRENCY_LIMIT));
            guard.insert(host.to_string(), Arc::clone(&semaphore));
            semaphore
        }
    };
    semaphore.acquire_owned().await.ok()
}

#[cfg(test)]
pub(super) fn get_resource_host_semaphore(host: &str) -> Option<Arc<Semaphore>> {
    RESOURCE_HOST_LIMITERS.lock().ok()?.get(host).cloned()
}
