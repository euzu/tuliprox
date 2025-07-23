use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io::{BufWriter, Error, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flate2::read::{GzDecoder, ZlibDecoder};
use futures::StreamExt;
use log::{debug, error, log_enabled, trace, Level};
use reqwest::header::CONTENT_ENCODING;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

use shared::error::create_tuliprox_error_result;
use shared::error::{str_to_io_error, TuliproxError, TuliproxErrorKind};
use shared::model::{InputFetchMethod, DEFAULT_USER_AGENT};
use crate::model::{format_elapsed_time, AppConfig};
use crate::model::{ConfigInput};
use crate::repository::storage::{get_input_storage_path};
use crate::repository::storage_const;
use crate::utils::compression::compression_utils::{is_deflate, is_gzip};
use crate::utils::{debug_if_enabled};
use shared::utils::{filter_request_header, sanitize_sensitive_info, short_hash, ENCODING_DEFLATE, ENCODING_GZIP};
use crate::utils::{get_file_path, persist_file};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MimeCategory {
    Unknown,
    Video,
    M3U8,
    Image,
    Json,
    Xml,
    Text,
    Unclassified,
}

pub fn classify_content_type(headers: &[(String, String)]) -> MimeCategory {
    headers.iter()
        .find_map(|(k, v)| {
            (k == axum::http::header::CONTENT_TYPE.as_str()).then_some(v)
        })
        .map_or(MimeCategory::Unknown, |v| match v.to_lowercase().as_str() {
            v if v.starts_with("video/") || v == "application/octet-stream" => MimeCategory::Video,
            v if v.contains("mpegurl") => MimeCategory::M3U8,
            v if v.starts_with("image/") => MimeCategory::Image,
            v if v.starts_with("application/json") || v.ends_with("+json") => MimeCategory::Json,
            v if v.starts_with("application/xml") || v.ends_with("+xml") || v == "text/xml" => MimeCategory::Xml,
            v if v.starts_with("text/") => MimeCategory::Text,
            _ => MimeCategory::Unclassified,
        })
}

pub async fn get_input_epg_content_as_file(client: Arc<reqwest::Client>, input: &ConfigInput, working_dir: &str, url_str: &str, persist_filepath: Option<PathBuf>) -> Result<PathBuf, TuliproxError> {
    debug_if_enabled!("getting input epg content working_dir: {}, url: {}", working_dir, sanitize_sensitive_info(url_str));
    if url_str.parse::<url::Url>().is_ok() {
        match download_epg_content_as_file(client, input, url_str, working_dir, persist_filepath).await {
            Ok(content) => Ok(content),
            Err(e) => {
                error!("cant download input epg url: {}  => {}", sanitize_sensitive_info(url_str), sanitize_sensitive_info(e.to_string().as_str()));
                create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed to download")
            }
        }
    } else {
        let result = match get_file_path(working_dir, Some(PathBuf::from(url_str))) {
            Some(filepath) => {
                if filepath.exists() {
                    if let Some(persist_file_value) = persist_filepath {
                        let to_file = &persist_file_value;
                        match fs::copy(&filepath, to_file) {
                            Ok(_) => {}
                            Err(e) => {
                                error!("cant persist to: {}  => {}", to_file.to_str().unwrap_or("?"), e);
                                return create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed to persist: {}  => {}", to_file.to_str().unwrap_or("?"), e);
                            }
                        }
                    }

                    if filepath.exists() {
                        Some(filepath)
                    } else {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed: file does not exists {filepath:?}");
                    }
                } else {
                    None
                }
            }
            None => None
        };

        result.map_or_else(|| {
            let msg = format!("cant read input url: {}", sanitize_sensitive_info(url_str));
            error!("{msg}");
            create_tuliprox_error_result!(TuliproxErrorKind::Notify, "{msg}")
        }, Ok)
    }
}


pub async fn get_input_text_content(client: Arc<reqwest::Client>, input: &ConfigInput, working_dir: &str, url_str: &str, persist_filepath: Option<PathBuf>) -> Result<String, TuliproxError> {
    debug_if_enabled!("getting input text content working_dir: {}, url: {}", working_dir, sanitize_sensitive_info(url_str));

    if url_str.parse::<url::Url>().is_ok() {
        match download_text_content(client, input, url_str, persist_filepath).await {
            Ok((content, _response_url)) => Ok(content),
            Err(e) => {
                error!("cant download input url: {}  => {}", sanitize_sensitive_info(url_str), sanitize_sensitive_info(e.to_string().as_str()));
                create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed to download")
            }
        }
    } else {
        let result = match get_file_path(working_dir, Some(PathBuf::from(url_str))) {
            Some(filepath) => {
                if filepath.exists() {
                    if let Some(persist_file_value) = persist_filepath {
                        let to_file = &persist_file_value;
                        match fs::copy(&filepath, to_file) {
                            Ok(_) => {}
                            Err(e) => {
                                error!("cant persist to: {}  => {}", to_file.to_str().unwrap_or("?"), e);
                                return create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed to persist: {}  => {}", to_file.to_str().unwrap_or("?"), e);
                            }
                        }
                    }

                    match get_local_file_content(&filepath) {
                        Ok(content) => Some(content),
                        Err(err) => {
                            return create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed : {}", err);
                        }
                    }
                } else {
                    None
                }
            }
            None => None
        };
        result.map_or_else(|| {
            let msg = format!("cant read input url: {}", sanitize_sensitive_info(url_str));
            error!("{msg}");
            create_tuliprox_error_result!(TuliproxErrorKind::Notify, "{msg}")
        }, Ok)
    }
}

pub fn get_client_request<S: ::std::hash::BuildHasher + Default>(client: &Arc<reqwest::Client>,
                                                                 method: InputFetchMethod,
                                                                 headers: Option<&HashMap<String, String, S>>,
                                                                 url: &Url,
                                                                 custom_headers: Option<&HashMap<String, Vec<u8>, S>>) -> reqwest::RequestBuilder {
    let request = match method {
        InputFetchMethod::GET => client.get(url.clone()),
        InputFetchMethod::POST => {
            // let base_url = url[..url::Position::BeforePath].to_string() + url.path();
            let mut params: HashMap<String, String, S> = HashMap::default();
            for (key, value) in url.query_pairs() {
                params.insert(key.to_string(), value.to_string());
            }
            // we could cut the params but we leave them as query and add them as form.
            client.post(url.clone()).form(&params)
        }
    };
    let headers = get_request_headers(headers, custom_headers);
    request.headers(headers)
}

pub fn get_request_headers<S: ::std::hash::BuildHasher + Default>(request_headers: Option<&HashMap<String, String, S>>, custom_headers: Option<&HashMap<String, Vec<u8>, S>>) -> HeaderMap {
    let mut headers = HeaderMap::default();
    if let Some(req_headers) = request_headers {
        for (key, value) in req_headers {
            if let (Ok(key), Ok(value)) = (HeaderName::from_bytes(key.as_bytes()), HeaderValue::from_bytes(value.as_bytes())) {
                if filter_request_header(key.as_str()) {
                    headers.insert(key, value);
                }
            }
        }
    }
    if let Some(custom) = custom_headers {
        let header_keys: HashSet<String> = headers.keys().map(|k| k.as_str().to_lowercase()).collect();
        for (key, value) in custom {
            let key_lc = key.to_lowercase();
            if filter_request_header(key_lc.as_str()) {
                if header_keys.contains(key_lc.as_str()) {
                    // debug_if_enabled!("Ignoring request header '{}={}'", key_lc, String::from_utf8_lossy(value));
                } else if let (Ok(key), Ok(value)) = (HeaderName::from_bytes(key.as_bytes()), HeaderValue::from_bytes(value)) {
                    headers.insert(key, value);
                }
            }
        }
    }
    if log_enabled!(Level::Trace) {
        let he: HashMap<String, String> = headers.iter().map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string())).collect();
        if !he.is_empty() {
            trace!("Request headers {he:?}");
        }
    }
    if !headers.contains_key(axum::http::header::USER_AGENT) {
        headers.insert(axum::http::header::USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
    }
    headers
}

