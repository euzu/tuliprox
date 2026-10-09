use super::{
    PANEL_API_DEFAULT_RETRY_AFTER_SECS, PANEL_API_MAX_RETRY_AFTER_SECS, PANEL_API_REQUEST_TIMEOUT_SECS,
    PANEL_API_RETRY_ATTEMPTS,
};
use crate::{
    api::model::AppState,
    model::InputSource,
    utils::{debug_if_enabled, format_http_status, request},
};
use axum::http::{header, HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use log::warn;
use serde_json::Value;
use shared::{error::TuliproxError, utils::sanitize_sensitive_info};
use std::{collections::HashMap, time::Duration};
use url::Url;

pub(super) fn sanitize_panel_api_json_for_log(value: &Value, sanitize_sensitive: bool) -> Value {
    match value {
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| sanitize_panel_api_json_for_log(v, sanitize_sensitive)).collect())
        }
        Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                if sanitize_sensitive {
                    if k.eq_ignore_ascii_case("api_key")
                        || k.eq_ignore_ascii_case("apikey")
                        || k.eq_ignore_ascii_case("token")
                    {
                        out.insert(k.clone(), Value::String("***".to_string()));
                        continue;
                    }
                    if k.eq_ignore_ascii_case("username") || k.eq_ignore_ascii_case("password") {
                        out.insert(k.clone(), Value::String("***".to_string()));
                        continue;
                    }
                }
                if k.eq_ignore_ascii_case("url") {
                    if let Some(s) = v.as_str() {
                        out.insert(k.clone(), Value::String(sanitize_sensitive_info(s).into_owned()));
                        continue;
                    }
                }
                out.insert(k.clone(), sanitize_panel_api_json_for_log(v, sanitize_sensitive));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

pub(super) fn panel_api_retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(status, StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_EARLY)
}

pub(super) fn panel_api_retry_after_from_header_value(raw: &str) -> Option<Duration> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(
            secs.clamp(PANEL_API_DEFAULT_RETRY_AFTER_SECS, PANEL_API_MAX_RETRY_AFTER_SECS),
        ));
    }
    let retry_at = DateTime::parse_from_rfc2822(trimmed).ok()?.with_timezone(&Utc);
    let now = Utc::now();
    let secs = retry_at.signed_duration_since(now).num_seconds();
    let secs = if secs <= 0 {
        PANEL_API_DEFAULT_RETRY_AFTER_SECS
    } else {
        u64::try_from(secs).unwrap_or(PANEL_API_DEFAULT_RETRY_AFTER_SECS)
    };
    Some(Duration::from_secs(secs.clamp(PANEL_API_DEFAULT_RETRY_AFTER_SECS, PANEL_API_MAX_RETRY_AFTER_SECS)))
}

pub(super) fn panel_api_retry_after_from_headers(headers: &HeaderMap) -> Duration {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(panel_api_retry_after_from_header_value)
        .unwrap_or_else(|| Duration::from_secs(PANEL_API_DEFAULT_RETRY_AFTER_SECS))
}

pub(super) async fn panel_get_json(app_state: &AppState, url: Url) -> Result<Value, TuliproxError> {
    let client = app_state.http_clients.default.load();
    let sanitized = sanitize_sensitive_info(url.as_str());
    for attempt in 0..PANEL_API_RETRY_ATTEMPTS {
        debug_if_enabled!("panel_api request attempt {} of {}: {}", attempt + 1, PANEL_API_RETRY_ATTEMPTS, sanitized);
        let resp = client
            .get(url.clone())
            .timeout(Duration::from_secs(PANEL_API_REQUEST_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            if attempt + 1 < PANEL_API_RETRY_ATTEMPTS && panel_api_retryable_status(status) {
                let cooldown = panel_api_retry_after_from_headers(resp.headers());
                warn!(
                    "panel_api request returned {}, retrying after {:.3}s: {}",
                    format_http_status(status),
                    cooldown.as_secs_f64(),
                    sanitized
                );
                tokio::time::sleep(cooldown).await;
                continue;
            }
            return Err(TuliproxError::ConfigPanelApi(format!(
                "panel_api request failed (http {}): {}",
                format_http_status(status),
                sanitized
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api read response failed: {e}")))?;
        let json: Value = serde_json::from_str(&body)
            .map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api invalid json (http {status}): {e}")))?;
        let sanitize_sensitive =
            app_state.app_config.config.load().log.as_ref().is_none_or(|l| l.sanitize_sensitive_info);
        let json_for_log = sanitize_panel_api_json_for_log(&json, sanitize_sensitive);
        if let Ok(json_str) = serde_json::to_string(&json_for_log) {
            debug_if_enabled!(
                "panel_api response (http {}): {}",
                format_http_status(status),
                sanitize_sensitive_info(&json_str)
            );
        }
        return Ok(json);
    }
    Err(TuliproxError::ConfigPanelApi(format!("panel_api request failed: {sanitized}")))
}

pub(super) async fn user_api_get_json(
    app_state: &AppState,
    input_source: &InputSource,
) -> Result<Value, TuliproxError> {
    let client = app_state.http_clients.default.load();
    let url = Url::parse(input_source.url.as_str()).map_err(|e| {
        TuliproxError::ConfigPanelApi(format!(
            "panel_api user_api invalid url {}: {e}",
            sanitize_sensitive_info(input_source.url.as_str())
        ))
    })?;
    debug_if_enabled!(
        "panel_api user_api request {}",
        sanitize_sensitive_info(&request::preview_request_diagnostics_for_logging(&url, input_source.get_provider()))
    );

    let config = app_state.app_config.config.load();
    let default_user_agent = config.default_user_agent.clone();
    let disabled_headers = config.get_disabled_headers();
    drop(config);

    let headers = request::get_request_headers(
        Some(&input_source.headers),
        None::<&HashMap<String, Vec<u8>>>,
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
    );
    let resp = request::send_with_retry_and_provider(
        &app_state.app_config,
        &url,
        input_source.get_provider(),
        false,
        |resolved_url| {
            client.get(resolved_url.clone()).headers(headers.clone()).timeout(std::time::Duration::from_secs(30))
        },
    )
    .await
    .map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api user_api request failed: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api user_api read response failed: {e}")))?;
    let json: Value = serde_json::from_str(&body)
        .map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api user_api invalid json (http {status}): {e}")))?;
    let sanitize_sensitive = app_state.app_config.config.load().log.as_ref().is_none_or(|l| l.sanitize_sensitive_info);
    let json_for_log = sanitize_panel_api_json_for_log(&json, sanitize_sensitive);
    if let Ok(json_str) = serde_json::to_string(&json_for_log) {
        debug_if_enabled!(
            "panel_api user_api response (http {}): {}",
            format_http_status(status),
            sanitize_sensitive_info(&json_str)
        );
    }
    Ok(json)
}
