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
    window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|storage| storage.get_item(key).ok().flatten())
}

pub fn remove_local_storage_item(key: &str) {
    if let Some(storage) = window().and_then(|w| w.local_storage().ok()).flatten() {
        if let Err(err) = storage.remove_item(key) {
            error!("failed to write to localStorage: {err:?}");
        }
    }
}