pub fn get_local_file_content(file_path: &PathBuf) -> Result<String, Error> {
    // Check if the file is accessible
    if file_path.exists() && file_path.is_file() {
        if let Ok(content) = fs::read(file_path) {
            if content.len() >= 2 && is_gzip(&content[0..2]) {
                let mut decoder = GzDecoder::new(&content[..]);
                let mut decode_buffer = String::new();
                return match decoder.read_to_string(&mut decode_buffer) {
                    Ok(_) => Ok(decode_buffer),
                    Err(err) => Err(str_to_io_error(&format!("failed to decode gzip content {err}")))
                };
            }
            return Ok(String::from_utf8_lossy(&content).parse().unwrap_or_default());
        }
    }
    let file_str = file_path.to_str().unwrap_or("?");
    Err(Error::new(ErrorKind::InvalidData, format!("Cant find file {file_str}")))
}


async fn get_remote_content_as_file(client: Arc<reqwest::Client>, input: &ConfigInput, url: &Url, file_path: &Path) -> Result<PathBuf, std::io::Error> {
    let start_time = Instant::now();
    let request = get_client_request(&client, input.method, Some(&input.headers), url, None);
    match request.send().await {
        Ok(response) => {
            if response.status().is_success() {
                // Open a file in write mode
                let mut file = BufWriter::with_capacity(8192, File::create(file_path)?);
                // Stream the response body in chunks
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            file.write_all(&bytes)?;
                        }
                        Err(err) => {
                            return Err(str_to_io_error(&format!("Failed to read chunk: {err}")));
                        }
                    }
                }

                file.flush()?;
                let elapsed = start_time.elapsed().as_secs();
                debug!("File downloaded successfully to {}, took:{}", file_path.display(), format_elapsed_time(elapsed));
                Ok(file_path.to_path_buf())
            } else {
                Err(str_to_io_error(&format!("Request failed with status {} {}", response.status(), sanitize_sensitive_info(url.as_str()))))
            }
        }
        Err(err) => Err(str_to_io_error(&format!("Request failed: {} {err}", sanitize_sensitive_info(url.as_str())))),
    }
}

