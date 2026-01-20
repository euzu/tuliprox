use crate::model::is_input_expired;
use crate::utils::request::get_local_csv_file_content;
use crate::utils::EnvResolvingReader;
use crate::utils::{file_reader, resolve_relative_path};
use futures::TryFutureExt;
use log::{error, warn};
use shared::error::{string_to_io_error, to_io_error, TuliproxError};
use shared::info_err;
use shared::model::{ConfigInputAliasDto, InputType};
use shared::utils::{get_credentials_from_url, get_credentials_from_url_str, parse_timestamp, sanitize_sensitive_info, Internable};
use std::io;
use std::io::{BufRead, Cursor, Error};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use url::Url;

const CSV_SEPARATOR: char = ';';
const HEADER_PREFIX: char = '#';
const FIELD_MAX_CON: &str = "max_connections";
const FIELD_PRIO: &str = "priority";
const FIELD_URL: &str = "url";
const FIELD_NAME: &str = "name";
const FIELD_USERNAME: &str = "username";
const FIELD_PASSWORD: &str = "password";
const FIELD_EXP_DATE: &str = "exp_date";
const FIELD_UNKNOWN: &str = "?";
const DEFAULT_COLUMNS: &[&str] = &[
    FIELD_URL,
    FIELD_MAX_CON,
    FIELD_PRIO,
    FIELD_NAME,
    FIELD_USERNAME,
    FIELD_PASSWORD,
    FIELD_EXP_DATE,
];
const CSV_EXTENSION: &str = ".csv";

pub fn is_csv_file(url: &str) -> bool {
    url.to_lowercase().ends_with(CSV_EXTENSION)
}


