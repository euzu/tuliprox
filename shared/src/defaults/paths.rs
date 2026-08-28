//! Path defaults and path-blank-or-default predicates.

// Filesystem constants used as defaults for config paths.
pub const CONFIG_PATH: &str = "config";
pub const CONFIG_FILE: &str = "config.yml";
pub const SOURCE_FILE: &str = "source.yml";
pub const MAPPING_FILE: &str = "mapping.yml";
pub const TEMPLATE_FILE: &str = "template.yml";
pub const API_PROXY_FILE: &str = "api-proxy.yml";
pub const USER_FILE: &str = "user.txt";
pub const USER_GROUP_FILE: &str = "groups.txt";

pub const DEFAULT_WEB_DIR: &str = "web";
pub const DEFAULT_BACKUP_DIR: &str = "backup";
pub const DEFAULT_CACHE_DIR: &str = "cache";
pub const DEFAULT_STORAGE_TEMP_DIR: &str = "tmp";
pub const DEFAULT_USER_CONFIG_DIR: &str = "user_config";
pub const DEFAULT_DOWNLOAD_DIR: &str = "downloads";
pub const DEFAULT_STORAGE_DIR: &str = "data"; // TODO rename to storage and use data for config, storage, ...
pub const DEFAULT_CUSTOM_STREAM_RESPONSE_PATH: &str = "resources";

pub fn get_default_web_root() -> String {
    DEFAULT_WEB_DIR.to_string()
}
pub fn is_blank_or_default_web_root(value: &str) -> bool {
    let normalized = value.trim().replace('\\', "/");
    if normalized.is_empty() {
        return true;
    }

    let normalized = normalized.trim_end_matches('/');
    normalized.trim_start_matches("./") == DEFAULT_WEB_DIR
}

pub fn is_default_dir_path(value: &str, default_dir: &str) -> bool {
    let normalized = value.trim().replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    let normalized = normalized.trim_start_matches("./");
    let default_dir = default_dir.trim().replace('\\', "/");
    let default_dir = default_dir.trim_end_matches('/');
    normalized == default_dir
}

pub fn is_blank_or_default_download_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_DOWNLOAD_DIR))
}
pub fn default_download_dir() -> Option<String> {
    Some(DEFAULT_DOWNLOAD_DIR.to_string())
}

pub fn default_main_storage_dir() -> Option<String> {
    Some(DEFAULT_STORAGE_DIR.to_string())
}
pub fn default_main_backup_dir() -> Option<String> {
    Some(DEFAULT_BACKUP_DIR.to_string())
}
pub fn default_main_user_config_dir() -> Option<String> {
    Some(DEFAULT_USER_CONFIG_DIR.to_string())
}
pub fn default_main_mapping_path() -> Option<String> {
    Some(format!("./{CONFIG_PATH}/{MAPPING_FILE}"))
}
pub fn default_main_template_path() -> Option<String> {
    Some(format!("./{CONFIG_PATH}/{TEMPLATE_FILE}"))
}
pub fn default_custom_stream_response_path() -> Option<String> {
    Some(DEFAULT_CUSTOM_STREAM_RESPONSE_PATH.to_string())
}
pub fn default_user_file_path() -> Option<String> {
    Some(format!("./{CONFIG_PATH}/{USER_FILE}"))
}
pub fn default_user_group_file_path() -> Option<String> {
    Some(format!("./{CONFIG_PATH}/{USER_GROUP_FILE}"))
}

fn is_default_config_file_path(value: &str, file_name: &str) -> bool {
    let normalized = value.trim().replace('\\', "/");
    let normalized = normalized.trim_start_matches("./");
    normalized == file_name
        || normalized.rsplit_once('/').is_some_and(|(dir, file)| dir == CONFIG_PATH && file == file_name)
}

pub fn is_blank_or_default_custom_stream_response_path(path: &Option<String>) -> bool {
    path.as_ref()
        .is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_CUSTOM_STREAM_RESPONSE_PATH))
}

pub fn is_blank_or_default_mapping_path(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_config_file_path(value, MAPPING_FILE))
}

pub fn is_blank_or_default_template_path(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_config_file_path(value, TEMPLATE_FILE))
}

pub fn is_blank_or_default_storage_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_STORAGE_DIR))
}

pub fn is_blank_or_default_backup_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_BACKUP_DIR))
}

pub fn is_blank_or_default_user_config_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_USER_CONFIG_DIR))
}

pub fn is_blank_or_default_user_file_path(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_config_file_path(value, USER_FILE))
}

pub fn is_blank_or_default_user_group_file_path(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_config_file_path(value, USER_GROUP_FILE))
}

pub fn is_blank_or_default_cache_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_CACHE_DIR))
}

pub fn normalize_optional_dir(path: &Option<String>, default_dir: &str) -> Option<String> {
    path.as_ref().and_then(|value| {
        if value.trim().is_empty() || is_default_dir_path(value, default_dir) {
            None
        } else {
            Some(value.clone())
        }
    })
}

pub fn normalize_optional_config_file_path(path: &Option<String>, default_file_name: &str) -> Option<String> {
    path.as_ref().and_then(|value| {
        if value.trim().is_empty() || is_default_config_file_path(value, default_file_name) {
            None
        } else {
            Some(value.clone())
        }
    })
}
