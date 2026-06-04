use log::error;
use web_sys::window;

pub fn set_local_storage_item(key: &str, value: &str) {
    if let Some(storage) = window().and_then(|w| w.local_storage().ok()).flatten() {
        if let Err(err) = storage.set_item(key, value) {
            error!("failed to write to localStorage: {err:?}");
        }
    }
}

pub fn get_local_storage_item(key: &str) -> Option<String> {
    window().and_then(|w| w.local_storage().ok()).flatten().and_then(|storage| storage.get_item(key).ok().flatten())
}

pub fn remove_local_storage_item(key: &str) {
    if let Some(storage) = window().and_then(|w| w.local_storage().ok()).flatten() {
        if let Err(err) = storage.remove_item(key) {
            error!("failed to write to localStorage: {err:?}");
        }
    }
}

pub fn get_location_hash() -> Option<String> {
    window()
        .and_then(|w| w.location().hash().ok())
        .map(|hash| hash.trim_start_matches('#').to_string())
        .filter(|hash| !hash.is_empty())
}

pub fn set_location_hash(value: &str) {
    if let Some(window) = window() {
        if let Err(err) = window.location().set_hash(value) {
            error!("failed to set location hash: {err:?}");
        }
    }
}
