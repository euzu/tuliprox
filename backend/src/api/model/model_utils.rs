use reqwest::{StatusCode};
use std::collections::{HashSet};
use std::str::FromStr;
use reqwest::header::HeaderMap;
use shared::utils::{filter_response_header};

pub fn get_response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let response_headers: Vec<(String, String)> = headers.iter()
        .filter(|(key, _)| filter_response_header(key.as_str()))
        .filter_map(|(key, value)| {
            value.to_str().ok().map(|v| (key.to_string(), v.to_string()))
        })
        .collect();
    response_headers
}

pub fn get_stream_response_with_headers(custom: Option<(Vec<(String, String)>, StatusCode)>) ->  (axum::http::StatusCode, axum::http::HeaderMap) {
    let mut headers = HeaderMap::new();
    let mut added_headers: HashSet<String> = HashSet::new();
    let mut status = StatusCode::OK;

    if let Some((custom_headers, status_code)) = custom {
        status = status_code;
        for (key, value) in custom_headers {
            if let (Ok(name), Ok(val)) = (axum::http::HeaderName::from_str(&key), axum::http::HeaderValue::from_str(&value)) {
                headers.insert(name.clone(), val);
                added_headers.insert(key);
            }
        }
    }

    let default_headers = vec![
        ("content-type", "application/octet-stream"),
    ];

    for (key, value) in default_headers {
        if !added_headers.contains(key) {
            if let (Ok(name), Ok(val)) = (axum::http::HeaderName::from_str(key), axum::http::HeaderValue::from_str(value)) {
                headers.insert(name, val);
            }
        }
    }

    if let Ok(date_header) = axum::http::HeaderValue::from_str(&chrono::Utc::now().to_rfc2822()) {
        headers.insert(axum::http::HeaderName::from_static("date"), date_header);
    }

    (status, headers)
}