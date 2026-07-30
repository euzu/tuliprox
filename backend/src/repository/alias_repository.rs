use crate::{
    model::is_input_expired,
    utils::{file_reader, request::get_local_file_content, resolve_relative_path, EnvResolvingReader},
};
use chrono::Local;
use futures::TryFutureExt;
use log::{error, warn};
use shared::{
    error::{string_to_io_error, to_io_error, TuliproxError},
    model::{
        ConfigInputAliasDto, InputType, StalkerAuthMode, StalkerDeviceProfileDto, StalkerEndpointPreference,
        StalkerInputConfigDto, StalkerMagPreset,
    },
    utils::{
        get_credentials_from_url, get_credentials_from_url_str, parse_timestamp, sanitize_sensitive_info, Internable,
        BATCH_SCHEME_PREFIX, PROVIDER_SCHEME_PREFIX,
    },
};
use std::{
    collections::HashMap,
    io,
    io::{BufRead, Cursor, Error},
    path::{Path, PathBuf},
    sync::{atomic::{AtomicU64, Ordering}, Arc},
};
use url::Url;
use uuid::Uuid;

const CSV_SEPARATOR: char = ';';
const HEADER_PREFIX: char = '#';
const FIELD_MAX_CON: &str = "max_connections";
const FIELD_PRIO: &str = "priority";
const FIELD_URL: &str = "url";
const FIELD_NAME: &str = "name";
const FIELD_USERNAME: &str = "username";
const FIELD_PASSWORD: &str = "password";
const FIELD_EXP_DATE: &str = "exp_date";
const FIELD_ENABLED: &str = "enabled";
const FIELD_STALKER_MAC_ADDRESS: &str = "mac_address";
const FIELD_STALKER_AUTH_MODE: &str = "auth_mode";
const FIELD_STALKER_MAG_PRESET: &str = "mag_preset";
const FIELD_STALKER_ENDPOINT_PREFERENCE: &str = "endpoint_preference";
const FIELD_UNKNOWN: &str = "?";
const DEFAULT_COLUMNS: &[&str] =
    &[FIELD_URL, FIELD_MAX_CON, FIELD_PRIO, FIELD_NAME, FIELD_USERNAME, FIELD_PASSWORD, FIELD_EXP_DATE, FIELD_ENABLED];
const CSV_EXTENSION: &str = ".csv";
static CSV_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasExpDateSortOrder {
    NewestFirst,
    OldestFirst,
}

pub struct BatchExpDateUpdate {
    pub account_key: String,
    pub account_name: Arc<str>,
    pub exp_date: i64,
    pub disable: bool,
}

pub fn compare_alias_exp_date_with_order(
    a: &ConfigInputAliasDto,
    b: &ConfigInputAliasDto,
    order: AliasExpDateSortOrder,
) -> std::cmp::Ordering {
    match (a.exp_date, b.exp_date) {
        (Some(a_ts), Some(b_ts)) => match order {
            AliasExpDateSortOrder::NewestFirst => b_ts.cmp(&a_ts),
            AliasExpDateSortOrder::OldestFirst => a_ts.cmp(&b_ts),
        },
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
    .then_with(|| a.name.cmp(&b.name))
}

pub fn is_csv_file(url: &str) -> bool { url.to_lowercase().ends_with(CSV_EXTENSION) }

fn build_m3u_url(base: &Url, username: Option<&str>, password: Option<&str>) -> Result<Url, url::ParseError> {
    let base_origin = base.origin().ascii_serialization();
    let mut url = base_origin.parse::<Url>()?.join("get.php")?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("username", username.unwrap_or(""));
        qp.append_pair("password", password.unwrap_or(""));
        qp.append_pair("type", "m3u_plus");
    }

    Ok(url)
}

fn csv_assign_mandatory_fields(alias: &mut ConfigInputAliasDto, input_type: InputType) {
    if !alias.url.is_empty() {
        let mut provider_scheme = false;
        if alias.url.starts_with(PROVIDER_SCHEME_PREFIX) {
            provider_scheme = true;
            alias.url = alias.url.replacen(PROVIDER_SCHEME_PREFIX, "http://", 1);
        }
        match Url::parse(alias.url.as_str()) {
            Ok(url) => {
                let (username, password) = get_credentials_from_url(&url);
                if username.is_none() || password.is_none() {
                    // xtream url
                    if input_type == InputType::XtreamBatch {
                        alias.url = url.origin().ascii_serialization();
                    } else if input_type == InputType::M3uBatch && alias.username.is_some() && alias.password.is_some()
                    {
                        match build_m3u_url(&url, alias.username.as_deref(), alias.password.as_deref()) {
                            Ok(alias_url) => {
                                alias.url = alias_url.to_string();
                            }
                            Err(err) => {
                                error!("Could not build m3u url for alias {}: {err}", alias.name);
                            }
                        }
                    }
                } else {
                    if input_type == InputType::XtreamBatch {
                        alias.url = url.origin().ascii_serialization();
                    }
                    // m3u url
                    alias.username = username;
                    alias.password = password;
                }

                if alias.name.is_empty() {
                    let username = alias.username.as_deref().unwrap_or_default();
                    let domain: Vec<&str> = url.domain().unwrap_or_default().split('.').collect();
                    if domain.len() > 1 {
                        alias.name = format!("{}_{username}", domain[domain.len() - 2]).intern();
                    } else {
                        alias.name = username.intern();
                    }
                }
            }
            Err(err) => {
                warn!("Could not parse URL '{}' for alias: {err}", sanitize_sensitive_info(&alias.url));
            }
        }
        if provider_scheme {
            alias.url = alias.url.replacen("http://", PROVIDER_SCHEME_PREFIX, 1);
        }
    }
}

fn str_to_bool(val: &str) -> bool {
    if val.is_empty() || val == "1" {
        return true;
    }
    if val == "0" || val.eq_ignore_ascii_case("f") || val.eq_ignore_ascii_case("false") {
        return false;
    }
    true
}