async fn get_remote_content(client: Arc<reqwest::Client>, input: &ConfigInput, url: &Url) -> Result<(String, String), Error> {
    let start_time = Instant::now();
    let request = get_client_request(&client, input.method, Some(&input.headers), url, None);
    match request.send().await {
        Ok(response) => {
            let is_success = response.status().is_success();
            if is_success {
                let response_url = response.url().to_string();
                let headers = response.headers();
                debug!("{headers:?}");
                let header_value = headers.get(CONTENT_ENCODING);
                let mut encoding = header_value.and_then(|encoding_header| encoding_header.to_str().map_or(None, |value| Some(value.to_string())));
                match response.bytes().await {
                    Ok(bytes) => {
                        if bytes.len() >= 2 {
                            if is_gzip(&bytes[0..2]) {
                                encoding = Some(ENCODING_GZIP.to_string());
                            } else if is_deflate(&bytes[0..2]) {
                                encoding = Some(ENCODING_DEFLATE.to_string());
                            }
                        }

                        let mut decode_buffer = String::new();
                        if let Some(encoding_type) = encoding {
                            match encoding_type.as_str() {
                                ENCODING_GZIP => {
                                    let mut decoder = GzDecoder::new(&bytes[..]);
                                    match decoder.read_to_string(&mut decode_buffer) {
                                        Ok(_) => {}
                                        Err(err) => return Err(str_to_io_error(&format!("failed to decode gzip content {err}")))
                                    }
                                }
                                ENCODING_DEFLATE => {
                                    let mut decoder = ZlibDecoder::new(&bytes[..]);
                                    match decoder.read_to_string(&mut decode_buffer) {
                                        Ok(_) => {}
                                        Err(err) => return Err(str_to_io_error(&format!("failed to decode zlib content {err}")))
                                    }
                                }
                                _ => {}
                            }
                        }

                        if decode_buffer.is_empty() {
                            let content_bytes = bytes.to_vec();
                            match String::from_utf8(content_bytes) {
                                Ok(decoded_content) => {
                                    debug_if_enabled!("Request took:{} {}", format_elapsed_time(start_time.elapsed().as_secs()), sanitize_sensitive_info(url.as_str()));
                                    Ok((decoded_content, response_url))
                                }
                                Err(err) => {
                                    println!("{err:?}");
                                    Err(str_to_io_error(&format!("failed to plain text content {err}")))
                                }
                            }
                        } else {
                            debug_if_enabled!("Request took:{},  {}", format_elapsed_time(start_time.elapsed().as_secs()), sanitize_sensitive_info(url.as_str()));
                            Ok((decode_buffer, response_url))
                        }
                    }
                    Err(err) => Err(str_to_io_error(&format!("failed to read response {} {err}", sanitize_sensitive_info(url.as_str()))))
                }
            } else {
                Err(str_to_io_error(&format!("Request failed with status {} {}", response.status(), sanitize_sensitive_info(url.as_str()))))
            }
        }
        Err(err) => Err(str_to_io_error(&format!("Request failed {} {err}", sanitize_sensitive_info(url.as_str()))))
    }
}

async fn download_epg_content_as_file(client: Arc<reqwest::Client>, input: &ConfigInput, url_str: &str, working_dir: &str, persist_filepath: Option<PathBuf>) -> Result<PathBuf, Error> {
    if let Ok(url) = url_str.parse::<url::Url>() {
        if url.scheme() == "file" {
            url.to_file_path().map_or_else(|()| Err(Error::new(ErrorKind::Unsupported, format!("Unknown file {}", sanitize_sensitive_info(url_str)))), |file_path| if file_path.exists() {
                Ok(file_path)
            } else {
                Err(Error::new(ErrorKind::NotFound, format!("Unknown file {}", file_path.display())))
            })
        } else {
            let file_path = persist_filepath.map_or_else(|| match get_input_storage_path(&input.name, working_dir) {
                Ok(download_path) => {
                    Ok(download_path.join(format!("{}_{}", short_hash(url_str), storage_const::FILE_EPG)))
                }
                Err(err) => Err(err)
            }, Ok);
            match file_path {
                Ok(persist_path) => get_remote_content_as_file(client, input, &url, &persist_path).await,
                Err(err) => Err(err)
            }
        }
    } else {
        Err(std::io::Error::new(ErrorKind::Unsupported, format!("Malformed URL {}", sanitize_sensitive_info(url_str))))
    }
}