fn build_m3u_url(
    base: &Url,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<Url, url::ParseError> {
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
        match Url::parse(alias.url.as_str()) {
            Ok(url) => {
                let (username, password) = get_credentials_from_url(&url);
                if username.is_none() || password.is_none() {
                    // xtream url
                    if input_type == InputType::XtreamBatch {
                        alias.url = url.origin().ascii_serialization();
                    } else if input_type == InputType::M3uBatch
                        && alias.username.is_some()
                        && alias.password.is_some()
                    {
                        match build_m3u_url(
                            &url,
                            alias.username.as_deref(),
                            alias.password.as_deref()) {
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
    }
}

fn csv_assign_config_input_column(
    config_input: &mut ConfigInputAliasDto,
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
        InputType::Library => InputType::Library,
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
        };

        let columns: Vec<&str> = line.split(CSV_SEPARATOR).collect();
        for (&header, &value) in default_columns.iter().zip(columns.iter()) {
            if let Err(err) = csv_assign_config_input_column(&mut config_input, header, value) {
                error!("Could not parse input line: {} err: {err}", line_idx+1);
            }
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
    match get_local_csv_file_content(file_path).await {
        Ok(content) => Ok((
            file_path.to_path_buf(),
            csv_read_inputs_from_reader(
                input_type,
                EnvResolvingReader::new(file_reader(Cursor::new(content))),
            )?,
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
    let raw_path = Path::new(file_uri);
    if raw_path.is_absolute() {
        return Ok(raw_path.to_path_buf());
    }
    if let Ok(url) = file_uri.parse::<Url>() {
        if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(path) => Ok(path),
                Err(()) => Err(string_to_io_error(format!("Could not open {file_uri}"))),
            }
        } else {
            Err(string_to_io_error(format!(
                "Only file:// is supported {file_uri}"
            )))
        }
    } else {
        resolve_relative_path(file_uri)
    }
}

async fn csv_write_input_to_path(
    file_path: &Path,
    aliases: &[ConfigInputAliasDto],
) -> Result<(), Error> {
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
        content.push_str(&alias.max_connections.to_string());
        content.push(CSV_SEPARATOR);
        content.push_str(&alias.priority.to_string());
        content.push(CSV_SEPARATOR);
        if let Some(exp) = alias.exp_date {
            content.push_str(
                &shared::utils::unix_ts_to_str_with_format(exp, "%Y-%m-%d %H:%M:%S")
                    .unwrap_or_default(),
            );
        }
        content.push('\n');
    }

    tokio::fs::write(file_path, content)
        .await
        .map_err(to_io_error)
}

pub async fn csv_write_inputs(
    file_uri: &str,
    aliases: &[ConfigInputAliasDto],
) -> Result<(), Error> {
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
        .map_err(|err| info_err!("{err}"))
        .await?;


    let url = if input_type == InputType::M3uBatch {
        let base = Url::parse(base_url).map_err(|e| info_err!("{e}"))?;
        build_m3u_url(&base, Some(username), Some(password))
            .map_err(|e| info_err!("{e}"))?
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
    };
    aliases.push(alias);

    csv_write_input_to_path(&file_path, &aliases)
        .map_err(|err| info_err!("{err}"))
        .await?;
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
        .map_err(|err| info_err!("{err}"))
        .await?;
    for alias in &mut aliases {
        if &alias.name == account_name
            || (alias.username == Some(username.to_string())
            && alias.password == Some(password.to_string()))
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
            .map_err(|err| info_err!("{err}"))
            .await?;
    } else {
        warn!("panel_api: could not find batch csv row for account {account_name}");
    }
    Ok(())
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
        .map_err(|err| info_err!("{err}"))
        .await?;

    for alias in &mut aliases {
        let mut is_match = &alias.name == account_name;
        if !is_match {
            is_match = alias.username.as_deref() == Some(old_username)
                && alias.password.as_deref() == Some(old_password);
        }
        if !is_match {
            is_match = alias.username.as_deref() == Some(new_username)
                && alias.password.as_deref() == Some(new_password);
        }

        if !is_match {
            if let (Some(u), Some(p)) = get_credentials_from_url_str(&alias.url) {
                is_match = (u == old_username && p == old_password)
                    || (u == new_username && p == new_password);
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
                let mut pairs: Vec<(String, String)> = url
                    .query_pairs()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
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
            .map_err(|err| info_err!("{err}"))
            .await?;
    } else {
        warn!("panel_api: could not find batch csv row to update credentials for account {account_name}");
    }
    Ok(())
}

pub async fn csv_patch_batch_remove_expired(
    input_type: InputType,
    csv_path: &Path,
) -> Result<bool, TuliproxError> {
    let (file_path, mut aliases) = csv_read_inputs_from_path(input_type, csv_path)
        .map_err(|err| info_err!("{err}"))
        .await?;
    let before_len = aliases.len();
    aliases.retain(|alias| !is_input_expired(alias.exp_date));
    let changed = before_len != aliases.len();
    if changed {
        csv_write_input_to_path(&file_path, &aliases)
            .map_err(|err| info_err!("{err}"))
            .await?;
    }
    Ok(changed)
}

pub async fn csv_patch_batch_sort_by_exp_date(
    input_type: InputType,
    csv_path: &Path,
) -> Result<bool, TuliproxError> {
    let (file_path, mut aliases) = csv_read_inputs_from_path(input_type, csv_path)
        .map_err(|err| info_err!("{err}"))
        .await?;
    if aliases.len() < 2 {
        return Ok(false);
    }
    let mut sorted = aliases.clone();
    sorted.sort_by(|a, b| {
        let a_ts = a.exp_date.unwrap_or(i64::MAX);
        let b_ts = b.exp_date.unwrap_or(i64::MAX);
        a_ts.cmp(&b_ts).then_with(|| a.name.cmp(&b.name))
    });
    if sorted == aliases {
        return Ok(false);
    }
    aliases = sorted;
    csv_write_input_to_path(&file_path, &aliases)
        .map_err(|err| info_err!("{err}"))
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use crate::repository::csv_read_inputs_from_reader;
    use crate::utils::{file_reader, resolve_env_var};
    use shared::model::InputType;
    use std::io::Cursor;

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
}