fn csv_assign_config_input_column(
    config_input: &mut ConfigInputAliasDto,
    input_type: InputType,
    header: &str,
    raw_value: &str,
) -> Result<(), io::Error> {
    let value = raw_value.trim();
    if !value.is_empty() {
        match header {
            FIELD_URL => {
                let url = Url::parse(value.trim()).map_err(to_io_error)?;
                config_input.url = url.to_string();
            }
            FIELD_MAX_CON => {
                let max_connections = value.parse::<u16>().unwrap_or(1);
                config_input.max_connections = max_connections;
            }
            FIELD_PRIO => {
                let priority = value.parse::<i16>().unwrap_or(0);
                config_input.priority = priority;
            }
            FIELD_NAME => {
                config_input.name = value.intern();
            }
            FIELD_USERNAME => {
                config_input.username = Some(value.to_string());
            }
            FIELD_PASSWORD => {
                config_input.password = Some(value.to_string());
            }
            FIELD_EXP_DATE => {
                config_input.exp_date = parse_timestamp(value).unwrap_or_else(|e| {
                    error!("Failed to parse exp_date '{value}': {e}");
                    None
                });
            }
            FIELD_ENABLED => {
                config_input.enabled = str_to_bool(value);
            }
            FIELD_STALKER_MAC_ADDRESS if input_type == InputType::StalkerBatch => {
                let stalker = config_input.stalker.get_or_insert_with(StalkerInputConfigDto::default);
                let device = stalker.device.get_or_insert_with(StalkerDeviceProfileDto::default);
                device.mac_address = Some(value.to_string());
            }
            FIELD_STALKER_AUTH_MODE if input_type == InputType::StalkerBatch => {
                let auth_mode = value.parse::<StalkerAuthMode>().map_err(to_io_error)?;
                config_input.stalker.get_or_insert_with(StalkerInputConfigDto::default).auth_mode = auth_mode;
            }
            FIELD_STALKER_MAG_PRESET if input_type == InputType::StalkerBatch => {
                let mag_preset = value.parse::<StalkerMagPreset>().map_err(to_io_error)?;
                config_input.stalker.get_or_insert_with(StalkerInputConfigDto::default).mag_preset = mag_preset;
            }
            FIELD_STALKER_ENDPOINT_PREFERENCE if input_type == InputType::StalkerBatch => {
                let endpoint_preference = value.parse::<StalkerEndpointPreference>().map_err(to_io_error)?;
                config_input.stalker.get_or_insert_with(StalkerInputConfigDto::default).endpoint_preference =
                    endpoint_preference;
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn csv_read_inputs_from_reader(
    batch_input_type: InputType,
    reader: impl BufRead,
) -> Result<Vec<ConfigInputAliasDto>, Error> {
    let input_type = match batch_input_type {
        InputType::M3uBatch | InputType::M3u => InputType::M3uBatch,
        InputType::XtreamBatch | InputType::Xtream => InputType::XtreamBatch,
        InputType::Stalker | InputType::StalkerBatch => InputType::StalkerBatch,
        InputType::Library => InputType::Library,
        InputType::Emby | InputType::Jellyfin | InputType::Plex | InputType::Staged => batch_input_type,
    };
    let mut result = vec![];
    let mut default_columns = vec![];
    default_columns.extend_from_slice(DEFAULT_COLUMNS);
    let mut header_defined = false;
    for (line_idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        if line.starts_with(HEADER_PREFIX) {
            if !header_defined {
                header_defined = true;
                default_columns = line[1..]
                    .split(CSV_SEPARATOR)
                    .map(|s| match s {
                        FIELD_URL => FIELD_URL,
                        FIELD_MAX_CON => FIELD_MAX_CON,
                        FIELD_PRIO => FIELD_PRIO,
                        FIELD_NAME => FIELD_NAME,
                        FIELD_USERNAME => FIELD_USERNAME,
                        FIELD_PASSWORD => FIELD_PASSWORD,
                        FIELD_EXP_DATE => FIELD_EXP_DATE,
                        FIELD_ENABLED => FIELD_ENABLED,
                        FIELD_STALKER_MAC_ADDRESS => FIELD_STALKER_MAC_ADDRESS,
                        FIELD_STALKER_AUTH_MODE => FIELD_STALKER_AUTH_MODE,
                        FIELD_STALKER_MAG_PRESET => FIELD_STALKER_MAG_PRESET,
                        FIELD_STALKER_ENDPOINT_PREFERENCE => FIELD_STALKER_ENDPOINT_PREFERENCE,
                        _ => {
                            error!("Field {s} is unsupported for csv input");
                            FIELD_UNKNOWN
                        }
                    })
                    .collect();
            }
            continue;
        }

        let mut config_input = ConfigInputAliasDto {
            id: 0,
            name: "".intern(),
            url: String::new(),
            username: None,
            password: None,
            priority: 0,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        };

        let columns: Vec<&str> = line.split(CSV_SEPARATOR).collect();
        let mut invalid = false;
        for (&header, &value) in default_columns.iter().zip(columns.iter()) {
            if let Err(err) = csv_assign_config_input_column(&mut config_input, input_type, header, value) {
                error!("Could not parse input line: {} err: {err}", line_idx + 1);
                invalid = true;
            }
        }
        if invalid {
            continue;
        }
        csv_assign_mandatory_fields(&mut config_input, input_type);
        if config_input.url.is_empty() {
            warn!("Skipping CSV line {}: missing or invalid url", line_idx + 1);
            continue;
        }
        result.push(config_input);
    }
    Ok(result)
}

async fn csv_read_inputs_from_path(
    input_type: InputType,
    file_path: &Path,
) -> Result<(PathBuf, Vec<ConfigInputAliasDto>), Error> {
    match get_local_file_content(file_path).await {
        Ok(content) => Ok((
            file_path.to_path_buf(),
            csv_read_inputs_from_reader(input_type, EnvResolvingReader::new(file_reader(Cursor::new(content))))?,
        )),
        Err(err) => Err(err),
    }
}

pub async fn csv_read_inputs(
    input_type: InputType,
    file_uri: &str,
) -> Result<(PathBuf, Vec<ConfigInputAliasDto>), Error> {
    let file_path = get_csv_file_path(file_uri)?;
    csv_read_inputs_from_path(input_type, &file_path).await
}

pub fn get_csv_file_path(file_uri: &str) -> Result<PathBuf, Error> {
    // Handle batch:// scheme: strip prefix and treat remainder as file path.
    if let Some(path_str) = file_uri.strip_prefix(BATCH_SCHEME_PREFIX) {
        let path = Path::new(path_str);
        return if path.is_absolute() { Ok(path.to_path_buf()) } else { resolve_relative_path(path_str) };
    }
    let raw_path = Path::new(file_uri);
    if raw_path.is_absolute() {
        return Ok(raw_path.to_path_buf());
    }
    if let Ok(_url) = file_uri.parse::<Url>() {
        Err(string_to_io_error(format!("Unsupported URL scheme for batch CSV, use batch:// instead: {file_uri}")))
    } else {
        resolve_relative_path(file_uri)
    }
}

pub async fn csv_backup_file(csv_path: &Path, backup_dir: &str) -> Result<(), TuliproxError> {
    let filename = csv_path.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
        TuliproxError::ConfigInput(format!("Could not derive a filename for alias CSV {}", csv_path.display()))
    })?;
    let backup_dir = PathBuf::from(backup_dir);
    tokio::fs::create_dir_all(&backup_dir)
        .await
        .map_err(|err| TuliproxError::ConfigInput(format!("Could not create alias CSV backup directory: {err}")))?;
    let backup_path = backup_dir.join(format!(
        "{filename}_{}_{}",
        Local::now().format("%Y%m%d_%H%M%S%9f"),
        Uuid::new_v4()
    ));
    let mut source = tokio::fs::File::open(csv_path)
        .await
        .map_err(|err| TuliproxError::ConfigInput(format!("Could not open alias CSV for backup: {err}")))?;
    let source_permissions = source
        .metadata()
        .await
        .map_err(|err| TuliproxError::ConfigInput(format!("Could not read alias CSV metadata: {err}")))?
        .permissions();
    let mut backup = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup_path)
        .await
        .map_err(|err| TuliproxError::ConfigInput(format!("Could not create alias CSV backup {}: {err}", backup_path.display())))?;
    if let Err(err) = tokio::io::copy(&mut source, &mut backup).await {
        drop(backup);
        let _ = tokio::fs::remove_file(&backup_path).await;
        return Err(TuliproxError::ConfigInput(format!(
            "Could not backup alias CSV to {}: {err}",
            backup_path.display()
        )));
    }
    drop(backup);
    if let Err(err) = tokio::fs::set_permissions(&backup_path, source_permissions).await {
        let _ = tokio::fs::remove_file(&backup_path).await;
        return Err(TuliproxError::ConfigInput(format!("Could not preserve alias CSV backup permissions: {err}")));
    }
    Ok(())
}

async fn csv_write_input_to_path(file_path: &Path, aliases: &[ConfigInputAliasDto]) -> Result<(), Error> {
    let write_stalker_fields = aliases.iter().any(|alias| alias.stalker.is_some());
    let mut content = String::new();
    content.push(HEADER_PREFIX);
    content.push_str(FIELD_NAME);
    content.push(CSV_SEPARATOR);
    content.push_str(FIELD_USERNAME);
    content.push(CSV_SEPARATOR);
    content.push_str(FIELD_PASSWORD);
    content.push(CSV_SEPARATOR);
    content.push_str(FIELD_URL);
    content.push(CSV_SEPARATOR);
    if write_stalker_fields {
        content.push_str(FIELD_STALKER_MAC_ADDRESS);
        content.push(CSV_SEPARATOR);
        content.push_str(FIELD_STALKER_AUTH_MODE);
        content.push(CSV_SEPARATOR);
        content.push_str(FIELD_STALKER_MAG_PRESET);
        content.push(CSV_SEPARATOR);
        content.push_str(FIELD_STALKER_ENDPOINT_PREFERENCE);
        content.push(CSV_SEPARATOR);
    }
    content.push_str(FIELD_ENABLED);
    content.push(CSV_SEPARATOR);
    content.push_str(FIELD_MAX_CON);
    content.push(CSV_SEPARATOR);
    content.push_str(FIELD_PRIO);
    content.push(CSV_SEPARATOR);
    content.push_str(FIELD_EXP_DATE);
    content.push('\n');

    for alias in aliases {
        content.push_str(&alias.name);
        content.push(CSV_SEPARATOR);
        content.push_str(alias.username.as_deref().unwrap_or(""));
        content.push(CSV_SEPARATOR);
        content.push_str(alias.password.as_deref().unwrap_or(""));
        content.push(CSV_SEPARATOR);
        content.push_str(&alias.url);
        content.push(CSV_SEPARATOR);
        if write_stalker_fields {
            if let Some(stalker) = alias.stalker.as_ref() {
                content
                    .push_str(stalker.device.as_ref().and_then(|device| device.mac_address.as_deref()).unwrap_or(""));
                content.push(CSV_SEPARATOR);
                content.push_str(stalker.auth_mode.as_ref());
                content.push(CSV_SEPARATOR);
                content.push_str(stalker.mag_preset.as_ref());
                content.push(CSV_SEPARATOR);
                content.push_str(stalker.endpoint_preference.as_ref());
            } else {
                content.push_str(";;;");
            }
            content.push(CSV_SEPARATOR);
        }
        content.push_str(if alias.enabled { "1" } else { "0" });
        content.push(CSV_SEPARATOR);
        content.push_str(&alias.max_connections.to_string());
        content.push(CSV_SEPARATOR);
        content.push_str(&alias.priority.to_string());
        content.push(CSV_SEPARATOR);
        if let Some(exp) = alias.exp_date {
            content.push_str(&shared::utils::unix_ts_to_str_with_format(exp, "%Y-%m-%d %H:%M:%S").unwrap_or_default());
        }
        content.push('\n');
    }

    csv_write_content_to_path(file_path, content).await
}

async fn csv_write_content_to_path(file_path: &Path, content: String) -> Result<(), Error> {
    let tmp_path = csv_temp_path(file_path)?;
    tokio::fs::write(&tmp_path, content).await.map_err(to_io_error)?;
    if let Err(err) = preserve_csv_metadata(file_path, &tmp_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(err);
    }
    if let Err(err) = replace_csv_file(&tmp_path, file_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(err);
    }
    Ok(())
}

async fn preserve_csv_metadata(file_path: &Path, tmp_path: &Path) -> Result<(), Error> {
    let metadata = match tokio::fs::metadata(file_path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    tokio::fs::set_permissions(tmp_path, metadata.permissions()).await?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let source = file_path.to_path_buf();
        let target = tmp_path.to_path_buf();
        tokio::task::spawn_blocking(move || copy_platform_acl(&source, &target))
            .await
            .map_err(|err| io::Error::other(format!("ACL copy task failed: {err}")))??;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_platform_acl(source: &Path, target: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    const ACL_NAME: &[u8] = b"system.posix_acl_access\0";
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains a null byte"))?;
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target path contains a null byte"))?;
    // SAFETY: Both pointers are valid null-terminated strings and the null buffer requests the required size.
    let size = unsafe { libc::getxattr(source.as_ptr(), ACL_NAME.as_ptr().cast(), std::ptr::null_mut(), 0) };
    if size < 0 {
        let err = io::Error::last_os_error();
        if acl_is_missing_or_unsupported(&err) {
            // SAFETY: Both pointers are valid null-terminated strings.
            let removed = unsafe { libc::removexattr(target.as_ptr(), ACL_NAME.as_ptr().cast()) };
            if removed != 0 {
                let remove_err = io::Error::last_os_error();
                if !acl_is_missing_or_unsupported(&remove_err) {
                    return Err(remove_err);
                }
            }
            return Ok(());
        }
        return Err(err);
    }
    let mut acl = vec![0_u8; usize::try_from(size).map_err(|_| io::Error::other("ACL size is invalid"))?];
    // SAFETY: The buffer has the size returned by getxattr and all pointers remain valid for the call.
    let read = unsafe { libc::getxattr(source.as_ptr(), ACL_NAME.as_ptr().cast(), acl.as_mut_ptr().cast(), acl.len()) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    acl.truncate(usize::try_from(read).map_err(|_| io::Error::other("ACL size is invalid"))?);
    // SAFETY: The target path, attribute name, and ACL buffer are valid for the duration of the call.
    let written = unsafe { libc::setxattr(target.as_ptr(), ACL_NAME.as_ptr().cast(), acl.as_ptr().cast(), acl.len(), 0) };
    if written == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
fn copy_platform_acl(source: &Path, target: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains a null byte"))?;
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target path contains a null byte"))?;
    // SAFETY: Both paths are valid null-terminated strings; COPYFILE_ACL copies metadata without replacing file data.
    let result = unsafe { libc::copyfile(source.as_ptr(), target.as_ptr(), std::ptr::null_mut(), libc::COPYFILE_ACL) };
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "linux")]
fn acl_is_missing_or_unsupported(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::ENODATA) || err.raw_os_error() == Some(libc::ENOTSUP)
}

async fn replace_csv_file(source: &Path, target: &Path) -> io::Result<()> {
    match tokio::fs::rename(source, target).await {
        Ok(()) => Ok(()),
        Err(err) => {
            #[cfg(windows)]
            if matches!(tokio::fs::try_exists(target).await, Ok(true)) {
                return replace_existing_csv_windows(source, target).await;
            }
            Err(err)
        }
    }
}

#[cfg(windows)]
async fn replace_existing_csv_windows(source: &Path, target: &Path) -> io::Result<()> {
    use std::{iter::once, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let backup_path = target.with_extension(format!("replace-backup-{}", Uuid::new_v4()));
    let source_wide = source.as_os_str().encode_wide().chain(once(0)).collect::<Vec<_>>();
    let target_wide = target.as_os_str().encode_wide().chain(once(0)).collect::<Vec<_>>();
    let backup_wide = backup_path.as_os_str().encode_wide().chain(once(0)).collect::<Vec<_>>();
    let replace_result = tokio::task::spawn_blocking(move || {
        // SAFETY: All path buffers are null-terminated and remain alive for the complete system call.
        let result = unsafe {
            ReplaceFileW(
                target_wide.as_ptr(),
                source_wide.as_ptr(),
                backup_wide.as_ptr(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    })
    .await
    .map_err(|err| io::Error::other(format!("Windows file replacement task failed: {err}")))?;

    match replace_result {
        Ok(()) => {
            let _ = tokio::fs::remove_file(&backup_path).await;
            Ok(())
        }
        Err(replace_err) => {
            if !matches!(tokio::fs::try_exists(target).await, Ok(true))
                && matches!(tokio::fs::try_exists(&backup_path).await, Ok(true))
            {
                tokio::fs::rename(&backup_path, target).await.map_err(|restore_err| {
                    io::Error::other(format!("File replacement failed ({replace_err}); restoring the original failed: {restore_err}"))
                })?;
            }
            Err(replace_err)
        }
    }
}

fn csv_temp_path(file_path: &Path) -> Result<PathBuf, Error> {
    let file_name = file_path.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
        string_to_io_error(format!("Could not derive filename for alias CSV {}", file_path.display()))
    })?;
    let suffix = CSV_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
    Ok(file_path.with_file_name(format!(".{file_name}.tmp-{}-{suffix}", std::process::id())))
}

pub async fn csv_write_inputs(file_uri: &str, aliases: &[ConfigInputAliasDto]) -> Result<(), Error> {
    let file_path = get_csv_file_path(file_uri)?;
    csv_write_input_to_path(&file_path, aliases).await
}

pub async fn csv_patch_batch_append(
    csv_path: &Path,
    input_type: InputType,
    alias_name: &str,
    base_url: &str,
    username: &str,
    password: &str,
    exp_date: Option<i64>,
) -> Result<(), TuliproxError> {
    // TODO check if alias name exists in any config ?

    let (file_path, mut aliases) = csv_read_inputs_from_path(input_type, csv_path)
        .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
        .await?;

    let url = if input_type == InputType::M3uBatch {
        let base = Url::parse(base_url).map_err(|e| TuliproxError::Config(format!("{e}")))?;
        build_m3u_url(&base, Some(username), Some(password))
            .map_err(|e| TuliproxError::ConfigInput(format!("{e}")))?
            .to_string()
    } else {
        base_url.to_string()
    };

    let alias = ConfigInputAliasDto {
        id: 0,
        name: alias_name.intern(),
        url,
        username: Some(username.to_string()),
        password: Some(password.to_string()),
        priority: 0,
        max_connections: 1,
        exp_date,
        enabled: true,
        stalker: None,
    };
    aliases.push(alias);

    csv_write_input_to_path(&file_path, &aliases).map_err(|err| TuliproxError::ConfigInput(format!("{err}"))).await?;
    Ok(())
}

pub async fn csv_patch_batch_update_exp_date(
    input_type: InputType,
    csv_path: &Path,
    account_name: &Arc<str>,
    username: &str,
    password: &str,
    exp_date: i64,
) -> Result<(), TuliproxError> {
    let mut matched = false;
    let (file_path, mut aliases) = csv_read_inputs_from_path(input_type, csv_path)
        .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
        .await?;
    for alias in &mut aliases {
        if &alias.name == account_name
            || (alias.username == Some(username.to_string()) && alias.password == Some(password.to_string()))
        {
            alias.exp_date = Some(exp_date);
            alias.max_connections = 1;
            matched = true;
        } else if let (Some(u), Some(p)) = get_credentials_from_url_str(&alias.url) {
            if u == username && p == password {
                alias.exp_date = Some(exp_date);
                alias.max_connections = 1;
                matched = true;
            }
        }
    }

    if matched {
        csv_write_input_to_path(&file_path, &aliases)
            .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
            .await?;
    } else {
        warn!("panel_api: could not find batch csv row for account {account_name}");
    }
    Ok(())
}

pub async fn csv_patch_batch_update_exp_dates(
    _input_type: InputType,
    csv_path: &Path,
    updates: &[BatchExpDateUpdate],
    backup_dir: &str,
) -> Result<(bool, Vec<String>), TuliproxError> {
    if updates.is_empty() {
        return Ok((false, Vec::new()));
    }

    let content = get_local_file_content(csv_path)
        .await
        .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))?;
    let updates_by_name = updates
        .iter()
        .map(|update| (update.account_name.as_ref(), update))
        .collect::<HashMap<_, _>>();
    let mut columns = DEFAULT_COLUMNS.iter().map(|column| (*column).to_string()).collect::<Vec<_>>();
    let mut header_defined = false;
    let mut header_extended = false;
    let mut matched_account_keys = Vec::new();
    let mut changed = false;
    let mut patched = String::with_capacity(content.len());
    for raw_line in content.split_inclusive('\n') {
        let has_newline = raw_line.ends_with('\n');
        let line_with_cr = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let has_carriage_return = line_with_cr.ends_with('\r');
        let line = line_with_cr.strip_suffix('\r').unwrap_or(line_with_cr);
        if line.starts_with(HEADER_PREFIX) {
            let header_columns = line[1..].split(CSV_SEPARATOR).collect::<Vec<_>>();
            let is_header = !header_defined && header_columns.contains(&FIELD_NAME);
            if is_header {
                header_defined = true;
                columns = header_columns.into_iter().map(ToString::to_string).collect();
                for required in [FIELD_ENABLED, FIELD_EXP_DATE] {
                    if !columns.iter().any(|column| column == required) {
                        columns.push(required.to_string());
                        header_extended = true;
                    }
                }
            }
            if is_header && header_extended {
                patched.push(HEADER_PREFIX);
                push_csv_row(&mut patched, &columns);
                push_line_ending(&mut patched, has_newline, has_carriage_return);
            } else {
                patched.push_str(raw_line);
            }
            continue;
        }
        if line.is_empty() {
            patched.push_str(raw_line);
            continue;
        }

        let name_index = columns.iter().position(|column| column == FIELD_NAME);
        let exp_index = columns.iter().position(|column| column == FIELD_EXP_DATE);
        let enabled_index = columns.iter().position(|column| column == FIELD_ENABLED);
        let mut values = line.split(CSV_SEPARATOR).map(ToString::to_string).collect::<Vec<_>>();
        if header_extended && values.len() < columns.len() {
            values.resize(columns.len(), String::new());
        }
        let update = name_index
            .and_then(|index| values.get(index))
            .and_then(|name| updates_by_name.get(name.as_str()))
            .copied();
        if let Some(update) = update {
            if values.len() < columns.len() {
                values.resize(columns.len(), String::new());
            }
            matched_account_keys.push(update.account_key.clone());
            if let Some(index) = exp_index {
                let expiration = shared::utils::unix_ts_to_str_with_format(update.exp_date, "%Y-%m-%d %H:%M:%S")
                    .unwrap_or_else(|| update.exp_date.to_string());
                if values[index] != expiration {
                    values[index] = expiration;
                    changed = true;
                }
            }
            if update.disable {
                if let Some(index) = enabled_index.filter(|index| values[*index] != "0") {
                    values[index] = "0".to_string();
                    changed = true;
                }
            }
        }
        if update.is_some() || header_extended {
            push_csv_row(&mut patched, &values);
            push_line_ending(&mut patched, has_newline, has_carriage_return);
        } else {
            patched.push_str(raw_line);
        }
    }
    if changed {
        csv_backup_file(csv_path, backup_dir).await?;
        csv_write_content_to_path(csv_path, patched)
            .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
            .await?;
    }
    Ok((changed, matched_account_keys))
}

fn push_csv_row(content: &mut String, values: &[String]) {
    if let Some((first, remaining)) = values.split_first() {
        content.push_str(first);
        for value in remaining {
            content.push(CSV_SEPARATOR);
            content.push_str(value);
        }
    }
}

fn push_line_ending(content: &mut String, has_newline: bool, has_carriage_return: bool) {
    if has_carriage_return {
        content.push('\r');
    }
    if has_newline {
        content.push('\n');
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn csv_patch_batch_update_credentials(
    input_type: InputType,
    csv_path: &Path,
    account_name: &Arc<str>,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
    exp_date: Option<i64>,
) -> Result<(), TuliproxError> {
    let mut matched = false;
    let (file_path, mut aliases) = csv_read_inputs_from_path(input_type, csv_path)
        .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
        .await?;

    for alias in &mut aliases {
        let mut is_match = &alias.name == account_name;
        if !is_match {
            is_match =
                alias.username.as_deref() == Some(old_username) && alias.password.as_deref() == Some(old_password);
        }
        if !is_match {
            is_match =
                alias.username.as_deref() == Some(new_username) && alias.password.as_deref() == Some(new_password);
        }

        if !is_match {
            if let (Some(u), Some(p)) = get_credentials_from_url_str(&alias.url) {
                is_match = (u == old_username && p == old_password) || (u == new_username && p == new_password);
            }
        }

        if !is_match {
            continue;
        }

        alias.username = Some(new_username.to_string());
        alias.password = Some(new_password.to_string());
        alias.max_connections = 1;
        if let Some(exp_date) = exp_date {
            alias.exp_date = Some(exp_date);
        }

        if matches!(input_type, InputType::M3uBatch | InputType::M3u) {
            if let Ok(mut url) = Url::parse(alias.url.as_str()) {
                let mut pairs: Vec<(String, String)> =
                    url.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                let mut has_user = false;
                let mut has_pass = false;
                for (k, v) in &mut pairs {
                    if k.eq_ignore_ascii_case("username") {
                        *v = new_username.to_string();
                        has_user = true;
                    } else if k.eq_ignore_ascii_case("password") {
                        *v = new_password.to_string();
                        has_pass = true;
                    }
                }
                if has_user || has_pass {
                    if !has_user {
                        pairs.push(("username".to_string(), new_username.to_string()));
                    }
                    if !has_pass {
                        pairs.push(("password".to_string(), new_password.to_string()));
                    }
                    url.query_pairs_mut().clear();
                    {
                        let mut qp = url.query_pairs_mut();
                        for (k, v) in pairs {
                            qp.append_pair(k.as_str(), v.as_str());
                        }
                    }
                    alias.url = url.to_string();
                }
            }
        }

        matched = true;
    }

    if matched {
        csv_write_input_to_path(&file_path, &aliases)
            .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
            .await?;
    } else {
        warn!("panel_api: could not find batch csv row to update credentials for account {account_name}");
    }
    Ok(())
}

pub async fn csv_patch_batch_remove_expired(input_type: InputType, csv_path: &Path) -> Result<bool, TuliproxError> {
    let (file_path, mut aliases) = csv_read_inputs_from_path(input_type, csv_path)
        .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
        .await?;
    let before_len = aliases.len();
    aliases.retain(|alias| !is_input_expired(alias.exp_date));
    let changed = before_len != aliases.len();
    if changed {
        csv_write_input_to_path(&file_path, &aliases)
            .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
            .await?;
    }
    Ok(changed)
}

pub async fn csv_patch_batch_sort_by_exp_date(
    input_type: InputType,
    csv_path: &Path,
    order: AliasExpDateSortOrder,
) -> Result<bool, TuliproxError> {
    let (file_path, mut aliases) = csv_read_inputs_from_path(input_type, csv_path)
        .map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
        .await?;
    if aliases.len() < 2 {
        return Ok(false);
    }
    let mut sorted = aliases.clone();
    sorted.sort_by(|a, b| compare_alias_exp_date_with_order(a, b, order));
    if sorted == aliases {
        return Ok(false);
    }
    aliases = sorted;
    csv_write_input_to_path(&file_path, &aliases).map_err(|err| TuliproxError::ConfigInput(format!("{err}"))).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{
        csv_backup_file, csv_patch_batch_sort_by_exp_date, csv_patch_batch_update_exp_dates, csv_read_inputs_from_path,
        csv_temp_path, csv_write_input_to_path, AliasExpDateSortOrder, BatchExpDateUpdate,
    };
    use crate::{repository::csv_read_inputs_from_reader, utils::file_reader};
    use shared::model::{InputType, StalkerAuthMode, StalkerEndpointPreference, StalkerMagPreset};
    use std::{io::Cursor, path::PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const M3U_BATCH: &str = r"
#url;name;max_connections;priority
http://hd.providerline.com:8080/get.php?username=user1&password=user1&type=m3u_plus;input_1
http://hd.providerline.com/get.php?username=user2&password=user2&type=m3u_plus;input_2;1;2
http://hd.providerline.com/get.php?username=user3&password=user3&type=m3u_plus;input_3;1;2
http://hd.providerline.com/get.php?username=user4&password=user4&type=m3u_plus;input_4
";

    const XTREAM_BATCH: &str = r"
#name;username;password;url;max_connections;exp_date
input_1;de566567;de2345f43g5;http://provider_1.tv:80;1;2028-11-23 13:12:34
input_2;de566567;de2345f43g5;http://provider_2.tv:8080;1;2028-12-23 13:12:34
";

    #[test]
    fn stalker_batch_keeps_portal_path() -> Result<(), std::io::Error> {
        let input = "http://portal.example/c/;1;0;portal;;;1\n";
        let aliases = csv_read_inputs_from_reader(InputType::StalkerBatch, Cursor::new(input))?;

        assert_eq!(aliases[0].url, "http://portal.example/c/");
        Ok(())
    }

    #[test]
    fn stalker_batch_reads_optional_alias_fields_by_header() -> Result<(), std::io::Error> {
        let input = "#endpoint_preference;url;mac_address;auth_mode;name;mag_preset\n\
portal;http://portal.example/c/;00:1A:79:12:34:56;mac_plus_credentials;primary;mag254_strict\n\
;http://backup.example/c/;;;backup;\n";
        let aliases = csv_read_inputs_from_reader(InputType::StalkerBatch, Cursor::new(input))?;

        let stalker = aliases
            .first()
            .ok_or_else(|| std::io::Error::other("missing primary Stalker alias"))?
            .stalker
            .as_ref()
            .ok_or_else(|| std::io::Error::other("missing Stalker alias configuration"))?;
        assert_eq!(stalker.auth_mode, StalkerAuthMode::MacPlusCredentials);
        assert_eq!(stalker.mag_preset, StalkerMagPreset::Mag254Strict);
        assert_eq!(stalker.endpoint_preference, StalkerEndpointPreference::Portal);
        assert_eq!(stalker.device.as_ref().and_then(|device| device.mac_address.as_deref()), Some("00:1A:79:12:34:56"));
        assert!(aliases.get(1).ok_or_else(|| std::io::Error::other("missing backup Stalker alias"))?.stalker.is_none());
        Ok(())
    }

    #[test]
    fn stalker_batch_skips_alias_with_invalid_enum() -> Result<(), std::io::Error> {
        let input = "#url;auth_mode\nhttp://portal.example/c/;invalid\n";
        let aliases = csv_read_inputs_from_reader(InputType::StalkerBatch, Cursor::new(input))?;

        assert!(aliases.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn stalker_batch_write_preserves_alias_fields() -> Result<(), std::io::Error> {
        let input = "#name;url;mac_address;auth_mode;mag_preset;endpoint_preference\n\
primary;http://portal.example/c/;00:1A:79:12:34:56;mac_only;mag254_strict;portal\n";
        let aliases = csv_read_inputs_from_reader(InputType::StalkerBatch, Cursor::new(input))?;
        let path = temp_csv_path("stalker-round-trip");

        csv_write_input_to_path(&path, &aliases).await?;
        let (_, written_aliases) = csv_read_inputs_from_path(InputType::StalkerBatch, &path).await?;
        let _ = std::fs::remove_file(path);

        let stalker = written_aliases
            .first()
            .ok_or_else(|| std::io::Error::other("missing persisted Stalker alias"))?
            .stalker
            .as_ref()
            .ok_or_else(|| std::io::Error::other("missing persisted Stalker alias configuration"))?;
        assert_eq!(stalker.auth_mode, StalkerAuthMode::MacOnly);
        assert_eq!(stalker.mag_preset, StalkerMagPreset::Mag254Strict);
        assert_eq!(stalker.endpoint_preference, StalkerEndpointPreference::Portal);
        assert_eq!(stalker.device.as_ref().and_then(|device| device.mac_address.as_deref()), Some("00:1A:79:12:34:56"));
        Ok(())
    }

    #[test]
    fn test_read_inputs_xtream_as_m3u() {
        let reader = file_reader(Cursor::new(XTREAM_BATCH));
        let result = csv_read_inputs_from_reader(InputType::M3uBatch, reader);
        assert!(result.is_ok());
        let aliases = result.unwrap();
        assert!(!aliases.is_empty());
        for config in aliases {
            assert!(config.url.contains("username"));
        }
    }

    #[test]
    fn test_read_inputs_m3u_as_m3u() {
        let reader = file_reader(Cursor::new(M3U_BATCH));
        let result = csv_read_inputs_from_reader(InputType::M3uBatch, reader);
        assert!(result.is_ok());
        let aliases = result.unwrap();
        assert!(!aliases.is_empty());
        for config in aliases {
            assert!(config.url.contains("username"));
        }
    }

    #[test]
    fn test_read_inputs_xtream_as_xtream() {
        let reader = file_reader(Cursor::new(XTREAM_BATCH));
        let result = csv_read_inputs_from_reader(InputType::XtreamBatch, reader);
        assert!(result.is_ok());
        let aliases = result.unwrap();
        assert!(!aliases.is_empty());
        for config in aliases {
            assert!(!config.url.contains("username"));
        }
    }

    #[test]
    fn test_read_inputs_m3u_as_xtream() {
        let reader = file_reader(Cursor::new(M3U_BATCH));
        let result = csv_read_inputs_from_reader(InputType::XtreamBatch, reader);
        assert!(result.is_ok());
        let aliases = result.unwrap();
        assert!(!aliases.is_empty());
        for config in aliases {
            assert!(!config.url.contains("username"));
        }
    }

    #[test]
    fn test_read_inputs_xtream_as_stalker_batch() {
        let reader = file_reader(Cursor::new(XTREAM_BATCH));
        let aliases = csv_read_inputs_from_reader(InputType::StalkerBatch, reader).expect("stalker batch aliases");
        assert_eq!(aliases.len(), 2);
        // Aliases must NOT materialize a stalker block of their own — they
        // inherit the parent input's stalker configuration in `as_input`.
        assert!(aliases.iter().all(|alias| alias.stalker.is_none()));
        assert!(aliases.iter().all(|alias| !alias.url.contains("username")));
    }

    fn temp_csv_path(test_name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tuliprox-{test_name}-{}-{}.csv",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("system time").as_nanos()
        ))
    }

    #[tokio::test]
    async fn csv_sort_by_exp_date_newest_first_keeps_existing_rows() {
        let path = temp_csv_path("csv-sort-newest-first");
        std::fs::write(
            &path,
            "#name;username;password;url;max_connections;exp_date\n\
old;old-user;old-pass;http://old.example;1;2026-01-01 00:00:00\n\
new;new-user;new-pass;http://new.example;1;2027-01-01 00:00:00\n\
missing;missing-user;missing-pass;http://missing.example;1;\n",
        )
        .expect("write csv fixture");

        let changed =
            csv_patch_batch_sort_by_exp_date(InputType::XtreamBatch, &path, AliasExpDateSortOrder::NewestFirst)
                .await
                .expect("sort succeeds");

        assert!(changed);
        let (_, aliases) = csv_read_inputs_from_path(InputType::XtreamBatch, &path).await.expect("read sorted csv");
        assert_eq!(aliases.len(), 3);
        assert_eq!(aliases[0].name.as_ref(), "new");
        assert_eq!(aliases[1].name.as_ref(), "old");
        assert_eq!(aliases[2].name.as_ref(), "missing");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn batch_expiry_update_returns_stable_account_key() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let csv_path = dir.path().join("aliases.csv");
        let backup_dir = dir.path().join("backups");
        tokio::fs::write(&csv_path, XTREAM_BATCH).await?;
        let update = BatchExpDateUpdate {
            account_key: "provider/input_1".to_string(),
            account_name: "input_1".into(),
            exp_date: 2_000_000_000,
            disable: true,
        };

        let (changed, updated) = csv_patch_batch_update_exp_dates(
            InputType::XtreamBatch,
            &csv_path,
            &[update],
            backup_dir.to_string_lossy().as_ref(),
        )
        .await?;

        assert!(changed);
        assert_eq!(updated, vec!["provider/input_1"]);
        let (_, aliases) = csv_read_inputs_from_path(InputType::XtreamBatch, &csv_path).await?;
        assert!(!aliases[0].enabled);
        Ok(())
    }

    #[tokio::test]
    async fn batch_expiry_update_preserves_raw_csv_fields() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let csv_path = dir.path().join("aliases.csv");
        let backup_dir = dir.path().join("backups");
        let raw = "# retained comment\n\
#name;username;password;url;enabled;max_connections;priority;exp_date;custom\n\
input_1;${env:PATH};${env:XTREAM_PASSWORD};http://provider.tv;1;1;0;2028-11-23 13:12:34;keep-me;trailing-a;trailing-b\n";
        tokio::fs::write(&csv_path, raw).await?;
        let update = BatchExpDateUpdate {
            account_key: "provider/input_1".to_string(),
            account_name: "input_1".into(),
            exp_date: 2_000_000_000,
            disable: true,
        };

        csv_patch_batch_update_exp_dates(
            InputType::XtreamBatch,
            &csv_path,
            &[update],
            backup_dir.to_string_lossy().as_ref(),
        )
        .await?;

        let written = tokio::fs::read_to_string(csv_path).await?;
        assert!(written.contains("# retained comment"));
        assert!(written.contains("${env:PATH};${env:XTREAM_PASSWORD}"));
        assert!(written.contains(";keep-me;trailing-a;trailing-b"));
        Ok(())
    }

    #[tokio::test]
    async fn batch_expiry_header_extension_preserves_trailing_fields() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let csv_path = dir.path().join("aliases.csv");
        let backup_dir = dir.path().join("backups");
        let raw = "#name;username;password;url;max_connections;priority;custom\n\
input_1;user;password;http://provider.tv;1;0;keep-me;old-enabled;old-expiry;trailing\n";
        tokio::fs::write(&csv_path, raw).await?;
        let update = BatchExpDateUpdate {
            account_key: "provider/input_1".to_string(),
            account_name: "input_1".into(),
            exp_date: 2_000_000_000,
            disable: true,
        };

        csv_patch_batch_update_exp_dates(
            InputType::XtreamBatch,
            &csv_path,
            &[update],
            backup_dir.to_string_lossy().as_ref(),
        )
        .await?;

        let written = tokio::fs::read_to_string(csv_path).await?;
        assert!(written.contains(";trailing"));
        Ok(())
    }

    #[tokio::test]
    async fn unmatched_batch_expiry_update_creates_no_backup() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let csv_path = dir.path().join("aliases.csv");
        let backup_dir = dir.path().join("backups");
        tokio::fs::write(&csv_path, XTREAM_BATCH).await?;
        let update = BatchExpDateUpdate {
            account_key: "provider/missing".to_string(),
            account_name: "missing".into(),
            exp_date: 2_000_000_000,
            disable: true,
        };

        let (changed, updated) = csv_patch_batch_update_exp_dates(
            InputType::XtreamBatch,
            &csv_path,
            &[update],
            backup_dir.to_string_lossy().as_ref(),
        )
        .await?;

        assert!(!changed);
        assert!(updated.is_empty());
        assert!(!backup_dir.exists());
        Ok(())
    }

    #[tokio::test]
    async fn alias_backups_do_not_overwrite_each_other() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let csv_path = dir.path().join("aliases.csv");
        let backup_dir = dir.path().join("backups");
        tokio::fs::write(&csv_path, XTREAM_BATCH).await?;

        let backup_dir_str = backup_dir.to_string_lossy();
        let backups = (0..16).map(|_| csv_backup_file(&csv_path, backup_dir_str.as_ref()));
        futures::future::try_join_all(backups).await?;

        let mut entries = tokio::fs::read_dir(&backup_dir).await?;
        let mut count = 0;
        while entries.next_entry().await?.is_some() {
            count += 1;
        }
        assert_eq!(count, 16);
        Ok(())
    }

    #[test]
    fn alias_csv_temp_paths_are_unique_and_stay_next_to_destination() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let csv_path = dir.path().join("aliases.csv");
        let first = csv_temp_path(&csv_path)?;
        let second = csv_temp_path(&csv_path)?;

        assert_eq!(first.parent(), csv_path.parent());
        assert_eq!(second.parent(), csv_path.parent());
        assert_ne!(first, second);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn alias_csv_rewrite_preserves_file_mode() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let csv_path = dir.path().join("aliases.csv");
        tokio::fs::write(&csv_path, XTREAM_BATCH).await?;
        tokio::fs::set_permissions(&csv_path, std::fs::Permissions::from_mode(0o640)).await?;

        csv_write_input_to_path(&csv_path, &[]).await?;

        assert_eq!(tokio::fs::metadata(csv_path).await?.permissions().mode() & 0o777, 0o640);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn alias_csv_rewrite_preserves_posix_acl() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let csv_path = dir.path().join("aliases.csv");
        tokio::fs::write(&csv_path, XTREAM_BATCH).await?;
        let status = std::process::Command::new("setfacl").args(["-m", "u:12345:r", csv_path.to_string_lossy().as_ref()]).status();
        let Ok(status) = status else { return Ok(()) };
        if !status.success() {
            return Ok(());
        }
        let before = std::process::Command::new("getfacl").args(["-cp", csv_path.to_string_lossy().as_ref()]).output()?;

        csv_write_input_to_path(&csv_path, &[]).await?;

        let after = std::process::Command::new("getfacl").args(["-cp", csv_path.to_string_lossy().as_ref()]).output()?;
        assert_eq!(after.stdout, before.stdout);
        Ok(())
    }
}
