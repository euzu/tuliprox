use shared::error::{string_to_io_error, to_io_error};
use crate::utils::EnvResolvingReader;
use crate::utils::{file_reader, resolve_relative_path};
use log::error;
use std::io;
use std::io::{BufRead, Cursor, Error};
use std::path::PathBuf;
use url::Url;
use shared::concat_string;
use shared::model::{ConfigInputAliasDto, InputType};
use shared::utils::{get_credentials_from_url, parse_timestamp, trim_last_slash, Internable};
use crate::utils::request::get_local_file_content;

const CSV_SEPARATOR: char = ';';
const HEADER_PREFIX: char = '#';

pub fn is_csv_file(url: &str) -> bool {
    url.to_lowercase().ends_with(".csv")
}
const FIELD_MAX_CON: &str = "max_connections";
const FIELD_PRIO: &str = "priority";
const FIELD_URL: &str = "url";
const FIELD_NAME: &str = "name";
const FIELD_USERNAME: &str = "username";
const FIELD_PASSWORD: &str = "password";
const FIELD_EXP_DATE: &str = "exp_date";
const FIELD_UNKNOWN: &str = "?";
const DEFAULT_COLUMNS: &[&str] = &[FIELD_URL, FIELD_MAX_CON, FIELD_PRIO, FIELD_NAME, FIELD_USERNAME, FIELD_PASSWORD, FIELD_EXP_DATE];

fn csv_assign_mandatory_fields(alias: &mut ConfigInputAliasDto, input_type: InputType) {
    if !alias.url.is_empty() {
        match Url::parse(alias.url.as_str()) {
            Ok(url) => {
                let (username, password) = get_credentials_from_url(&url);
                if username.is_none() || password.is_none() {
                    // xtream url
                    if input_type == InputType::XtreamBatch {
                        alias.url = url.origin().ascii_serialization();
                    } else if input_type == InputType::M3uBatch && alias.username.is_some() && alias.password.is_some() {
                        alias.url = format!("{}/get.php?username={}&password={}&type=m3u_plus",
                                            trim_last_slash(&url.origin().ascii_serialization()),
                                            alias.username.as_deref().unwrap_or(""),
                                            alias.password.as_deref().unwrap_or("")
                        );
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
                        alias.name = concat_string!(domain[domain.len() - 2], "_", username).intern();
                    } else {
                        alias.name = username.intern();
                    }
                }
            }
            Err(_err) => {}
        }
    }
}

fn csv_assign_config_input_column(config_input: &mut ConfigInputAliasDto, header: &str, raw_value: &str) -> Result<(), io::Error> {
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

pub fn csv_read_inputs_from_reader(batch_input_type: InputType, reader: impl BufRead) -> Result<Vec<ConfigInputAliasDto>, Error> {
    let input_type = match batch_input_type {
        InputType::M3uBatch | InputType::M3u => InputType::M3uBatch,
        InputType::XtreamBatch | InputType::Xtream => InputType::XtreamBatch,
        InputType::Library => InputType::Library
    };
    let mut result = vec![];
    let mut default_columns = vec![];
    default_columns.extend_from_slice(DEFAULT_COLUMNS);
    let mut header_defined = false;
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        if line.starts_with(HEADER_PREFIX) {
            if !header_defined {
                header_defined = true;
                default_columns = line[1..].split(CSV_SEPARATOR).map(|s| {
                    match s {
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
                    }
                }).collect();
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
                error!("Could not parse input line: {line} err: {err}");
            }
        }
        csv_assign_mandatory_fields(&mut config_input, input_type);
        result.push(config_input);
    }
    Ok(result)
}


pub async fn csv_read_inputs(input_type: InputType, file_uri: &str) -> Result<(PathBuf, Vec<ConfigInputAliasDto>), Error> {
    let file_path = get_csv_file_path(file_uri)?;
    match get_local_file_content(&file_path).await {
        Ok(content) => {
            Ok((file_path, csv_read_inputs_from_reader(input_type, EnvResolvingReader::new(file_reader(Cursor::new(content))))?))
        }
        Err(err) => {
            Err(err)
        }
    }
}

pub fn get_csv_file_path(file_uri: &str) -> Result<PathBuf, Error> {
    if let Ok(url) = file_uri.parse::<Url>() {
        if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(path) => Ok(path),
                Err(()) => Err(string_to_io_error(format!("Could not open {file_uri}"))),
            }
        } else {
            Err(string_to_io_error(format!("Only file:// is supported {file_uri}")))
        }
    } else {
        resolve_relative_path(file_uri)
    }
}

pub async fn csv_write_inputs(file_uri: &str, aliases: &[ConfigInputAliasDto]) -> Result<(), Error> {
    let file_path = get_csv_file_path(file_uri)?;
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
        if let Some(exp) = alias.exp_date {
            content.push_str(&shared::utils::unix_ts_to_str_with_format(exp, "%Y-%m-%d %H:%M:%S").unwrap_or_default());
        }
        content.push('\n');
    }

    tokio::fs::write(file_path, content).await.map_err(to_io_error)
}

#[cfg(test)]
mod tests {
    use crate::utils::file::csv_input_reader::csv_read_inputs_from_reader;
    use crate::utils::{file_reader, resolve_env_var};
    use std::io::{Cursor};
    use shared::model::InputType;

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

    #[test]
    fn test_resolve() {
        let resolved = resolve_env_var("${env:HOME}");
        assert_eq!(resolved, std::env::var("HOME").unwrap());
    }
}