pub async fn download_text_content(client: Arc<reqwest::Client>, input: &ConfigInput, url_str: &str, persist_filepath: Option<PathBuf>) -> Result<(String, String), Error> {
    if let Ok(url) = url_str.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            url.to_file_path().map_or_else(|()| Err(str_to_io_error(&format!("Unknown file {}", sanitize_sensitive_info(url_str)))), |file_path|
                get_local_file_content(&file_path).map(|c| (c, url.to_string())),
            )
        } else {
            get_remote_content(client, input, &url).await
        };
        match result {
            Ok((content, response_url)) => {
                if persist_filepath.is_some() {
                    persist_file(persist_filepath, &content);
                }
                Ok((content, response_url))
            }
            Err(err) => Err(err)
        }
    } else {
        Err(str_to_io_error(&format!("Malformed URL {}", sanitize_sensitive_info(url_str))))
    }
}

async fn download_json_content(client: Arc<reqwest::Client>, input: &ConfigInput, url: &str, persist_filepath: Option<PathBuf>) -> Result<serde_json::Value, Error> {
    debug_if_enabled!("downloading json content from {}", sanitize_sensitive_info(url));
    match download_text_content(client, input, url, persist_filepath).await {
        Ok((content, _response_url)) => {
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(value) => Ok(value),
                Err(err) => Err(str_to_io_error(&format!("Failed to parse json {err}")))
            }
        }
        Err(err) => Err(err)
    }
}

pub async fn get_input_json_content(client: Arc<reqwest::Client>, input: &ConfigInput, url: &str, persist_filepath: Option<PathBuf>) -> Result<serde_json::Value, TuliproxError> {
    match download_json_content(client, input, url, persist_filepath).await {
        Ok(content) => Ok(content),
        Err(e) => create_tuliprox_error_result!(TuliproxErrorKind::Notify, "cant download input url: {}  => {}", sanitize_sensitive_info(url), sanitize_sensitive_info(e.to_string().as_str()))
    }
}


pub fn create_client(cfg: &AppConfig) -> reqwest::ClientBuilder {
    let mut client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10);

    let config = cfg.config.load();
    if let Some(proxy_cfg) = config.proxy.as_ref() {
        let proxy = match reqwest::Proxy::all(&proxy_cfg.url) {
            Ok(proxy) => {
                if let (Some(username), Some(password)) = (&proxy_cfg.username, &proxy_cfg.password) {
                    Some(proxy.basic_auth(username, password))
                } else {
                    Some(proxy)
                }
            }
            Err(err) => {
                error!("Failed to create proxy {}, {err}", &proxy_cfg.url);
                None
            }
        };
        client = if let Some(prxy) = proxy {
            client.proxy(prxy)
        } else {
            client
        };
    }

    if let Some(rp_config) = config.reverse_proxy.as_ref() {
        if rp_config.disable_referer_header {
            client = client.referer(false);
        }
    }

    client
}

#[cfg(test)]
mod tests {
    use shared::utils::{get_base_url_from_str, replace_url_extension, sanitize_sensitive_info};

    #[test]
    fn test_url_mask() {
        // Replace with "***"
        let query = "https://bubblegum.tv/live/username/password/2344";
        let masked = sanitize_sensitive_info(query);
        println!("{masked}");
    }

    #[test]
    fn test_replace_ext() {
        let tests = [
            ("http://hello.world.com", "http://hello.world.com"),
            ("http://hello.world.com/123", "http://hello.world.com/123.mp4"),
            ("http://hello.world.com/123.ts?hello=world", "http://hello.world.com/123.mp4?hello=world"),
            ("http://hello.world.com/123?hello=world", "http://hello.world.com/123.mp4?hello=world"),
            ("http://hello.world.com/123#hello=world", "http://hello.world.com/123.mp4#hello=world")
        ];

        for (test, expect) in &tests {
            assert_eq!(replace_url_extension(test, ".mp4"), *expect);
        }
    }

    #[test]
    fn tes_base_url() {
        let url = "http://my.provider.com:8080/xmltv?username=hello";
        let expected = "http://my.provider.com:8080";
        assert_eq!(get_base_url_from_str(url).unwrap(), expected);
    }
}
