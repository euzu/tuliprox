use crate::api::config_file::ConfigFile;
use crate::api::model::{
    create_panel_api_provisioning_stream_with_stop, create_provider_connections_exhausted_stream,
    AppState, StreamDetails,
};
use crate::model::{is_input_expired, ConfigInput, ConfigInputAlias, GracePeriodOptions, PanelApiConfig, PanelApiQueryParam, ProxyUserCredentials};
use crate::repository::{
    csv_patch_batch_append, csv_patch_batch_remove_expired, csv_patch_batch_sort_by_exp_date,
    csv_patch_batch_update_credentials, csv_patch_batch_update_exp_date, get_csv_file_path,
};
use crate::tools::atomic_once_flag::AtomicOnceFlag;
use crate::utils::{
    debug_if_enabled, format_http_status, persist_source_config, read_sources_file_from_path,
};
use axum::http::{Method, StatusCode};
use chrono::{NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use jsonwebtoken::get_current_timestamp;
use log::{error, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared::concat_string;
use shared::error::{info_err, info_err_res, TuliproxError};
use shared::model::{
    ConfigInputAliasDto, InputType, PanelApiAliasPoolSizeValue,
    PanelApiProvisioningMethod, ProxyUserStatus, SourcesConfigDto, VirtualId,
};
use shared::utils::{get_base_url_from_str, get_credentials_from_url, get_credentials_from_url_str, get_i64_from_serde_value, get_string_from_serde_value, parse_timestamp, sanitize_sensitive_info, Internable};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

#[derive(Debug, Clone)]
struct AccountCredentials {
    name: Arc<str>,
    username: String,
    password: String,
    exp_date: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PanelApiExpireMode {
    UtcString,
    ServerTzString,
}

#[derive(Debug, Clone)]
struct PanelApiTimeContext {
    expire_mode: PanelApiExpireMode,
    server_tz: Option<Tz>,
}

#[derive(Debug, Clone)]
struct UserApiAccountInfo {
    exp_date: Option<i64>,
    server_now_ts: Option<i64>,
    server_tz: Option<Tz>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PanelApiTimeCache {
    inputs: HashMap<String, PanelApiTimeCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PanelApiTimeCacheEntry {
    expire_mode: PanelApiExpireMode,
    server_tz: Option<String>,
    skew_secs: Option<i64>,
}

fn parse_boolish(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        Value::String(s) => matches!(
            s.trim().to_lowercase().as_str(),
            "true" | "1" | "yes" | "y" | "ok"
        ),
        _ => false,
    }
}

fn extract_stringish(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn is_date_only_yyyy_mm_dd(value: &str) -> bool {
    let value = value.trim();
    if value.len() != 10 {
        return false;
    }
    let bytes = value.as_bytes();
    bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

fn parse_panel_expire_utc(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let parsed = parse_timestamp(value).ok().flatten();
    if parsed.is_some() {
        return parsed;
    }
    if is_date_only_yyyy_mm_dd(value) {
        let normalized = format!("{value} 00:00:00");
        return parse_timestamp(&normalized).ok().flatten();
    }
    None
}

fn parse_panel_expire_with_tz(value: &str, tz: Tz) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(ts) = value.parse::<i64>() {
        return Some(ts);
    }
    let value = if is_date_only_yyyy_mm_dd(value) {
        format!("{value} 00:00:00")
    } else {
        value.to_string()
    };
    let dt = NaiveDateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S").ok()?;
    match tz.from_local_datetime(&dt) {
        chrono::LocalResult::Single(local_dt) => Some(local_dt.timestamp()),
        chrono::LocalResult::Ambiguous(first, _) => Some(first.timestamp()),
        chrono::LocalResult::None => None,
    }
}

fn normalize_panel_expire(value: &str, ctx: Option<&PanelApiTimeContext>) -> Option<i64> {
    let Some(ctx) = ctx else {
        return parse_panel_expire_utc(value);
    };
    if let Ok(ts) = value.trim().parse::<i64>() {
        return Some(ts);
    }
    match ctx.expire_mode {
        PanelApiExpireMode::UtcString => parse_panel_expire_utc(value),
        PanelApiExpireMode::ServerTzString => ctx
            .server_tz
            .and_then(|tz| parse_panel_expire_with_tz(value, tz))
            .or_else(|| parse_panel_expire_utc(value)),
    }
}

fn is_input_expired_at(exp_date: Option<i64>, now: u64) -> bool {
    let Some(exp_date) = exp_date else {
        return false;
    };
    u64::try_from(exp_date)
        .map_or(true, |exp_ts| exp_ts <= now)
}

fn is_expiring_with_offset_at(exp_date: Option<i64>, offset_secs: u64, now: u64) -> bool {
    let Some(exp_date) = exp_date else {
        return false;
    };
    let Ok(exp_ts) = u64::try_from(exp_date) else {
        return true;
    };
    if exp_ts <= now {
        return false;
    }
    now.saturating_add(offset_secs) >= exp_ts
}

fn first_json_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    match value {
        Value::Array(arr) => arr.first().and_then(|v| v.as_object()),
        Value::Object(obj) => Some(obj),
        _ => None,
    }
}

fn extract_username_password_from_json(
    obj: &serde_json::Map<String, Value>,
) -> Option<(String, String)> {
    let username = obj
        .get("username")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let password = obj
        .get("password")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (username, password) {
        (Some(u), Some(p)) => Some((u.to_string(), p.to_string())),
        _ => None,
    }
}

fn validate_type_is_m3u(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    let typ = params
        .iter()
        .find(|p| p.key.trim().eq_ignore_ascii_case("type"))
        .map(|p| p.value.trim().to_string());
    let typ_str = typ.as_deref().unwrap_or_default();
    if typ_str.trim().eq_ignore_ascii_case("m3u") {
        Ok(())
    } else if typ_str.is_empty() {
        info_err_res!("panel_api: missing required query param 'type=m3u'")
    } else {
        info_err_res!("panel_api: unsupported type={typ_str}, only m3u is supported")
    }
}

fn require_api_key_param(params: &[PanelApiQueryParam], section: &str) -> Result<(), TuliproxError> {
    let api_key = params
        .iter()
        .find(|p| p.key.trim().eq_ignore_ascii_case("api_key"));
    let Some(api_key) = api_key else {
        return info_err_res!(
            "panel_api: {section} must contain query param 'api_key' (use value 'auto')"
        );
    };
    if api_key.value.trim().is_empty() {
        return info_err_res!(
            "panel_api: {section} query param 'api_key' must not be empty (use value 'auto')"
        );
    }
    Ok(())
}

fn require_username_password_params_auto(
    params: &[PanelApiQueryParam],
    section: &str,
) -> Result<(), TuliproxError> {
    let username = params
        .iter()
        .find(|p| p.key.trim().eq_ignore_ascii_case("username"));
    let password = params
        .iter()
        .find(|p| p.key.trim().eq_ignore_ascii_case("password"));
    if username.is_none() || password.is_none() {
        return info_err_res!(
            "panel_api: {section} must contain query params 'username' and 'password' (use value 'auto')"
        );
    }
    if !username.is_some_and(|p| p.value.trim().eq_ignore_ascii_case("auto"))
        || !password.is_some_and(|p| p.value.trim().eq_ignore_ascii_case("auto"))
    {
        return info_err_res!(
            "panel_api: {section} requires 'username: auto' and 'password: auto' (credentials must not be hardcoded)"
        );
    }
    Ok(())
}

fn validate_client_new_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.client_new")?;
    validate_type_is_m3u(params)?;
    if params
        .iter()
        .any(|p| p.key.trim().eq_ignore_ascii_case("user"))
    {
        return info_err_res!("panel_api: client_new must not contain query param 'user'");
    }
    Ok(())
}

fn validate_client_renew_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.client_renew")?;
    validate_type_is_m3u(params)?;
    require_username_password_params_auto(params, "query_parameter.client_renew")?;
    Ok(())
}

fn validate_client_info_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.client_info")?;
    require_username_password_params_auto(params, "query_parameter.client_info")?;
    Ok(())
}

fn validate_account_info_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.account_info")?;
    let has_user = params
        .iter()
        .any(|p| p.key.trim().eq_ignore_ascii_case("username"));
    let has_pass = params
        .iter()
        .any(|p| p.key.trim().eq_ignore_ascii_case("password"));
    if has_user || has_pass {
        require_username_password_params_auto(params, "query_parameter.account_info")?;
    }
    Ok(())
}

fn validate_client_adult_content_params(
    params: &[PanelApiQueryParam],
) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.client_adult_content")?;
    let has_user = params
        .iter()
        .any(|p| p.key.trim().eq_ignore_ascii_case("username"));
    let has_pass = params
        .iter()
        .any(|p| p.key.trim().eq_ignore_ascii_case("password"));
    if has_user || has_pass {
        require_username_password_params_auto(params, "query_parameter.client_adult_content")?;
    }
    Ok(())
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy)]
struct PanelApiOptionalFlags {
    account_info: bool,
    client_new: bool,
    client_renew: bool,
    adult_content: bool,
}

fn resolve_panel_api_optional_flags(
    cfg: &PanelApiConfig,
    input_name: &str,
) -> PanelApiOptionalFlags {
    let flags = PanelApiOptionalFlags {
        account_info: !cfg.query_parameter.account_info.is_empty(),
        client_new: !cfg.query_parameter.client_new.is_empty(),
        client_renew: !cfg.query_parameter.client_renew.is_empty(),
        adult_content: !cfg.query_parameter.client_adult_content.is_empty(),
    };
    let name = sanitize_sensitive_info(input_name);
    if !flags.client_renew {
        debug_if_enabled!(
            "panel_api request for client_renew disabled due to missing arguments for {}",
            name
        );
    }
    if !flags.client_new {
        debug_if_enabled!(
            "panel_api request for client_new disabled due to missing arguments for {}",
            name
        );
    }
    if !flags.adult_content {
        debug_if_enabled!(
            "panel_api request for client_adult_content disabled due to missing arguments for {}",
            name
        );
    }
    if !flags.account_info {
        debug_if_enabled!(
            "panel_api request for account_info disabled due to missing arguments for {}",
            name
        );
    }
    flags
}

fn parse_panel_api_provisioning_offset_secs(offset: &str) -> Result<u64, TuliproxError> {
    let raw = offset.trim();
    if raw.is_empty() {
        return Ok(0);
    }
    let lower = raw.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let last = *bytes.last().unwrap_or(&b'\0');
    let (num_part, multiplier) = match last {
        b's' => (&lower[..lower.len().saturating_sub(1)], 1_u64),
        b'm' => (&lower[..lower.len().saturating_sub(1)], 60_u64),
        b'h' => (&lower[..lower.len().saturating_sub(1)], 60_u64 * 60),
        b'd' => (&lower[..lower.len().saturating_sub(1)], 60_u64 * 60 * 24),
        b'0'..=b'9' => (lower.as_str(), 1_u64),
        _ => {
            return info_err_res!(
                "panel_api.provisioning.offset must be a number with optional suffix s/m/h/d (e.g. 30m, 12h), got '{raw}'"
            );
        }
    };
    let num_part = num_part.trim();
    if num_part.is_empty() {
        return info_err_res!(
            "panel_api.provisioning.offset must be a number with optional suffix s/m/h/d (e.g. 30m, 12h), got '{raw}'"
        );
    }
    let value: u64 = num_part
        .parse()
        .map_err(|_| info_err!("panel_api.provisioning.offset is not a valid number: '{raw}'"))?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| info_err!("panel_api.provisioning.offset is too large: '{raw}'"))
}

fn validate_panel_api_config(cfg: &PanelApiConfig) -> Result<(), TuliproxError> {
    if !cfg.enabled {
        return Ok(());
    }
    if cfg.url.trim().is_empty() {
        return info_err_res!("panel_api: url is missing");
    }
    if cfg.api_key.as_ref().is_none_or(|k| k.trim().is_empty()) {
        return info_err_res!("panel_api: api_key is missing");
    }
    if cfg.query_parameter.client_info.is_empty() {
        return info_err_res!("panel_api: query_parameter.client_info must be configured");
    }
    validate_client_info_params(&cfg.query_parameter.client_info)?;
    if !cfg.query_parameter.client_new.is_empty() {
        validate_client_new_params(&cfg.query_parameter.client_new)?;
    }
    if !cfg.query_parameter.client_renew.is_empty() {
        validate_client_renew_params(&cfg.query_parameter.client_renew)?;
    }
    if !cfg.query_parameter.account_info.is_empty() {
        validate_account_info_params(&cfg.query_parameter.account_info)?;
    }
    if !cfg.query_parameter.client_adult_content.is_empty() {
        validate_client_adult_content_params(&cfg.query_parameter.client_adult_content)?;
    }
    let (min_val, max_val) = alias_pool_limit_values(cfg);
    if let Some(PanelApiAliasPoolSizeValue::Number(value)) = min_val {
        if *value == 0 {
            return info_err_res!("panel_api.alias_pool.size.min must be greater than 0");
        }
    }
    if let Some(PanelApiAliasPoolSizeValue::Number(value)) = max_val {
        if *value == 0 {
            return info_err_res!("panel_api.alias_pool.size.max must be greater than 0");
        }
    }
    let min = min_val.and_then(PanelApiAliasPoolSizeValue::as_number);
    let max = max_val.and_then(PanelApiAliasPoolSizeValue::as_number);
    if let (Some(min), Some(max)) = (min, max) {
        if min > max {
            return info_err_res!(
                "panel_api.alias_pool.size.min must be <= panel_api.alias_pool.size.max"
            );
        }
    }
    if cfg.provisioning.probe_interval_sec == 0 {
        return info_err_res!("panel_api.provisioning.probe_interval_sec must be greater than 0");
    }
    if let Some(offset) = cfg.provisioning.offset.as_deref() {
        let _secs = parse_panel_api_provisioning_offset_secs(offset)?;
    }
    Ok(())
}

fn resolve_query_params(
    params: &[PanelApiQueryParam],
    api_key: Option<&str>,
    creds: Option<(&str, &str)>,
) -> Result<Vec<(String, String)>, TuliproxError> {
    let mut out = Vec::with_capacity(params.len());
    for p in params {
        let key = p.key.trim();
        if key.is_empty() {
            continue;
        }
        let mut value = p.value.trim().to_string();
        if value.eq_ignore_ascii_case("auto") {
            if key.eq_ignore_ascii_case("api_key") {
                let Some(k) = api_key.filter(|s| !s.trim().is_empty()) else {
                    return info_err_res!(
                        "panel_api: query param {key} uses 'auto' but panel_api.api_key is missing"
                    );
                };
                value = k.to_string();
            } else if key.eq_ignore_ascii_case("username") {
                let Some((u, _)) = creds else {
                    return info_err_res!(
                        "panel_api: query param {key} uses 'auto' but no account username is available"
                    );
                };
                value = u.to_string();
            } else if key.eq_ignore_ascii_case("password") {
                let Some((_, pw)) = creds else {
                    return info_err_res!(
                        "panel_api: query param {key} uses 'auto' but no account password is available"
                    );
                };
                value = pw.to_string();
            }
        }
        out.push((key.to_string(), value));
    }
    Ok(out)
}

fn build_panel_url(
    base_url: &str,
    query_params: &[(String, String)],
) -> Result<Url, TuliproxError> {
    let mut url =
        Url::parse(base_url).map_err(|e| info_err!("panel_api: invalid url {base_url}: {e}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query_params {
            pairs.append_pair(k, v);
        }
    }
    Ok(url)
}

fn sanitize_panel_api_json_for_log(value: &Value, sanitize_sensitive: bool) -> Value {
    match value {
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| sanitize_panel_api_json_for_log(v, sanitize_sensitive))
                .collect(),
        ),
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
                        out.insert(
                            k.clone(),
                            Value::String(sanitize_sensitive_info(s).into_owned()),
                        );
                        continue;
                    }
                }
                out.insert(
                    k.clone(),
                    sanitize_panel_api_json_for_log(v, sanitize_sensitive),
                );
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

async fn panel_get_json(app_state: &AppState, url: Url) -> Result<Value, TuliproxError> {
    let client = app_state.http_client.load();
    let sanitized = sanitize_sensitive_info(url.as_str());
    debug_if_enabled!("panel_api request {}", sanitized);
    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| info_err!("panel_api request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| info_err!("panel_api read response failed: {e}"))?;
    let json: Value = serde_json::from_str(&body)
        .map_err(|e| info_err!("panel_api invalid json (http {status}): {e}"))?;
    let sanitize_sensitive = app_state
        .app_config
        .config
        .load()
        .log
        .as_ref()
        .is_none_or(|l| l.sanitize_sensitive_info);
    let json_for_log = sanitize_panel_api_json_for_log(&json, sanitize_sensitive);
    if let Ok(json_str) = serde_json::to_string(&json_for_log) {
        debug_if_enabled!(
            "panel_api response (http {}): {}",
            format_http_status(status),
            sanitize_sensitive_info(&json_str)
        );
    }
    Ok(json)
}

async fn user_api_get_json(app_state: &AppState, url: Url) -> Result<Value, TuliproxError> {
    let client = app_state.http_client.load();
    let sanitized = sanitize_sensitive_info(url.as_str());
    debug_if_enabled!("panel_api user_api request {}", sanitized);
    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| info_err!("panel_api user_api request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| info_err!("panel_api user_api read response failed: {e}"))?;
    let json: Value = serde_json::from_str(&body)
        .map_err(|e| info_err!("panel_api user_api invalid json (http {status}): {e}"))?;
    let sanitize_sensitive = app_state
        .app_config
        .config
        .load()
        .log
        .as_ref()
        .is_none_or(|l| l.sanitize_sensitive_info);
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

async fn panel_client_new(
    app_state: &AppState,
    cfg: &PanelApiConfig,
) -> Result<(String, String, Option<String>), TuliproxError> {
    validate_client_new_params(&cfg.query_parameter.client_new)?;
    let params = resolve_query_params(
        &cfg.query_parameter.client_new,
        cfg.api_key.as_deref(),
        None,
    )?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return info_err_res!("panel_api: client_new response is not a JSON object/array");
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return info_err_res!("panel_api: client_new status=false");
    }
    if let Some((u, p)) = extract_username_password_from_json(obj) {
        return Ok((u, p, None));
    }
    if let Some(url_str) = obj.get("url").and_then(|v| v.as_str()) {
        if let (Some(u), Some(p)) = get_credentials_from_url_str(url_str) {
            let base = get_base_url_from_str(url_str);
            return Ok((u, p, base));
        }
    }
    info_err_res!("panel_api: client_new response missing username/password (and no parsable url)")
}

async fn panel_client_renew(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    username: &str,
    password: &str,
) -> Result<(), TuliproxError> {
    validate_client_renew_params(&cfg.query_parameter.client_renew)?;
    let params = resolve_query_params(
        &cfg.query_parameter.client_renew,
        cfg.api_key.as_deref(),
        Some((username, password)),
    )?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return info_err_res!("panel_api: client_renew response is not a JSON object/array");
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return info_err_res!("panel_api: client_renew status=false");
    }
    Ok(())
}

async fn panel_client_info_raw(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    username: &str,
    password: &str,
) -> Result<Option<String>, TuliproxError> {
    validate_client_info_params(&cfg.query_parameter.client_info)?;
    let params = resolve_query_params(
        &cfg.query_parameter.client_info,
        cfg.api_key.as_deref(),
        Some((username, password)),
    )?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return info_err_res!("panel_api: client_info response is not a JSON object/array");
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return info_err_res!("panel_api: client_info status=false");
    }
    let expire = obj
        .get("expire")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if expire.is_empty() {
        Ok(None)
    } else {
        Ok(Some(expire))
    }
}

async fn panel_client_info(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    username: &str,
    password: &str,
    time_ctx: Option<&PanelApiTimeContext>,
) -> Result<Option<i64>, TuliproxError> {
    let expire = panel_client_info_raw(app_state, cfg, username, password).await?;
    Ok(expire
        .as_deref()
        .and_then(|value| normalize_panel_expire(value, time_ctx)))
}

async fn fetch_root_user_api_info(
    app_state: &AppState,
    input: &ConfigInput,
) -> Option<UserApiAccountInfo> {
    let (username, password) = extract_account_creds_from_input(input)?;
    let base_url = get_base_url_from_str(input.url.as_str()).unwrap_or_else(|| input.url.clone());
    let mut url = Url::parse(base_url.as_str()).ok()?;
    url.set_path("/player_api.php");
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("username", username.as_ref());
        pairs.append_pair("password", password.as_ref());
        pairs.append_pair("action", "account_info");
    }

    let json = match user_api_get_json(app_state, url).await {
        Ok(json) => json,
        Err(err) => {
            debug_if_enabled!(
                "panel_api user_api account_info failed for {}: {}",
                sanitize_sensitive_info(&input.name),
                sanitize_sensitive_info(err.to_string().as_str())
            );
            return None;
        }
    };

    let exp_date = json
        .get("user_info")
        .and_then(|v| v.get("exp_date"))
        .and_then(get_i64_from_serde_value);
    let server_now_ts = json
        .get("server_info")
        .and_then(|v| v.get("timestamp_now"))
        .and_then(get_i64_from_serde_value);
    let server_tz = json
        .get("server_info")
        .and_then(|v| v.get("timezone"))
        .and_then(get_string_from_serde_value)
        .and_then(|tz| tz.parse::<Tz>().ok());

    Some(UserApiAccountInfo {
        exp_date,
        server_now_ts,
        server_tz,
    })
}

fn resolve_panel_expire_mode(
    root_expire: Option<i64>,
    panel_expire: Option<&str>,
    server_tz: Option<Tz>,
) -> PanelApiExpireMode {
    let Some(root_expire) = root_expire else {
        return PanelApiExpireMode::UtcString;
    };
    let Some(panel_expire) = panel_expire else {
        return PanelApiExpireMode::UtcString;
    };
    let utc_ts = parse_panel_expire_utc(panel_expire);
    let tz_ts = server_tz.and_then(|tz| parse_panel_expire_with_tz(panel_expire, tz));
    let Some(utc_ts) = utc_ts else {
        return tz_ts.map_or(PanelApiExpireMode::UtcString, |_| {
            PanelApiExpireMode::ServerTzString
        });
    };
    let Some(tz_ts) = tz_ts else {
        return PanelApiExpireMode::UtcString;
    };
    let diff_utc = (utc_ts - root_expire).abs();
    let diff_tz = (tz_ts - root_expire).abs();
    let threshold = 120_i64;
    match (diff_utc <= threshold, diff_tz <= threshold) {
        (true, false) => PanelApiExpireMode::UtcString,
        (false, true) => PanelApiExpireMode::ServerTzString,
        _ => {
            if diff_tz < diff_utc {
                PanelApiExpireMode::ServerTzString
            } else {
                PanelApiExpireMode::UtcString
            }
        }
    }
}

fn apply_clock_skew(now: u64, skew_secs: i64) -> u64 {
    if skew_secs == 0 {
        return now;
    }
    if skew_secs.is_negative() {
        now.saturating_sub(skew_secs.unsigned_abs())
    } else {
        now.saturating_add(u64::try_from(skew_secs).unwrap_or(0))
    }
}

fn panel_api_time_cache_path(app_state: &AppState) -> PathBuf {
    let paths = app_state.app_config.paths.load();
    PathBuf::from(&paths.config_path)
        .join("panel-api")
        .join("panel_api_time_cache.json")
}

async fn load_panel_api_time_cache(app_state: &AppState, cache_path: &Path) -> PanelApiTimeCache {
    if let Some(parent) = cache_path.parent() {
        if tokio::fs::create_dir_all(parent).await.is_err() {
            return PanelApiTimeCache::default();
        }
    }
    let _lock = app_state.app_config.file_locks.read_lock(cache_path).await;
    let Ok(content) = tokio::fs::read_to_string(cache_path).await else {
        return PanelApiTimeCache::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

async fn persist_panel_api_time_cache(
    app_state: &AppState,
    cache_path: &Path,
    cache: &PanelApiTimeCache,
) -> Result<(), TuliproxError> {
    if let Some(parent) = cache_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| info_err!("panel_api: failed to create cache dir: {e}"))?;
    }
    let content = serde_json::to_string_pretty(cache).map_err(|e| info_err!("panel_api: {e}"))?;
    let _lock = app_state.app_config.file_locks.write_lock(cache_path).await;
    tokio::fs::write(cache_path, content)
        .await
        .map_err(|e| info_err!("panel_api: failed to persist time cache: {e}"))?;
    Ok(())
}

fn parse_cached_tz(tz: Option<String>) -> Option<Tz> {
    tz.and_then(|name| name.parse::<Tz>().ok())
}

async fn panel_account_info(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    creds: Option<(&str, &str)>,
) -> Result<Option<String>, TuliproxError> {
    if cfg.query_parameter.account_info.is_empty() {
        return Ok(None);
    }
    validate_account_info_params(&cfg.query_parameter.account_info)?;
    let params = resolve_query_params(
        &cfg.query_parameter.account_info,
        cfg.api_key.as_deref(),
        creds,
    )?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return info_err_res!("panel_api: account_info response is not a JSON object/array");
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return info_err_res!("panel_api: account_info status=false");
    }
    let Some(credits) = obj.get("credits").and_then(extract_stringish) else {
        return info_err_res!("panel_api: account_info response missing credits");
    };
    Ok(Some(credits))
}

async fn panel_client_adult_content(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    creds: Option<(&str, &str)>,
) -> Result<(), TuliproxError> {
    if cfg.query_parameter.client_adult_content.is_empty() {
        return Ok(());
    }
    validate_client_adult_content_params(&cfg.query_parameter.client_adult_content)?;
    let params = resolve_query_params(
        &cfg.query_parameter.client_adult_content,
        cfg.api_key.as_deref(),
        creds,
    )?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return info_err_res!(
            "panel_api: client_adult_content response is not a JSON object/array"
        );
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return info_err_res!("panel_api: client_adult_content status=false");
    }
    Ok(())
}

fn extract_account_creds_from_input(input: &ConfigInput) -> Option<(String, String)> {
    if let (Some(u), Some(p)) = (input.username.as_deref(), input.password.as_deref()) {
        if !u.trim().is_empty() && !p.trim().is_empty() {
            return Some((u.to_string(), p.to_string()));
        }
    }
    Url::parse(input.url.as_str()).ok().and_then(|u| {
        let (uu, pp) = get_credentials_from_url(&u);
        match (uu, pp) {
            (Some(uu), Some(pp)) if !uu.trim().is_empty() && !pp.trim().is_empty() => {
                Some((uu, pp))
            }
            _ => None,
        }
    })
}

fn alias_pool_limit_values(
    cfg: &PanelApiConfig,
) -> (
    Option<&PanelApiAliasPoolSizeValue>,
    Option<&PanelApiAliasPoolSizeValue>,
) {
    let size = cfg.alias_pool.as_ref().and_then(|p| p.size.as_ref());
    let min = size.and_then(|s| s.min.as_ref());
    let max = size.and_then(|s| s.max.as_ref());
    (min, max)
}

#[allow(dead_code)]
fn alias_pool_both_auto(cfg: &PanelApiConfig) -> bool {
    let (min, max) = alias_pool_limit_values(cfg);
    min.is_some_and(PanelApiAliasPoolSizeValue::is_auto)
        && max.is_some_and(PanelApiAliasPoolSizeValue::is_auto)
}

fn alias_pool_has_min(cfg: &PanelApiConfig) -> bool {
    let (min, _) = alias_pool_limit_values(cfg);
    min.is_some()
}

fn resolve_alias_pool_limit_value(
    value: Option<&PanelApiAliasPoolSizeValue>,
    auto_value: Option<u16>,
) -> Option<u16> {
    match value {
        Some(PanelApiAliasPoolSizeValue::Number(v)) => Some(*v),
        Some(PanelApiAliasPoolSizeValue::Auto(_)) => auto_value,
        None => None,
    }
}

fn is_proxy_user_enabled(user: &ProxyUserCredentials) -> bool {
    if let Some(status) = user.status {
        if !matches!(status, ProxyUserStatus::Active | ProxyUserStatus::Trial) {
            return false;
        }
    }
    !is_input_expired(user.exp_date)
}

fn find_input_target_names(app_state: &AppState, input_name: &Arc<str>) -> Vec<String> {
    let sources = app_state.app_config.sources.load();
    for source in &sources.sources {
        if source.inputs.iter().any(|name| name == input_name) {
            return source.targets.iter().map(|t| t.name.clone()).collect();
        }
    }
    vec![]
}

fn count_enabled_proxy_users(app_state: &AppState, input_name: &Arc<str>) -> usize {
    let api_proxy_guard = app_state.app_config.api_proxy.load();
    let Some(api_proxy) = api_proxy_guard.as_ref() else {
        return 0;
    };
    let target_names = find_input_target_names(app_state, input_name);
    if target_names.is_empty() {
        return 0;
    }
    api_proxy
        .user
        .iter()
        .filter(|target_user| {
            target_names
                .iter()
                .any(|target| target.eq_ignore_ascii_case(&target_user.target))
        })
        .map(|target_user| {
            target_user
                .credentials
                .iter()
                .filter(|cred| is_proxy_user_enabled(cred))
                .count()
        })
        .sum()
}

fn resolve_alias_pool_auto_value(app_state: &AppState, input_name: &Arc<str>) -> u16 {
    let enabled_users = count_enabled_proxy_users(app_state, input_name);
    u16::try_from(enabled_users).unwrap_or(u16::MAX)
}

#[allow(dead_code)]
pub(crate) fn target_has_alias_pool_min(app_state: &AppState, target_name: &str) -> bool {
    let sources = app_state.app_config.sources.load();
    for source in &sources.sources {
        let target_match = source
            .targets
            .iter()
            .any(|target| target.name.eq_ignore_ascii_case(target_name));
        if !target_match {
            continue;
        }
        for input_name in &source.inputs {
            let Some(input) = sources.get_input_by_name(input_name) else {
                continue;
            };
            if let Some(panel_cfg) = input.panel_api.as_ref() {
                if !panel_cfg.enabled {
                    continue;
                }
                if alias_pool_has_min(panel_cfg) {
                    return true;
                }
            }
        }
    }
    false
}

fn resolve_alias_pool_limits(
    app_state: &AppState,
    input_name: &Arc<str>,
    cfg: &PanelApiConfig,
) -> Result<(Option<u16>, Option<u16>), TuliproxError> {
    let (min_val, max_val) = alias_pool_limit_values(cfg);
    if min_val.is_none() && max_val.is_none() {
        return Ok((None, None));
    }
    let min_auto = min_val.is_some_and(PanelApiAliasPoolSizeValue::is_auto);
    let auto_value = min_auto.then(|| resolve_alias_pool_auto_value(app_state, input_name));
    let min = resolve_alias_pool_limit_value(min_val, auto_value);
    let max = match max_val {
        Some(PanelApiAliasPoolSizeValue::Number(value)) => Some(*value),
        Some(PanelApiAliasPoolSizeValue::Auto(_)) | None => None,
    };
    if let (Some(min), Some(max)) = (min, max) {
        if min > max {
            return info_err_res!(
                "panel_api.alias_pool.size.min must be <= panel_api.alias_pool.size.max"
            );
        }
    }
    Ok((min, max))
}

#[allow(dead_code)]
fn resolve_alias_pool_min(
    app_state: &AppState,
    input_name: &Arc<str>,
    cfg: &PanelApiConfig,
) -> Option<u16> {
    let (min_val, _) = alias_pool_limit_values(cfg);
    let min_val = min_val?;
    let auto_value = min_val
        .is_auto()
        .then(|| resolve_alias_pool_auto_value(app_state, input_name));
    resolve_alias_pool_limit_value(Some(min_val), auto_value)
}

fn alias_pool_remove_expired(cfg: &PanelApiConfig) -> bool {
    cfg.alias_pool.as_ref().is_some_and(|p| p.remove_expired)
}

fn collect_accounts(input: &ConfigInput) -> Vec<AccountCredentials> {
    let mut out = Vec::new();
    if let Some((u, p)) = extract_account_creds_from_input(input) {
        out.push(AccountCredentials {
            name: input.name.clone(),
            username: u,
            password: p,
            exp_date: input.exp_date,
        });
    }
    if let Some(aliases) = input.aliases.as_ref() {
        for a in aliases {
            if let (Some(u), Some(p)) = (a.username.as_deref(), a.password.as_deref()) {
                if !u.trim().is_empty() && !p.trim().is_empty() {
                    out.push(AccountCredentials {
                        name: a.name.clone(),
                        username: u.to_string(),
                        password: p.to_string(),
                        exp_date: a.exp_date,
                    });
                }
            }
        }
    }
    out
}

fn compare_alias_exp_date(a: &ConfigInputAliasDto, b: &ConfigInputAliasDto) -> Ordering {
    let a_ts = a.exp_date.unwrap_or(i64::MAX);
    let b_ts = b.exp_date.unwrap_or(i64::MAX);
    a_ts.cmp(&b_ts).then_with(|| a.name.cmp(&b.name))
}

fn compare_alias_exp_date_config(a: &ConfigInputAlias, b: &ConfigInputAlias) -> Ordering {
    let a_ts = a.exp_date.unwrap_or(i64::MAX);
    let b_ts = b.exp_date.unwrap_or(i64::MAX);
    a_ts.cmp(&b_ts).then_with(|| a.name.cmp(&b.name))
}

fn compare_account_exp_date(a: &AccountCredentials, b: &AccountCredentials) -> Ordering {
    let a_ts = a.exp_date.unwrap_or(i64::MAX);
    let b_ts = b.exp_date.unwrap_or(i64::MAX);
    a_ts.cmp(&b_ts).then_with(|| a.name.cmp(&b.name))
}

fn aliases_need_sort_config(aliases: &[ConfigInputAlias]) -> bool {
    if aliases.len() < 2 {
        return false;
    }
    aliases
        .windows(2)
        .any(|pair| compare_alias_exp_date_config(&pair[0], &pair[1]) == Ordering::Greater)
}

fn sort_aliases_by_exp_date(aliases: &mut Vec<ConfigInputAliasDto>) -> bool {
    if aliases.len() < 2 {
        return false;
    }
    let mut sorted = aliases.clone();
    sorted.sort_by(compare_alias_exp_date);
    if &sorted == aliases {
        false
    } else {
        *aliases = sorted;
        true
    }
}

fn sort_account_aliases_keep_root_first(accounts: &mut Vec<AccountCredentials>, root_name: &str) {
    let root = accounts.iter().find(|acct| acct.name.as_ref() == root_name).cloned();
    let mut aliases: Vec<AccountCredentials> = accounts
        .iter()
        .filter(|acct| acct.name.as_ref() != root_name)
        .cloned()
        .collect();
    aliases.sort_by(compare_account_exp_date);
    accounts.clear();
    if let Some(root) = root {
        accounts.push(root);
    }
    accounts.extend(aliases);
}

fn is_account_valid(exp_date: Option<i64>) -> bool {
    exp_date.is_some() && !is_input_expired(exp_date)
}

fn count_valid_accounts(accounts: &[AccountCredentials]) -> usize {
    accounts
        .iter()
        .filter(|acct| is_account_valid(acct.exp_date))
        .count()
}

fn root_counts_towards_pool(accounts: &[AccountCredentials], input_name: &Arc<str>) -> bool {
    accounts
        .iter()
        .find(|acct| &acct.name == input_name)
        .is_some_and(|acct| is_account_valid(acct.exp_date))
}

fn count_valid_accounts_at(accounts: &[AccountCredentials], now: u64) -> usize {
    accounts
        .iter()
        .filter(|acct| acct.exp_date.is_some() && !is_input_expired_at(acct.exp_date, now))
        .count()
}

fn count_valid_alias_accounts_at(
    accounts: &[AccountCredentials],
    input_name: &Arc<str>,
    now: u64,
) -> usize {
    accounts
        .iter()
        .filter(|acct| {
            &acct.name != input_name
                && acct.exp_date.is_some()
                && !is_input_expired_at(acct.exp_date, now)
        })
        .count()
}

fn root_counts_towards_pool_at(
    accounts: &[AccountCredentials],
    input_name: &Arc<str>,
    now: u64,
) -> bool {
    accounts
        .iter()
        .find(|acct| &acct.name == input_name)
        .is_some_and(|acct| acct.exp_date.is_some() && !is_input_expired_at(acct.exp_date, now))
}

fn should_reload_sources_after_internal_write(app_state: &AppState) -> bool {
    !app_state.app_config.config.load().config_hot_reload
}

pub(crate) fn is_alias_pool_max_reached(app_state: &AppState, input: &ConfigInput) -> bool {
    let Some(panel_cfg) = input.panel_api.as_ref() else {
        return false;
    };
    if !panel_cfg.enabled {
        return false;
    }
    if panel_cfg.url.trim().is_empty() {
        return false;
    }
    if validate_panel_api_config(panel_cfg).is_err() {
        return false;
    }
    let Ok((_, max_pool)) = resolve_alias_pool_limits(app_state, &input.name, panel_cfg) else {
        return false;
    };
    if let Some(max_pool) = max_pool {
        let valid_count = count_valid_accounts(&collect_accounts(input));
        if valid_count >= max_pool as usize {
            debug_if_enabled!(
                "panel_api: alias_pool.size.max reached for input {} (valid_accounts={}, max={})",
                sanitize_sensitive_info(&input.name),
                valid_count,
                max_pool
            );
            return true;
        }
    }
    false
}

pub(crate) fn can_provision_on_exhausted(app_state: &AppState, input: &ConfigInput) -> bool {
    let Some(panel_cfg) = input.panel_api.as_ref() else {
        return false;
    };
    if !panel_cfg.enabled {
        return false;
    }
    if panel_cfg.url.trim().is_empty() {
        return false;
    }
    if let Err(err) = validate_panel_api_config(panel_cfg) {
        debug_if_enabled!(
            "panel_api config invalid: {}",
            sanitize_sensitive_info(err.to_string().as_str())
        );
        return false;
    }
    if is_alias_pool_max_reached(app_state, input) {
        return false;
    }
    true
}

#[allow(dead_code)]
pub(crate) fn find_input_by_name(
    app_state: &AppState,
    input_name: &Arc<str>,
) -> Option<Arc<ConfigInput>> {
    let sources = app_state.app_config.sources.load();
    sources.get_input_by_name(input_name).map(Arc::clone)
}

pub(crate) fn find_input_by_provider_name(
    app_state: &AppState,
    provider_name: &str,
) -> Option<Arc<ConfigInput>> {
    let sources = app_state.app_config.sources.load();
    for input in &sources.inputs {
        if &*input.name == provider_name {
            return Some(Arc::clone(input));
        }
        if input
            .aliases
            .as_ref()
            .is_some_and(|aliases| aliases.iter().any(|alias| &*alias.name == provider_name))
        {
            return Some(Arc::clone(input));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn patch_source_yml_add_alias(
    app_state: &Arc<AppState>,
    source_file_path: &Path,
    input_name: &Arc<str>,
    alias_name: &Arc<str>,
    base_url: &str,
    username: &str,
    password: &str,
    exp_date: Option<i64>,
) -> Result<(), TuliproxError> {
    let mut sources = match read_sources_file_from_path(source_file_path, false, false, None) {
        Ok(sources) => sources,
        Err(e) => return info_err_res!("panel_api: failed to read source file: {e}"),
    };

    let Some(input) = sources.inputs.iter_mut().find(|i| i.name == *input_name) else {
        return info_err_res!("panel_api: could not find input '{input_name}' in source.yml");
    };

    let aliases = input.aliases.get_or_insert_with(Vec::new);
    let next_index = aliases.iter().map(|a| a.id).max().unwrap_or(0);
    if next_index == u16::MAX {
        return info_err_res!("panel_api: cannot add alias for '{input_name}': alias id overflow");
    }

    let alias = ConfigInputAliasDto {
        id: 0,
        name: Arc::clone(alias_name),
        url: base_url.to_string(),
        username: Some(username.to_string()),
        password: Some(password.to_string()),
        priority: 0,
        max_connections: 1,
        exp_date,
        enabled: true,
    };

   input.upsert_alias(alias)?;

    persist_source_config(app_state, Some(source_file_path), sources).await?;
    Ok(())
}

#[derive(Debug, Clone)]
enum SourcesYmlPatch {
    UpdatePanelApiCredits {
        input_name: Arc<str>,
        credits: String,
    },
    SortAliases {
        input_name: Arc<str>,
    },
    UpdateExpDate {
        input_name: Arc<str>,
        account_name: Arc<str>,
        exp_date: i64,
    },
    UpdateRootCredentials {
        input_name: Arc<str>,
        username: String,
        password: String,
        exp_date: Option<i64>,
    },
    UpdateAliasCredentials {
        input_name: Arc<str>,
        alias_name: Arc<str>,
        username: String,
        password: String,
        exp_date: Option<i64>,
    },
    AddAlias {
        input_name: Arc<str>,
        alias_name: Arc<str>,
        base_url: String,
        username: String,
        password: String,
        exp_date: Option<i64>,
    },
    RemoveExpiredAliases {
        input_name: Arc<str>,
    },
}

fn update_url_query_credentials_if_present(url: &mut String, username: &str, password: &str) {
    let Ok(mut parsed) = Url::parse(url.as_str()) else {
        return;
    };
    let mut pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut has_user = false;
    let mut has_pass = false;
    for (k, v) in &mut pairs {
        if k.eq_ignore_ascii_case("username") {
            *v = username.to_string();
            has_user = true;
        } else if k.eq_ignore_ascii_case("password") {
            *v = password.to_string();
            has_pass = true;
        }
    }
    if has_user || has_pass {
        if !has_user {
            pairs.push(("username".to_string(), username.to_string()));
        }
        if !has_pass {
            pairs.push(("password".to_string(), password.to_string()));
        }
        parsed.query_pairs_mut().clear();
        {
            let mut qp = parsed.query_pairs_mut();
            for (k, v) in pairs {
                qp.append_pair(k.as_str(), v.as_str());
            }
        }
        *url = parsed.to_string();
    }
}

#[allow(clippy::too_many_lines)]
fn apply_sources_yml_patches(
    doc: &mut SourcesConfigDto,
    patches: &[SourcesYmlPatch],
) -> Result<bool, TuliproxError> {
    if patches.is_empty() {
        return Ok(false);
    }

    let mut changed = false;
    let mut inputs_by_name: HashMap<Arc<str>, usize> = HashMap::with_capacity(doc.inputs.len());
    let mut alias_indices: Vec<HashMap<Arc<str>, usize>> = Vec::with_capacity(doc.inputs.len());
    for (idx, input) in doc.inputs.iter().enumerate() {
        inputs_by_name.insert(input.name.clone(), idx);
        let map = input
            .aliases
            .as_ref()
            .map(|aliases| {
                aliases
                    .iter()
                    .enumerate()
                    .map(|(idx, alias)| (alias.name.clone(), idx))
                    .collect::<HashMap<Arc<str>, usize>>()
            })
            .unwrap_or_default();
        alias_indices.push(map);
    }

    for patch in patches {
        match patch {
            SourcesYmlPatch::UpdatePanelApiCredits {
                input_name,
                credits,
            } => {
                let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                    info_err!("panel_api: could not find input '{input_name}' in source.yml")
                })?;
                let Some(panel_api) = doc.inputs[idx].panel_api.as_mut() else {
                    return Err(info_err!(
                        "panel_api: could not find panel_api for input '{input_name}' in source.yml"
                    ));
                };
                if panel_api.credits.as_deref().map(str::trim) != Some(credits.trim()) {
                    panel_api.credits = Some(credits.trim().to_string());
                    changed = true;
                }
            }
            SourcesYmlPatch::SortAliases { input_name } => {
                let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                    info_err!("panel_api: could not find input '{input_name}' in source.yml")
                })?;
                let Some(aliases) = doc.inputs[idx].aliases.as_mut() else {
                    continue;
                };
                if sort_aliases_by_exp_date(aliases) {
                    alias_indices[idx] = aliases
                        .iter()
                        .enumerate()
                        .map(|(idx, alias)| (alias.name.clone(), idx))
                        .collect();
                    changed = true;
                }
            }
            SourcesYmlPatch::UpdateExpDate {
                input_name,
                account_name,
                exp_date,
            } => {
                let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                    info_err!("panel_api: could not find input '{input_name}' in source.yml")
                })?;
                if account_name == input_name {
                    if doc.inputs[idx].exp_date != Some(*exp_date) {
                        doc.inputs[idx].exp_date = Some(*exp_date);
                        doc.inputs[idx].enabled = true;
                        doc.inputs[idx].max_connections = 1;
                        changed = true;
                    }
                    continue;
                }
                let Some(alias_idx) = alias_indices[idx].get(account_name).copied() else {
                    return Err(info_err!(
                        "panel_api: could not find alias '{account_name}' under input '{input_name}' in source.yml"
                    ));
                };
                let aliases = doc.inputs[idx]
                    .aliases
                    .as_mut()
                    .ok_or_else(|| info_err!("panel_api: input '{input_name}' has no aliases"))?;
                if aliases[alias_idx].exp_date != Some(*exp_date) {
                    aliases[alias_idx].exp_date = Some(*exp_date);
                    aliases[alias_idx].max_connections = 1;
                    changed = true;
                }
            }
            SourcesYmlPatch::UpdateRootCredentials {
                input_name,
                username,
                password,
                exp_date,
            } => {
                let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                    info_err!("panel_api: could not find input '{input_name}' in source.yml")
                })?;
                let input = &mut doc.inputs[idx];
                let exp_date_changed = exp_date.is_some() && input.exp_date != *exp_date;
                if input.username.as_deref() != Some(username.as_str())
                    || input.password.as_deref() != Some(password.as_str())
                    || exp_date_changed
                {
                    input.username = Some(username.clone());
                    input.password = Some(password.clone());
                    input.enabled = true;
                    input.max_connections = 1;
                    if let Some(exp_date) = *exp_date {
                        input.exp_date = Some(exp_date);
                    }
                    update_url_query_credentials_if_present(&mut input.url, username, password);
                    changed = true;
                }
            }
            SourcesYmlPatch::UpdateAliasCredentials {
                input_name,
                alias_name,
                username,
                password,
                exp_date,
            } => {
                let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                    info_err!("panel_api: could not find input '{input_name}' in source.yml")
                })?;
                let Some(alias_idx) = alias_indices[idx].get(alias_name).copied() else {
                    return Err(info_err!(
                        "panel_api: could not find alias '{alias_name}' under input '{input_name}' in source.yml"
                    ));
                };
                let aliases = doc.inputs[idx]
                    .aliases
                    .as_mut()
                    .ok_or_else(|| info_err!("panel_api: input '{input_name}' has no aliases"))?;
                let alias = &mut aliases[alias_idx];
                let exp_date_changed = exp_date.is_some() && alias.exp_date != *exp_date;
                if alias.username.as_deref() != Some(username.as_str())
                    || alias.password.as_deref() != Some(password.as_str())
                    || exp_date_changed
                {
                    alias.username = Some(username.clone());
                    alias.password = Some(password.clone());
                    alias.max_connections = 1;
                    if let Some(exp_date) = *exp_date {
                        alias.exp_date = Some(exp_date);
                    }
                    update_url_query_credentials_if_present(&mut alias.url, username, password);
                    changed = true;
                }
            }
            SourcesYmlPatch::AddAlias {
                input_name,
                alias_name,
                base_url,
                username,
                password,
                exp_date,
            } => {
                let idx = *inputs_by_name.get(input_name).ok_or_else(|| {
                    info_err!("panel_api: could not find input '{input_name}' in source.yml")
                })?;
                let input_type = doc.inputs[idx].input_type;
                let aliases = doc.inputs[idx].aliases.get_or_insert_with(Vec::new);

                let next_index = u16::try_from(aliases.len()).map_err(|_| {
                    info_err!("panel_api: cannot add alias for '{input_name}': alias id overflow")
                })?;

                let mut alias = ConfigInputAliasDto {
                    id: 0,
                    name: Arc::clone(alias_name),
                    url: base_url.clone(),
                    username: Some(username.clone()),
                    password: Some(password.clone()),
                    priority: 0,
                    max_connections: 1,
                    exp_date: *exp_date,
                    enabled: true,
                };
                alias.prepare(next_index, &input_type)?;
                aliases.push(alias);

                alias_indices[idx].insert(Arc::clone(alias_name), aliases.len().saturating_sub(1));
                changed = true;
            }
            SourcesYmlPatch::RemoveExpiredAliases { input_name } => {
                let idx = *inputs_by_name.get(input_name).ok_or_else(|| {
                    info_err!("panel_api: could not find input '{input_name}' in source.yml")
                })?;
                let Some(aliases) = doc.inputs[idx].aliases.as_mut() else {
                    continue;
                };
                let before = aliases.len();
                aliases.retain(|a| !is_input_expired(a.exp_date));
                if aliases.len() != before {
                    alias_indices[idx] = aliases
                        .iter()
                        .enumerate()
                        .map(|(idx, alias)| (alias.name.clone(), idx))
                        .collect();
                    changed = true;
                }
            }
        }
    }

    Ok(changed)
}

async fn persist_sources_yml_with_patches(
    app_state: &Arc<AppState>,
    sources_path: &Path,
    patches: &[SourcesYmlPatch],
) -> Result<bool, TuliproxError> {
    if patches.is_empty() {
        return Ok(false);
    }
    let mut sources = read_sources_file_from_path(sources_path, false, false, None)
        .map_err(|e| info_err!("panel_api: failed to read source file: {e}"))?;

    let changed = apply_sources_yml_patches(&mut sources, patches)?;
    if !changed {
        return Ok(false);
    }

    persist_source_config(app_state, Some(sources_path), sources).await?;
    Ok(true)
}

async fn patch_source_yml_update_exp_date(
    app_state: &Arc<AppState>,
    source_file_path: &Path,
    input_name: &Arc<str>,
    account_name: &Arc<str>,
    exp_date: i64,
) -> Result<(), TuliproxError> {
    let mut sources = match read_sources_file_from_path(source_file_path, false, false, None) {
        Ok(sources) => sources,
        Err(e) => return info_err_res!("panel_api: failed to read source file: {e}"),
    };

    let Some(input) = sources.inputs.iter_mut().find(|i| i.name == *input_name) else {
        return info_err_res!("panel_api: could not find input '{input_name}' in source.yml");
    };

    if account_name == input_name {
        input.exp_date = Some(exp_date);
        input.enabled = true;
        input.max_connections = 1;
    } else if let Some(aliases) = input.aliases.as_mut() {
        let Some(alias) = aliases.iter_mut().find(|a| &a.name == account_name) else {
            return info_err_res!(
                "panel_api: could not find alias '{account_name}' under input '{input_name}' in source.yml"
            );
        };
        alias.exp_date = Some(exp_date);
        alias.max_connections = 1;
    } else {
        return info_err_res!(
            "panel_api: input '{input_name}' has no aliases; cannot update exp_date for '{account_name}'"
        );
    }

    persist_source_config(app_state, Some(source_file_path), sources).await?;
    Ok(())
}

const MAX_ALIAS_NAME_ATTEMPTS: usize = 1000;

fn derive_unique_alias_name(existing: &[Arc<str>], input_name: &Arc<str>, username: &str) -> Arc<str> {
    let base: Arc<str> = concat_string!(input_name, "-", username).intern();
    if !existing.contains(&base) {
        return base;
    }
    for i in 2..MAX_ALIAS_NAME_ATTEMPTS {
        let cand = concat_string!(&*base, "-", &i.to_string()).intern();
        if !existing.contains(&cand) {
            return cand;
        }
    }
    warn!("derive_unique_alias_name: exhausted {MAX_ALIAS_NAME_ATTEMPTS} attempts for base '{base}'; returning potentially duplicate name");
    base
}

fn derive_unique_alias_name_set(
    existing: &HashSet<Arc<str>>,
    input_name: &Arc<str>,
    username: &str,
) -> String {
    let base = format!("{input_name}-{username}");
    if !existing.contains(base.as_str()) {
        return base;
    }
    for i in 2..MAX_ALIAS_NAME_ATTEMPTS {
        let cand = format!("{base}-{i}");
        if !existing.contains(cand.as_str()) {
            return cand;
        }
    }
    warn!(
        "derive_unique_alias_name_set: exhausted {MAX_ALIAS_NAME_ATTEMPTS} attempts for base '{base}'; returning potentially duplicate name"
    );
    base
}

#[derive(Debug, Clone)]
pub(crate) enum PanelApiProvisionOutcome {
    Renewed { username: String, password: String },
    Created { username: String, password: String },
}

impl PanelApiProvisionOutcome {
    pub(crate) fn credentials(&self) -> (&str, &str) {
        match self {
            Self::Renewed { username, password } | Self::Created { username, password } => {
                (username.as_str(), password.as_str())
            }
        }
    }

    pub(crate) fn kind_label(&self) -> &'static str {
        match self {
            Self::Renewed { .. } => "client_renew",
            Self::Created { .. } => "client_new",
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn try_renew_expired_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    is_batch: bool,
    sources_path: &Path,
    treat_missing_exp_date_as_expired: bool,
    optional: PanelApiOptionalFlags,
) -> Option<PanelApiProvisionOutcome> {
    if !optional.client_renew {
        return None;
    }
    let adult_enabled = optional.adult_content;
    let mut candidates = collect_accounts(input);
    for acct in &mut candidates {
        if treat_missing_exp_date_as_expired && acct.exp_date.is_none() {
            acct.exp_date = panel_client_info(
                app_state,
                panel_cfg,
                acct.username.as_str(),
                acct.password.as_str(),
                None,
            )
            .await
            .ok()
            .flatten();
        }
    }
    candidates.sort_by_key(|a| a.exp_date.unwrap_or(i64::MAX));

    for acct in &candidates {
        // Only attempt renew/new when the account is *known* to be expired.
        // If exp_date is missing (even after an optional client_info refresh), we skip renewal.
        let expired = is_input_expired(acct.exp_date);
        if !expired {
            continue;
        }
        match panel_client_renew(
            app_state,
            panel_cfg,
            acct.username.as_str(),
            acct.password.as_str(),
        )
        .await
        {
            Ok(()) => {
                if adult_enabled {
                    if let Err(err) = panel_client_adult_content(
                        app_state,
                        panel_cfg,
                        Some((acct.username.as_str(), acct.password.as_str())),
                    )
                    .await
                    {
                        debug_if_enabled!(
                            "panel_api client_adult_content failed for {}: {}",
                            sanitize_sensitive_info(&acct.name),
                            sanitize_sensitive_info(err.to_string().as_str())
                        );
                    }
                }
                let refreshed_exp = panel_client_info(
                    app_state,
                    panel_cfg,
                    acct.username.as_str(),
                    acct.password.as_str(),
                    None,
                )
                .await
                .ok()
                .flatten();

                if let Some(new_exp) = refreshed_exp.or(acct.exp_date) {
                    if is_batch {
                        let batch_url = input.t_batch_url.as_deref().unwrap_or_default();
                        if let Ok(csv_path) = get_csv_file_path(batch_url) {
                            let _csv_lock =
                                app_state.app_config.file_locks.write_lock(&csv_path).await;
                            if let Err(err) = csv_patch_batch_update_exp_date(
                                input.input_type,
                                &csv_path,
                                &acct.name,
                                &acct.username,
                                &acct.password,
                                new_exp,
                            )
                            .await
                            {
                                debug_if_enabled!(
                                    "panel_api failed to persist renew exp_date to csv: {}",
                                    err
                                );
                            }
                        }
                    } else {
                        let _src_lock = app_state
                            .app_config
                            .file_locks
                            .write_lock(sources_path)
                            .await;
                        if let Err(err) = patch_source_yml_update_exp_date(
                            app_state,
                            sources_path,
                            &input.name,
                            &acct.name,
                            new_exp,
                        )
                        .await
                        {
                            debug_if_enabled!(
                                "panel_api failed to persist renew exp_date to source.yml: {}",
                                err
                            );
                        }
                    }
                }

                if should_reload_sources_after_internal_write(app_state.as_ref()) {
                    if let Err(err) = ConfigFile::load_sources(app_state).await {
                        debug_if_enabled!("panel_api reload sources failed: {}", err);
                    }
                }
                return Some(PanelApiProvisionOutcome::Renewed {
                    username: acct.username.clone(),
                    password: acct.password.clone(),
                });
            }
            Err(err) => {
                debug_if_enabled!(
                    "panel_api client_renew failed for {}: {}",
                    sanitize_sensitive_info(&acct.name),
                    sanitize_sensitive_info(err.to_string().as_str())
                );
            }
        }
    }
    None
}

#[allow(clippy::too_many_lines)]
async fn try_create_new_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    is_batch: bool,
    sources_path: &Path,
    optional: PanelApiOptionalFlags,
) -> Option<PanelApiProvisionOutcome> {
    if !optional.client_new {
        return None;
    }
    let adult_enabled = optional.adult_content;
    match panel_client_new(app_state, panel_cfg).await {
        Ok((username, password, base_url_from_resp)) => {
            let base_url = base_url_from_resp.unwrap_or_else(|| input.url.clone());
            let base_url =
                get_base_url_from_str(base_url.as_str()).unwrap_or_else(|| base_url.clone());

            let mut existing_names: Vec<Arc<str>> = vec![input.name.clone()];
            if let Some(aliases) = input.aliases.as_ref() {
                existing_names.extend(aliases.iter().map(|a| a.name.clone()));
            }
            let alias_name = derive_unique_alias_name(&existing_names, &input.name, &username);

            if adult_enabled {
                if let Err(err) =
                    panel_client_adult_content(app_state, panel_cfg, Some((&username, &password)))
                        .await
                {
                    debug_if_enabled!(
                        "panel_api client_adult_content failed for {}: {}",
                        sanitize_sensitive_info(&alias_name),
                        sanitize_sensitive_info(err.to_string().as_str())
                    );
                }
            }

            let exp_date = panel_client_info(app_state, panel_cfg, &username, &password, None)
                .await
                .ok()
                .flatten();

            if is_batch {
                let batch_url = input.t_batch_url.as_deref().unwrap_or_default();
                match get_csv_file_path(batch_url) {
                    Ok(csv_path) => {
                        let batch_type = if input.input_type == InputType::Xtream {
                            InputType::XtreamBatch
                        } else {
                            InputType::M3uBatch
                        };
                        let _csv_lock = app_state.app_config.file_locks.write_lock(&csv_path).await;
                        if let Err(err) = csv_patch_batch_append(
                            &csv_path,
                            batch_type,
                            &alias_name,
                            &base_url,
                            &username,
                            &password,
                            exp_date,
                        )
                        .await
                        {
                            warn!("panel_api failed to append new account to csv: {err}");
                            return None;
                        }
                    }
                    Err(err) => {
                        warn!(
                            "panel_api cannot resolve batch csv path {}: {}",
                            sanitize_sensitive_info(batch_url),
                            err
                        );
                        return None;
                    }
                }
            } else {
                let _src_lock = app_state
                    .app_config
                    .file_locks
                    .write_lock(sources_path)
                    .await;
                if let Err(err) = patch_source_yml_add_alias(
                    app_state,
                    sources_path,
                    &input.name,
                    &alias_name,
                    &base_url,
                    &username,
                    &password,
                    exp_date,
                )
                .await
                {
                    warn!("panel_api failed to persist new alias to source.yml: {err}");
                    return None;
                }
            }

            if should_reload_sources_after_internal_write(app_state.as_ref()) {
                if let Err(err) = ConfigFile::load_sources(app_state).await {
                    error!("panel_api reload sources failed: {err}");
                    return None;
                }
            }
            Some(PanelApiProvisionOutcome::Created { username, password })
        }
        Err(err) => {
            debug_if_enabled!(
                "panel_api client_new failed: {}",
                sanitize_sensitive_info(err.to_string().as_str())
            );
            None
        }
    }
}

pub async fn try_provision_account_on_exhausted(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
) -> Option<PanelApiProvisionOutcome> {
    let Some(panel_cfg) = input.panel_api.as_ref() else {
        debug_if_enabled!(
            "panel_api: skipped (no panel_api config) for input {}",
            sanitize_sensitive_info(&input.name)
        );
        return None;
    };
    if !panel_cfg.enabled {
        debug_if_enabled!(
            "panel_api: skipped (panel_api.enabled false) for input {}",
            sanitize_sensitive_info(&input.name)
        );
        return None;
    }
    if panel_cfg.url.trim().is_empty() {
        debug_if_enabled!(
            "panel_api: skipped (panel_api.url empty) for input {}",
            sanitize_sensitive_info(&input.name)
        );
        return None;
    }

    let _input_lock = app_state
        .app_config
        .file_locks
        .write_lock_str(format!("panel_api:{}", input.name).as_str())
        .await;

    if let Err(err) = validate_panel_api_config(panel_cfg) {
        debug_if_enabled!(
            "panel_api config invalid: {}",
            sanitize_sensitive_info(err.to_string().as_str())
        );
        return None;
    }
    if is_alias_pool_max_reached(app_state, input) {
        return None;
    }

    let optional = resolve_panel_api_optional_flags(panel_cfg, &input.name);
    debug_if_enabled!(
        "panel_api: exhausted -> provisioning for input {} (aliases={})",
        sanitize_sensitive_info(&input.name),
        input.aliases.as_ref().map_or(0, Vec::len)
    );

    let is_batch = input
        .t_batch_url
        .as_ref()
        .is_some_and(|u| !u.trim().is_empty());
    let sources_file_path = app_state.app_config.paths.load().sources_file_path.clone();
    let sources_path = PathBuf::from(&sources_file_path);

    if let Some(outcome) = try_renew_expired_account(
        app_state,
        input,
        panel_cfg,
        is_batch,
        sources_path.as_path(),
        true,
        optional,
    )
    .await
    {
        debug_if_enabled!(
            "panel_api: provisioning succeeded via client_renew for input {}",
            sanitize_sensitive_info(&input.name)
        );
        return Some(outcome);
    }
    let created = try_create_new_account(
        app_state,
        input,
        panel_cfg,
        is_batch,
        sources_path.as_path(),
        optional,
    )
    .await;
    debug_if_enabled!(
        "panel_api: provisioning via client_new for input {} => {}",
        sanitize_sensitive_info(&input.name),
        if created.is_some() {
            "success"
        } else {
            "failed"
        }
    );
    created
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn ensure_alias_pool_min(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    accounts: &mut Vec<AccountCredentials>,
    min_pool: u16,
    csv_path: Option<&Path>,
    sources_yml_patches: &mut Vec<SourcesYmlPatch>,
    time_ctx: Option<&PanelApiTimeContext>,
    effective_now: u64,
    optional: PanelApiOptionalFlags,
) -> (bool, u16) {
    if min_pool == 0 {
        return (false, 0);
    }

    let renew_enabled = optional.client_renew;
    let new_enabled = optional.client_new;
    let adult_enabled = optional.adult_content;
    if !renew_enabled && !new_enabled {
        return (false, 0);
    }

    let mut changed = false;
    let mut provisioned = 0_u16;
    let max_pool = resolve_alias_pool_limits(app_state.as_ref(), &input.name, panel_cfg)
        .ok()
        .and_then(|(_, max)| max);
    let mut existing_names: HashSet<Arc<str>> = accounts.iter().map(|a| a.name.clone()).collect();
    let max_attempts = usize::from(min_pool).saturating_add(10);
    for _ in 0..max_attempts {
        let current_valid =
            count_valid_alias_accounts_at(accounts, &input.name, effective_now);
        if current_valid >= usize::from(min_pool) {
            break;
        }
        if let Some(max_pool) = max_pool {
            if current_valid >= usize::from(max_pool) {
                break;
            }
        }

        let expired_index = accounts
            .iter()
            .enumerate()
            .filter(|(_, acct)| {
                acct.name != input.name && is_input_expired_at(acct.exp_date, effective_now)
            })
            .min_by_key(|(_, acct)| acct.exp_date.unwrap_or(i64::MAX))
            .map(|(idx, _)| idx);

        if let Some(idx) = expired_index {
            let acct = accounts.get(idx).cloned();
            let Some(acct) = acct else {
                break;
            };
            if renew_enabled {
                match panel_client_renew(
                    app_state.as_ref(),
                    panel_cfg,
                    acct.username.as_str(),
                    acct.password.as_str(),
                )
                .await
                {
                    Ok(()) => {
                        provisioned = provisioned.saturating_add(1);
                        if adult_enabled {
                            if let Err(err) = panel_client_adult_content(
                                app_state.as_ref(),
                                panel_cfg,
                                Some((acct.username.as_str(), acct.password.as_str())),
                            )
                            .await
                            {
                                debug_if_enabled!(
                                    "panel_api client_adult_content failed for {}: {}",
                                    sanitize_sensitive_info(&acct.name),
                                    sanitize_sensitive_info(err.to_string().as_str())
                                );
                            }
                        }

                        let refreshed_exp = panel_client_info(
                            app_state.as_ref(),
                            panel_cfg,
                            acct.username.as_str(),
                            acct.password.as_str(),
                            time_ctx,
                        )
                        .await
                        .ok()
                        .flatten()
                        .or(acct.exp_date);

                        if let Some(new_exp) = refreshed_exp {
                            if let Some(acct_mut) = accounts.get_mut(idx) {
                                acct_mut.exp_date = Some(new_exp);
                            }
                            if let Some(csv_path) = csv_path {
                                let _csv_lock =
                                    app_state.app_config.file_locks.write_lock(csv_path).await;
                                if let Err(err) = csv_patch_batch_update_exp_date(
                                    input.input_type,
                                    csv_path,
                                    &acct.name,
                                    acct.username.as_str(),
                                    acct.password.as_str(),
                                    new_exp,
                                )
                                .await
                                {
                                    debug_if_enabled!(
                                        "panel_api failed to persist renew exp_date to csv: {}",
                                        err
                                    );
                                } else {
                                    changed = true;
                                }
                            } else {
                                sources_yml_patches.push(SourcesYmlPatch::UpdateExpDate {
                                    input_name: input.name.clone(),
                                    account_name: acct.name.clone(),
                                    exp_date: new_exp,
                                });
                                changed = true;
                            }
                        }
                        continue;
                    }
                    Err(err) => {
                        debug_if_enabled!(
                            "panel_api client_renew failed for {}: {}",
                            sanitize_sensitive_info(&acct.name),
                            sanitize_sensitive_info(err.to_string().as_str())
                        );
                    }
                }
            }
        }

        if !new_enabled {
            break;
        }
        match panel_client_new(app_state.as_ref(), panel_cfg).await {
            Ok((username, password, base_url_from_resp)) => {
                let base_url = base_url_from_resp.unwrap_or_else(|| input.url.clone());
                let base_url =
                    get_base_url_from_str(base_url.as_str()).unwrap_or_else(|| base_url.clone());

                let alias_name =
                    derive_unique_alias_name_set(&existing_names, &input.name, &username);
                existing_names.insert(alias_name.clone().into());

                if adult_enabled {
                    if let Err(err) = panel_client_adult_content(
                        app_state.as_ref(),
                        panel_cfg,
                        Some((&username, &password)),
                    )
                    .await
                    {
                        debug_if_enabled!(
                            "panel_api client_adult_content failed for {}: {}",
                            sanitize_sensitive_info(&alias_name),
                            sanitize_sensitive_info(err.to_string().as_str())
                        );
                    }
                }

                let exp_date = panel_client_info(
                    app_state.as_ref(),
                    panel_cfg,
                    &username,
                    &password,
                    time_ctx,
                )
                .await
                .ok()
                .flatten();

                accounts.push(AccountCredentials {
                    name: alias_name.clone().into(),
                    username: username.clone(),
                    password: password.clone(),
                    exp_date,
                });
                provisioned = provisioned.saturating_add(1);

                if let Some(csv_path) = csv_path {
                    let batch_type = if input.input_type == InputType::Xtream {
                        InputType::XtreamBatch
                    } else if input.input_type == InputType::M3u {
                        InputType::M3uBatch
                    } else {
                        input.input_type
                    };
                    let _csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                    if let Err(err) = csv_patch_batch_append(
                        csv_path,
                        batch_type,
                        &alias_name,
                        &base_url,
                        &username,
                        &password,
                        exp_date,
                    )
                    .await
                    {
                        warn!("panel_api failed to append new account to csv: {err}");
                        break;
                    }
                    changed = true;
                } else {
                    sources_yml_patches.push(SourcesYmlPatch::AddAlias {
                        input_name: input.name.clone(),
                        alias_name: alias_name.into(),
                        base_url,
                        username,
                        password,
                        exp_date,
                    });
                    changed = true;
                }
            }
            Err(err) => {
                debug_if_enabled!(
                    "panel_api client_new failed: {}",
                    sanitize_sensitive_info(err.to_string().as_str())
                );
                break;
            }
        }
    }

    (changed, provisioned)
}

#[allow(clippy::too_many_lines)]
async fn sync_panel_api_for_input_on_boot(
    app_state: &Arc<AppState>,
    input: &Arc<ConfigInput>,
    sources_path: &Path,
) -> bool {
    let Some(panel_cfg) = input.panel_api.as_ref() else {
        return false;
    };
    if !panel_cfg.enabled || panel_cfg.url.trim().is_empty() {
        return false;
    }

    if let Err(err) = validate_panel_api_config(panel_cfg) {
        debug_if_enabled!(
            "panel_api boot sync skipped for {}: {}",
            sanitize_sensitive_info(&input.name),
            sanitize_sensitive_info(err.to_string().as_str())
        );
        return false;
    }
    let optional = resolve_panel_api_optional_flags(panel_cfg, &input.name);

    let input_name = &input.name;
    let _input_lock = app_state
        .app_config
        .file_locks
        .write_lock_str(format!("panel_api:{input_name}").as_str())
        .await;

    let mut any_change = false;
    let is_batch = input
        .t_batch_url
        .as_ref()
        .is_some_and(|u| !u.trim().is_empty());
    let batch_url = input.t_batch_url.as_deref().unwrap_or_default();
    let csv_path = if is_batch {
        get_csv_file_path(batch_url).ok()
    } else {
        None
    };
    let mut sources_yml_patches: Vec<SourcesYmlPatch> = Vec::new();
    let mut pending_sources_yml = false;

    let mut accounts = collect_accounts(input.as_ref());
    if panel_cfg.alias_pool.is_some() {
        sort_account_aliases_keep_root_first(&mut accounts, &input.name);
    }
    let mut existing_names: HashSet<Arc<str>> = accounts.iter().map(|a| a.name.clone()).collect();
    let mut newly_created_accounts: Vec<AccountCredentials> = Vec::new();
    let mut time_ctx: Option<PanelApiTimeContext> = None;
    let mut effective_now = get_current_timestamp();

    let cache_path = panel_api_time_cache_path(app_state.as_ref());
    let mut time_cache = load_panel_api_time_cache(app_state.as_ref(), &cache_path).await;
    let cached_entry = time_cache.inputs.get(input.name.as_ref()).cloned();

    if panel_cfg.alias_pool.is_some() {
        if let Some(csv_path) = csv_path.as_ref() {
            let _csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
            match csv_patch_batch_sort_by_exp_date(input.input_type, csv_path).await {
                Ok(true) => any_change = true,
                Ok(false) => {}
                Err(err) => debug_if_enabled!(
                    "panel_api boot/update failed to sort csv alias pool for {}: {}",
                    sanitize_sensitive_info(&input.name),
                    err
                ),
            }
        } else if input
            .aliases
            .as_ref()
            .is_some_and(|aliases| aliases_need_sort_config(aliases))
        {
            sources_yml_patches.push(SourcesYmlPatch::SortAliases {
                input_name: input.name.clone(),
            });
            pending_sources_yml = true;
        }
    }

    if let Some((root_username, root_password)) = extract_account_creds_from_input(input.as_ref()) {
        let user_info = fetch_root_user_api_info(app_state.as_ref(), input.as_ref()).await;
        let (root_exp_date, server_tz, skew_secs) = if let Some(info) = user_info {
            let local_now = i64::try_from(get_current_timestamp()).unwrap_or(0);
            let skew_secs = info.server_now_ts.unwrap_or(local_now) - local_now;
            (info.exp_date, info.server_tz, Some(skew_secs))
        } else {
            (None, None, None)
        };

        let panel_expire_raw = match panel_client_info_raw(
            app_state.as_ref(),
            panel_cfg,
            root_username.as_str(),
            root_password.as_str(),
        )
        .await
        {
            Ok(expire) => expire,
            Err(err) => {
                debug_if_enabled!(
                    "panel_api root client_info (raw) failed for {}: {}",
                    sanitize_sensitive_info(&input.name),
                    sanitize_sensitive_info(err.to_string().as_str())
                );
                None
            }
        };

        let mut expire_mode =
            resolve_panel_expire_mode(root_exp_date, panel_expire_raw.as_deref(), server_tz);
        let mut server_tz = server_tz;
        let mut skew_secs = skew_secs.or_else(|| cached_entry.as_ref().and_then(|e| e.skew_secs));

        if root_exp_date.is_none() {
            if let Some(cached) = cached_entry.as_ref() {
                expire_mode = cached.expire_mode;
                if server_tz.is_none() {
                    server_tz = parse_cached_tz(cached.server_tz.clone());
                }
                if skew_secs.is_none() {
                    skew_secs = cached.skew_secs;
                }
            }
        }

        time_ctx = Some(PanelApiTimeContext {
            expire_mode,
            server_tz,
        });
        effective_now = apply_clock_skew(get_current_timestamp(), skew_secs.unwrap_or_default());

        time_cache.inputs.insert(
            input.name.to_string(),
            PanelApiTimeCacheEntry {
                expire_mode,
                server_tz: server_tz.map(|tz| tz.name().to_string()),
                skew_secs,
            },
        );
        if let Err(err) =
            persist_panel_api_time_cache(app_state.as_ref(), &cache_path, &time_cache).await
        {
            debug_if_enabled!(
                "panel_api failed to persist time cache for {}: {}",
                sanitize_sensitive_info(&input.name),
                err
            );
        }

        let server_tz_name = server_tz.as_ref().map_or("none", |tz| Tz::name(*tz));
        debug_if_enabled!(
            "panel_api time context for input {}: expire_mode={:?}, tz={}, skew_secs={}",
            sanitize_sensitive_info(&input.name),
            expire_mode,
            server_tz_name,
            skew_secs.unwrap_or_default()
        );
    } else if let Some(cached) = cached_entry.as_ref() {
        let server_tz = parse_cached_tz(cached.server_tz.clone());
        time_ctx = Some(PanelApiTimeContext {
            expire_mode: cached.expire_mode,
            server_tz,
        });
        effective_now = apply_clock_skew(
            get_current_timestamp(),
            cached.skew_secs.unwrap_or_default(),
        );
        let server_tz_name = server_tz.as_ref().map_or("none", |tz| Tz::name(*tz));
        debug_if_enabled!(
            "panel_api time context fallback for input {}: expire_mode={:?}, tz={}, skew_secs={}",
            sanitize_sensitive_info(&input.name),
            cached.expire_mode,
            server_tz_name,
            cached.skew_secs.unwrap_or_default()
        );
    } else {
        debug_if_enabled!(
            "panel_api time context skipped for input {}: missing root credentials",
            sanitize_sensitive_info(&input.name)
        );
    }

    for acct in &mut accounts {
        let new_exp = match panel_client_info(
            app_state.as_ref(),
            panel_cfg,
            &acct.username,
            &acct.password,
            time_ctx.as_ref(),
        )
        .await
        {
            Ok(v) => v,
            Err(err) => {
                debug_if_enabled!(
                    "panel_api client_info failed for {}: {}",
                    sanitize_sensitive_info(&acct.name),
                    sanitize_sensitive_info(err.to_string().as_str())
                );
                None
            }
        };
        let Some(new_exp) = new_exp else {
            continue;
        };
        if acct.exp_date == Some(new_exp) {
            continue;
        }

        if let Some(csv_path) = csv_path.as_ref() {
            let _csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
            if let Err(err) = csv_patch_batch_update_exp_date(
                input.input_type,
                csv_path,
                &acct.name,
                &acct.username,
                &acct.password,
                new_exp,
            )
            .await
            {
                debug_if_enabled!(
                    "panel_api boot sync failed to persist exp_date to csv: {}",
                    err
                );
                continue;
            }
            any_change = true;
        } else {
            sources_yml_patches.push(SourcesYmlPatch::UpdateExpDate {
                input_name: input.name.clone(),
                account_name: acct.name.clone(),
                exp_date: new_exp,
            });
            pending_sources_yml = true;
        }
        acct.exp_date = Some(new_exp);
    }

    let offset_secs = panel_cfg
        .provisioning
        .offset
        .as_deref()
        .and_then(|v| parse_panel_api_provisioning_offset_secs(v).ok())
        .unwrap_or(0);

    let min_pool = resolve_alias_pool_min(app_state.as_ref(), &input.name, panel_cfg);
    let renew_enabled = optional.client_renew;
    let new_enabled = optional.client_new;
    let adult_enabled = optional.adult_content;
    let provisioning_enabled = renew_enabled || new_enabled;
    // Refresh/provision credentials on boot/update.
    // Root is handled first (may affect desired_aliases and avoids over-provisioning).
    let mut provisioned_root = 0_u16;
    let mut provisioned_aliases = 0_u16;

    if let Some(root_idx) = accounts.iter().position(|a| a.name == input.name) {
        let now = effective_now;
        let offset_deadline = now.saturating_add(offset_secs);
        let root_exp_date = accounts[root_idx].exp_date;
        let root_exp_missing = root_exp_date.is_none();
        let root_expired = match root_exp_date {
            Some(ts) => u64::try_from(ts)
                .map_or(true, |exp_ts| exp_ts <= now),
            None => false,
        };
        let root_expiring = match root_exp_date {
            Some(ts) => u64::try_from(ts)
                .map_or(true, |exp_ts| exp_ts > now && exp_ts <= offset_deadline),
            None => false,
        };
        let should_refresh_root = root_exp_missing || root_expired || root_expiring;

        let root_exp_display =
            root_exp_date.map_or_else(|| "None".to_string(), |ts| ts.to_string());
        debug_if_enabled!(
            "panel_api boot/update root status for input {} (offset={}s): exp_date={}, expired={}, expiring(offset)={}",
            sanitize_sensitive_info(&input.name),
            offset_secs,
            root_exp_display,
            root_expired,
            root_expiring
        );

        if should_refresh_root {
            if provisioning_enabled {
                let old_username = accounts[root_idx].username.clone();
                let old_password = accounts[root_idx].password.clone();

                debug_if_enabled!(
                "panel_api boot/update refreshing root account {} for input {} (exp_date={}, offset={}s)",
                sanitize_sensitive_info(&old_username),
                sanitize_sensitive_info(&input.name),
                root_exp_display,
                offset_secs
            );

                let (active_username, active_password, creds_changed) = if renew_enabled {
                    match panel_client_renew(
                        app_state.as_ref(),
                        panel_cfg,
                        old_username.as_str(),
                        old_password.as_str(),
                    )
                    .await
                    {
                        Ok(()) => {
                            provisioned_root = 1;
                            (old_username.clone(), old_password.clone(), false)
                        }
                        Err(err) => {
                            debug_if_enabled!(
                                "panel_api client_renew failed for root {}: {}",
                                sanitize_sensitive_info(&input.name),
                                sanitize_sensitive_info(err.to_string().as_str())
                            );
                            if new_enabled {
                                match panel_client_new(app_state.as_ref(), panel_cfg).await {
                                    Ok((new_username, new_password, _base_url_from_resp)) => {
                                        provisioned_root = 1;
                                        // Variant B: if the old root is still valid but within offset window,
                                        // keep it as a new alias entry so we don't lose usable credentials.
                                        let park_old_root_as_alias = root_expiring
                                            && !root_expired
                                            && root_exp_date.is_some();

                                        if park_old_root_as_alias {
                                            let base_url =
                                                get_base_url_from_str(input.url.as_str())
                                                    .unwrap_or_else(|| input.url.clone());
                                            let alias_name = derive_unique_alias_name_set(
                                                &existing_names,
                                                &input.name,
                                                old_username.as_str(),
                                            );
                                            existing_names.insert(alias_name.clone().into());

                                            if let Some(csv_path) = csv_path.as_ref() {
                                                let batch_type =
                                                    if input.input_type == InputType::Xtream {
                                                        InputType::XtreamBatch
                                                    } else if input.input_type == InputType::M3u {
                                                        InputType::M3uBatch
                                                    } else {
                                                        input.input_type
                                                    };
                                                let _csv_lock = app_state
                                                    .app_config
                                                    .file_locks
                                                    .write_lock(csv_path)
                                                    .await;
                                                if let Err(err) = csv_patch_batch_append(
                                                    csv_path,
                                                    batch_type,
                                                    &alias_name,
                                                    &base_url,
                                                    &old_username,
                                                    &old_password,
                                                    root_exp_date,
                                                )
                                                .await
                                                {
                                                    debug_if_enabled!(
                                                    "panel_api boot/update failed to park old root as csv alias {}: {}",
                                                    sanitize_sensitive_info(&alias_name),
                                                    err
                                                );
                                                } else {
                                                    any_change = true;
                                                }
                                            } else {
                                                sources_yml_patches.push(
                                                    SourcesYmlPatch::AddAlias {
                                                        input_name: input.name.clone(),
                                                        alias_name: alias_name.clone().into(),
                                                        base_url,
                                                        username: old_username.clone(),
                                                        password: old_password.clone(),
                                                        exp_date: root_exp_date,
                                                    },
                                                );
                                                pending_sources_yml = true;
                                            }

                                            accounts.push(AccountCredentials {
                                                name: alias_name.into(),
                                                username: old_username.clone(),
                                                password: old_password.clone(),
                                                exp_date: root_exp_date,
                                            });

                                            debug_if_enabled!(
                                            "panel_api boot/update parked old root credentials for input {} as new alias (exp_date={:?})",
                                            sanitize_sensitive_info(&input.name),
                                            root_exp_date
                                        );
                                        }

                                        if let Some(csv_path) = csv_path.as_ref() {
                                            let _csv_lock = app_state
                                                .app_config
                                                .file_locks
                                                .write_lock(csv_path)
                                                .await;
                                            if let Err(err) = csv_patch_batch_update_credentials(
                                                input.input_type,
                                                csv_path,
                                                &input.name,
                                                &old_username,
                                                &old_password,
                                                &new_username,
                                                &new_password,
                                                None,
                                            )
                                            .await
                                            {
                                                debug_if_enabled!(
                                                "panel_api boot/update failed to persist new root credentials to csv for {}: {}",
                                                sanitize_sensitive_info(&input.name),
                                                err
                                            );
                                            } else {
                                                any_change = true;
                                            }
                                        } else {
                                            sources_yml_patches.push(
                                                SourcesYmlPatch::UpdateRootCredentials {
                                                    input_name: input.name.clone(),
                                                    username: new_username.clone(),
                                                    password: new_password.clone(),
                                                    exp_date: None,
                                                },
                                            );
                                            pending_sources_yml = true;
                                        }

                                        accounts[root_idx].username.clone_from(&new_username);
                                        accounts[root_idx].password.clone_from(&new_password);
                                        (new_username, new_password, true)
                                    }
                                    Err(err) => {
                                        debug_if_enabled!(
                                            "panel_api client_new failed for root {}: {}",
                                            sanitize_sensitive_info(&input.name),
                                            sanitize_sensitive_info(err.to_string().as_str())
                                        );
                                        (old_username.clone(), old_password.clone(), false)
                                    }
                                }
                            } else {
                                (old_username.clone(), old_password.clone(), false)
                            }
                        }
                    }
                } else if new_enabled {
                    match panel_client_new(app_state.as_ref(), panel_cfg).await {
                        Ok((new_username, new_password, _base_url_from_resp)) => {
                            provisioned_root = 1;
                            let park_old_root_as_alias =
                                root_expiring && !root_expired && root_exp_date.is_some();

                            if park_old_root_as_alias {
                                let base_url = get_base_url_from_str(input.url.as_str())
                                    .unwrap_or_else(|| input.url.clone());
                                let alias_name = derive_unique_alias_name_set(
                                    &existing_names,
                                    &input.name,
                                    old_username.as_str(),
                                );
                                existing_names.insert(alias_name.clone().into());

                                if let Some(csv_path) = csv_path.as_ref() {
                                    let batch_type = if input.input_type == InputType::Xtream {
                                        InputType::XtreamBatch
                                    } else if input.input_type == InputType::M3u {
                                        InputType::M3uBatch
                                    } else {
                                        input.input_type
                                    };
                                    let _csv_lock =
                                        app_state.app_config.file_locks.write_lock(csv_path).await;
                                    if let Err(err) = csv_patch_batch_append(
                                        csv_path,
                                        batch_type,
                                        &alias_name,
                                        &base_url,
                                        &old_username,
                                        &old_password,
                                        root_exp_date,
                                    )
                                    .await
                                    {
                                        debug_if_enabled!(
                                        "panel_api boot/update failed to park old root as csv alias {}: {}",
                                        sanitize_sensitive_info(&alias_name),
                                        err
                                    );
                                    } else {
                                        any_change = true;
                                    }
                                } else {
                                    sources_yml_patches.push(SourcesYmlPatch::AddAlias {
                                        input_name: input.name.clone(),
                                        alias_name: alias_name.clone().into(),
                                        base_url,
                                        username: old_username.clone(),
                                        password: old_password.clone(),
                                        exp_date: root_exp_date,
                                    });
                                    pending_sources_yml = true;
                                }

                                accounts.push(AccountCredentials {
                                    name: alias_name.into(),
                                    username: old_username.clone(),
                                    password: old_password.clone(),
                                    exp_date: root_exp_date,
                                });

                                debug_if_enabled!(
                                "panel_api boot/update parked old root credentials for input {} as new alias (exp_date={:?})",
                                sanitize_sensitive_info(&input.name),
                                root_exp_date
                            );
                            }

                            if let Some(csv_path) = csv_path.as_ref() {
                                let _csv_lock =
                                    app_state.app_config.file_locks.write_lock(csv_path).await;
                                if let Err(err) = csv_patch_batch_update_credentials(
                                    input.input_type,
                                    csv_path,
                                    &input.name,
                                    &old_username,
                                    &old_password,
                                    &new_username,
                                    &new_password,
                                    None,
                                )
                                .await
                                {
                                    debug_if_enabled!(
                                    "panel_api boot/update failed to persist new root credentials to csv for {}: {}",
                                    sanitize_sensitive_info(&input.name),
                                    err
                                );
                                } else {
                                    any_change = true;
                                }
                            } else {
                                sources_yml_patches.push(SourcesYmlPatch::UpdateRootCredentials {
                                    input_name: input.name.clone(),
                                    username: new_username.clone(),
                                    password: new_password.clone(),
                                    exp_date: None,
                                });
                                pending_sources_yml = true;
                            }

                            accounts[root_idx].username.clone_from(&new_username);
                            accounts[root_idx].password.clone_from(&new_password);
                            (new_username, new_password, true)
                        }
                        Err(err) => {
                            debug_if_enabled!(
                                "panel_api client_new failed for root {}: {}",
                                sanitize_sensitive_info(&input.name),
                                sanitize_sensitive_info(err.to_string().as_str())
                            );
                            (old_username.clone(), old_password.clone(), false)
                        }
                    }
                } else {
                    (old_username.clone(), old_password.clone(), false)
                };

                if adult_enabled {
                    if let Err(err) = panel_client_adult_content(
                        app_state.as_ref(),
                        panel_cfg,
                        Some((active_username.as_str(), active_password.as_str())),
                    )
                    .await
                    {
                        debug_if_enabled!(
                            "panel_api client_adult_content failed for root {}: {}",
                            sanitize_sensitive_info(&input.name),
                            sanitize_sensitive_info(err.to_string().as_str())
                        );
                    }
                }

                let refreshed_exp = panel_client_info(
                    app_state.as_ref(),
                    panel_cfg,
                    active_username.as_str(),
                    active_password.as_str(),
                    time_ctx.as_ref(),
                )
                .await
                .ok()
                .flatten();

                let ready = wait_for_panel_api_account_ready(
                    app_state,
                    input.as_ref(),
                    panel_cfg,
                    &input.name,
                    active_username.as_str(),
                    active_password.as_str(),
                )
                .await;
                if !ready {
                    debug_if_enabled!(
                    "panel_api boot/update probe timeout for root {}; skipping exp_date refresh",
                    sanitize_sensitive_info(&input.name)
                );
                } else if let Some(new_exp) = refreshed_exp {
                    if let Some(csv_path) = csv_path.as_ref() {
                        let _csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                        let result = if creds_changed {
                            csv_patch_batch_update_credentials(
                                input.input_type,
                                csv_path,
                                &input.name,
                                old_username.as_str(),
                                old_password.as_str(),
                                active_username.as_str(),
                                active_password.as_str(),
                                Some(new_exp),
                            )
                            .await
                        } else {
                            csv_patch_batch_update_exp_date(
                                input.input_type,
                                csv_path,
                                &input.name,
                                old_username.as_str(),
                                old_password.as_str(),
                                new_exp,
                            )
                            .await
                        };
                        if let Err(err) = result {
                            debug_if_enabled!(
                            "panel_api boot/update failed to persist root exp_date to csv for {}: {}",
                            sanitize_sensitive_info(&input.name),
                            err
                        );
                        } else {
                            accounts[root_idx].exp_date = Some(new_exp);
                            any_change = true;
                        }
                    } else {
                        if creds_changed {
                            sources_yml_patches.push(SourcesYmlPatch::UpdateRootCredentials {
                                input_name: input.name.clone(),
                                username: active_username.clone(),
                                password: active_password.clone(),
                                exp_date: Some(new_exp),
                            });
                        } else {
                            sources_yml_patches.push(SourcesYmlPatch::UpdateExpDate {
                                input_name: input.name.clone(),
                                account_name: input.name.clone(),
                                exp_date: new_exp,
                            });
                        }
                        pending_sources_yml = true;
                        accounts[root_idx].exp_date = Some(new_exp);
                    }
                }
            } else {
                debug_if_enabled!(
                    "panel_api boot/update skipped root refresh for input {}: client_new/client_renew disabled",
                    sanitize_sensitive_info(&input.name)
                );
            }
        }
    } else {
        debug_if_enabled!(
            "panel_api boot/update skipped root provisioning for input {}: missing credentials",
            sanitize_sensitive_info(&input.name)
        );
    }

    // Plan and execute alias refresh after the root operation (avoids over-provisioning).
    let now = effective_now;
    let offset_deadline = now.saturating_add(offset_secs);
    let root_valid = root_counts_towards_pool_at(&accounts, &input.name, now);
    let desired_aliases = min_pool.filter(|m| *m > 0).map_or_else(
        || {
            u16::try_from(accounts.iter().filter(|a| a.name != input.name).count())
                .unwrap_or(u16::MAX)
        },
        |min_pool| min_pool.saturating_sub(u16::from(root_valid)),
    );

    let expiring_aliases = accounts
        .iter()
        .filter(|a| {
            a.name != input.name && is_expiring_with_offset_at(a.exp_date, offset_secs, now)
        })
        .count();
    let expired_aliases = accounts
        .iter()
        .filter(|a| a.name != input.name && is_input_expired_at(a.exp_date, now))
        .count();

    let valid_aliases_beyond_offset = accounts
        .iter()
        .filter(|a| {
            if a.name == input.name {
                return false;
            }
            if is_input_expired_at(a.exp_date, now) {
                return false;
            }
            match a.exp_date {
                Some(ts) => u64::try_from(ts)
                    .is_ok_and(|exp_ts| exp_ts > offset_deadline),
                None => false,
            }
        })
        .count();

    let desired_aliases_u16 = desired_aliases;
    let alias_total = accounts.iter().filter(|a| a.name != input.name).count();
    let alias_total_u16 = u16::try_from(alias_total).unwrap_or(u16::MAX);
    let missing_aliases_u16 = desired_aliases_u16.saturating_sub(alias_total_u16);

    let (refresh_plan, planned_refresh_aliases) = if provisioning_enabled {
        let mut refresh_candidates: Vec<usize> = accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                if a.name == input.name {
                    return false;
                }
                if is_input_expired_at(a.exp_date, now) {
                    return false;
                }
                match a.exp_date {
                    None => true,
                    Some(ts) => u64::try_from(ts)
                        .map_or(true, |exp_ts| exp_ts <= offset_deadline),
                }
            })
            .map(|(idx, _)| idx)
            .collect();

        refresh_candidates.sort_by_key(|idx| {
            let acct = &accounts[*idx];
            match acct.exp_date {
                None => (0_u8, i64::MIN),
                Some(ts) => (1_u8, ts),
            }
        });

        let valid_aliases_beyond_offset_u16 =
            u16::try_from(valid_aliases_beyond_offset).unwrap_or(u16::MAX);
        let needed_refresh_aliases_u16 =
            desired_aliases_u16.saturating_sub(valid_aliases_beyond_offset_u16);
        let planned_refresh_aliases = refresh_candidates
            .len()
            .min(usize::from(needed_refresh_aliases_u16));
        let refresh_plan: Vec<usize> = refresh_candidates
            .into_iter()
            .take(planned_refresh_aliases)
            .collect();
        (refresh_plan, planned_refresh_aliases)
    } else {
        (Vec::new(), 0)
    };

    let log_pool = alias_pool_has_min(panel_cfg);
    let enabled_users = if log_pool {
        count_enabled_proxy_users(app_state.as_ref(), &input.name)
    } else {
        0
    };
    if log_pool {
        debug_if_enabled!(
            "panel_api boot/update provisioning aliases for input {} (offset={}s): desired={}, valid_beyond_offset={}, expiring(offset)={}, expired={}, refresh_planned(offset)={}, missing={}",
            sanitize_sensitive_info(&input.name),
            offset_secs,
            desired_aliases_u16,
            valid_aliases_beyond_offset,
            expiring_aliases,
            expired_aliases,
            planned_refresh_aliases,
            missing_aliases_u16
        );
    }

    if !refresh_plan.is_empty() {
        debug_if_enabled!(
            "panel_api boot/update alias refresh plan for input {}: selected={}",
            sanitize_sensitive_info(&input.name),
            refresh_plan
                .iter()
                .filter_map(|idx| accounts.get(*idx))
                .map(|acct| sanitize_sensitive_info(&acct.name).to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    for idx in refresh_plan {
        let Some(acct) = accounts.get_mut(idx) else {
            continue;
        };
        let account_name = acct.name.clone();
        let old_username = acct.username.clone();
        let old_password = acct.password.clone();

        debug_if_enabled!(
            "panel_api boot/update refreshing alias account {} for input {} (exp_date={:?}, offset={}s)",
            sanitize_sensitive_info(&account_name),
            sanitize_sensitive_info(&input.name),
            acct.exp_date,
            offset_secs
        );

        let (active_username, active_password, creds_changed) = if renew_enabled {
            match panel_client_renew(
                app_state.as_ref(),
                panel_cfg,
                old_username.as_str(),
                old_password.as_str(),
            )
            .await
            {
                Ok(()) => {
                    provisioned_aliases = provisioned_aliases.saturating_add(1);
                    (old_username.clone(), old_password.clone(), false)
                }
                Err(err) => {
                    debug_if_enabled!(
                        "panel_api client_renew failed for alias {}: {}",
                        sanitize_sensitive_info(&account_name),
                        sanitize_sensitive_info(err.to_string().as_str())
                    );
                    if new_enabled {
                        match panel_client_new(app_state.as_ref(), panel_cfg).await {
                            Ok((new_username, new_password, base_url_from_resp)) => {
                                let base_url =
                                    base_url_from_resp.unwrap_or_else(|| input.url.clone());
                                let base_url = get_base_url_from_str(base_url.as_str())
                                    .unwrap_or_else(|| base_url.clone());

                                let alias_name = derive_unique_alias_name_set(
                                    &existing_names,
                                    &input.name,
                                    &new_username,
                                );
                                existing_names.insert(alias_name.clone().into());

                                if adult_enabled {
                                    if let Err(err) = panel_client_adult_content(
                                        app_state.as_ref(),
                                        panel_cfg,
                                        Some((new_username.as_str(), new_password.as_str())),
                                    )
                                    .await
                                    {
                                        debug_if_enabled!(
                                            "panel_api client_adult_content failed for {}: {}",
                                            sanitize_sensitive_info(&alias_name),
                                            sanitize_sensitive_info(err.to_string().as_str())
                                        );
                                    }
                                }

                                let exp_date = panel_client_info(
                                    app_state.as_ref(),
                                    panel_cfg,
                                    new_username.as_str(),
                                    new_password.as_str(),
                                    time_ctx.as_ref(),
                                )
                                .await
                                .ok()
                                .flatten();

                                if let Some(csv_path) = csv_path.as_ref() {
                                    let batch_type = if input.input_type == InputType::Xtream {
                                        InputType::XtreamBatch
                                    } else if input.input_type == InputType::M3u {
                                        InputType::M3uBatch
                                    } else {
                                        input.input_type
                                    };
                                    let _csv_lock =
                                        app_state.app_config.file_locks.write_lock(csv_path).await;
                                    if let Err(err) = csv_patch_batch_append(
                                        csv_path,
                                        batch_type,
                                        &alias_name,
                                        &base_url,
                                        &new_username,
                                        &new_password,
                                        exp_date,
                                    )
                                    .await
                                    {
                                        debug_if_enabled!(
                                            "panel_api boot/update failed to append new csv account for {}: {}",
                                            sanitize_sensitive_info(&alias_name),
                                            err
                                        );
                                        continue;
                                    }
                                    any_change = true;
                                } else {
                                    sources_yml_patches.push(SourcesYmlPatch::AddAlias {
                                        input_name: input.name.clone(),
                                        alias_name: alias_name.clone().into(),
                                        base_url,
                                        username: new_username.clone(),
                                        password: new_password.clone(),
                                        exp_date,
                                    });
                                    pending_sources_yml = true;
                                }

                                newly_created_accounts.push(AccountCredentials {
                                    name: alias_name.into(),
                                    username: new_username,
                                    password: new_password,
                                    exp_date,
                                });
                                provisioned_aliases = provisioned_aliases.saturating_add(1);
                                continue;
                            }
                            Err(err) => {
                                debug_if_enabled!(
                                    "panel_api client_new failed for input {}: {}",
                                    sanitize_sensitive_info(&input.name),
                                    sanitize_sensitive_info(err.to_string().as_str())
                                );
                                continue;
                            }
                        }
                    }
                    continue;
                }
            }
        } else if new_enabled {
            match panel_client_new(app_state.as_ref(), panel_cfg).await {
                Ok((new_username, new_password, base_url_from_resp)) => {
                    let base_url = base_url_from_resp.unwrap_or_else(|| input.url.clone());
                    let base_url = get_base_url_from_str(base_url.as_str())
                        .unwrap_or_else(|| base_url.clone());

                    let alias_name =
                        derive_unique_alias_name_set(&existing_names, &input.name, &new_username);
                    existing_names.insert(alias_name.clone().into());

                    if adult_enabled {
                        if let Err(err) = panel_client_adult_content(
                            app_state.as_ref(),
                            panel_cfg,
                            Some((new_username.as_str(), new_password.as_str())),
                        )
                        .await
                        {
                            debug_if_enabled!(
                                "panel_api client_adult_content failed for {}: {}",
                                sanitize_sensitive_info(&alias_name),
                                sanitize_sensitive_info(err.to_string().as_str())
                            );
                        }
                    }

                    let exp_date = panel_client_info(
                        app_state.as_ref(),
                        panel_cfg,
                        new_username.as_str(),
                        new_password.as_str(),
                        time_ctx.as_ref(),
                    )
                    .await
                    .ok()
                    .flatten();

                    if let Some(csv_path) = csv_path.as_ref() {
                        let batch_type = if input.input_type == InputType::Xtream {
                            InputType::XtreamBatch
                        } else if input.input_type == InputType::M3u {
                            InputType::M3uBatch
                        } else {
                            input.input_type
                        };
                        let _csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                        if let Err(err) = csv_patch_batch_append(
                            csv_path,
                            batch_type,
                            &alias_name,
                            &base_url,
                            &new_username,
                            &new_password,
                            exp_date,
                        )
                        .await
                        {
                            debug_if_enabled!(
                                "panel_api boot/update failed to append new csv account for {}: {}",
                                sanitize_sensitive_info(&alias_name),
                                err
                            );
                            continue;
                        }
                        any_change = true;
                    } else {
                        sources_yml_patches.push(SourcesYmlPatch::AddAlias {
                            input_name: input.name.clone(),
                            alias_name: alias_name.clone().into(),
                            base_url,
                            username: new_username.clone(),
                            password: new_password.clone(),
                            exp_date,
                        });
                        pending_sources_yml = true;
                    }

                    newly_created_accounts.push(AccountCredentials {
                        name: alias_name.into(),
                        username: new_username,
                        password: new_password,
                        exp_date,
                    });
                    provisioned_aliases = provisioned_aliases.saturating_add(1);
                    continue;
                }
                Err(err) => {
                    debug_if_enabled!(
                        "panel_api client_new failed for input {}: {}",
                        sanitize_sensitive_info(&input.name),
                        sanitize_sensitive_info(err.to_string().as_str())
                    );
                    continue;
                }
            }
        } else {
            continue;
        };

        if adult_enabled {
            if let Err(err) = panel_client_adult_content(
                app_state.as_ref(),
                panel_cfg,
                Some((active_username.as_str(), active_password.as_str())),
            )
            .await
            {
                debug_if_enabled!(
                    "panel_api client_adult_content failed for {}: {}",
                    sanitize_sensitive_info(&account_name),
                    sanitize_sensitive_info(err.to_string().as_str())
                );
            }
        }

        let refreshed_exp = panel_client_info(
            app_state.as_ref(),
            panel_cfg,
            active_username.as_str(),
            active_password.as_str(),
            time_ctx.as_ref(),
        )
        .await
        .ok()
        .flatten();

        if let Some(new_exp) = refreshed_exp {
            if let Some(csv_path) = csv_path.as_ref() {
                let _csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                let result = if creds_changed {
                    csv_patch_batch_update_credentials(
                        input.input_type,
                        csv_path,
                        &account_name,
                        &old_username,
                        &old_password,
                        active_username.as_str(),
                        active_password.as_str(),
                        Some(new_exp),
                    )
                    .await
                } else {
                    csv_patch_batch_update_exp_date(
                        input.input_type,
                        csv_path,
                        &account_name,
                        &old_username,
                        &old_password,
                        new_exp,
                    )
                    .await
                };
                if let Err(err) = result {
                    debug_if_enabled!(
                        "panel_api boot/update failed to persist exp_date to csv for {}: {}",
                        sanitize_sensitive_info(&account_name),
                        err
                    );
                } else {
                    acct.exp_date = Some(new_exp);
                    any_change = true;
                }
            } else {
                if creds_changed {
                    sources_yml_patches.push(SourcesYmlPatch::UpdateAliasCredentials {
                        input_name: input.name.clone(),
                        alias_name: account_name.clone(),
                        username: active_username.clone(),
                        password: active_password.clone(),
                        exp_date: Some(new_exp),
                    });
                } else {
                    sources_yml_patches.push(SourcesYmlPatch::UpdateExpDate {
                        input_name: input.name.clone(),
                        account_name: account_name.clone(),
                        exp_date: new_exp,
                    });
                }
                pending_sources_yml = true;
                acct.exp_date = Some(new_exp);
            }
        } else {
            debug_if_enabled!(
                "panel_api boot/update renew/create succeeded but exp_date refresh failed for {}",
                sanitize_sensitive_info(&account_name)
            );
        }
    }

    if !newly_created_accounts.is_empty() {
        accounts.extend(newly_created_accounts);
    }

    if adult_enabled {
        for acct in &accounts {
            if let Err(err) = panel_client_adult_content(
                app_state.as_ref(),
                panel_cfg,
                Some((acct.username.as_str(), acct.password.as_str())),
            )
            .await
            {
                debug_if_enabled!(
                    "panel_api client_adult_content failed for {}: {}",
                    sanitize_sensitive_info(&acct.name),
                    sanitize_sensitive_info(err.to_string().as_str())
                );
            }
        }
    }

    let min_pool = min_pool.filter(|m| *m > 0);
    if let Some(min_pool) = min_pool {
        let root_valid = root_counts_towards_pool(&accounts, &input.name);
        let alias_min = min_pool.saturating_sub(u16::from(root_valid));
        let (pool_changed, pool_provisioned) = ensure_alias_pool_min(
            app_state,
            input.as_ref(),
            panel_cfg,
            &mut accounts,
            alias_min,
            csv_path.as_deref(),
            &mut sources_yml_patches,
            time_ctx.as_ref(),
            effective_now,
            optional,
        )
        .await;
        if pool_changed {
            if csv_path.is_some() {
                any_change = true;
            } else {
                pending_sources_yml = true;
            }
        }
        provisioned_aliases = provisioned_aliases.saturating_add(pool_provisioned);
    }

    if log_pool {
        let valid_total = count_valid_accounts_at(&accounts, now);
        debug_if_enabled!(
            "panel_api boot/update provisioning total for input {} (offset={}s): enabled_users={}, valid_accounts={}, provisioned_root={}, provisioned_aliases={}",
            sanitize_sensitive_info(&input.name),
            offset_secs,
            enabled_users,
            valid_total,
            provisioned_root,
            provisioned_aliases
        );
    }

    if alias_pool_remove_expired(panel_cfg) {
        if let Some(csv_path) = csv_path.as_ref() {
            let _csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
            match csv_patch_batch_remove_expired(input.input_type, csv_path).await {
                Ok(true) => any_change = true,
                Ok(false) => {}
                Err(err) => debug_if_enabled!(
                    "panel_api boot sync failed to remove expired csv accounts: {}",
                    err
                ),
            }
        } else {
            sources_yml_patches.push(SourcesYmlPatch::RemoveExpiredAliases {
                input_name: input.name.clone(),
            });
            pending_sources_yml = true;
        }
    }

    if optional.account_info {
        let creds = accounts
            .first()
            .map(|acct| (acct.username.as_str(), acct.password.as_str()));
        match panel_account_info(app_state.as_ref(), panel_cfg, creds).await {
            Ok(Some(credits)) => {
                let normalized = credits.trim().to_string();
                if !normalized.is_empty()
                    /* && panel_cfg.credits.as_deref().map(str::trim) != Some(normalized.as_str()) */
                {
                    sources_yml_patches.push(SourcesYmlPatch::UpdatePanelApiCredits {
                        input_name: input.name.clone(),
                        credits: normalized,
                    });
                    pending_sources_yml = true;
                }
            }
            Ok(None) => {}
            Err(err) => {
                debug_if_enabled!(
                    "panel_api account_info failed for {}: {}",
                    sanitize_sensitive_info(&input.name),
                    sanitize_sensitive_info(err.to_string().as_str())
                );
            }
        }
    }

    if pending_sources_yml {
        let _src_lock = app_state
            .app_config
            .file_locks
            .write_lock(sources_path)
            .await;
        match persist_sources_yml_with_patches(app_state, sources_path, &sources_yml_patches).await
        {
            Ok(true) => any_change = true,
            Ok(false) => {}
            Err(err) => debug_if_enabled!(
                "panel_api boot sync failed to persist source.yml patches: {}",
                err
            ),
        }
    }

    any_change
}

pub(crate) async fn sync_panel_api_exp_dates_on_boot(app_state: &Arc<AppState>) {
    let sources_file_path = app_state.app_config.paths.load().sources_file_path.clone();
    let sources_path = PathBuf::from(&sources_file_path);
    let mut any_change = false;

    let sources = app_state.app_config.sources.load();
    for input in &sources.inputs {
        if sync_panel_api_for_input_on_boot(app_state, input, sources_path.as_path()).await {
            any_change = true;
        }
    }

    if any_change {
        // Even with `config_hot_reload=true`, the file watcher reload is asynchronous.
        // Reload immediately so subsequent routines use updated credentials (e.g. after root renewal).
        if let Err(err) = ConfigFile::load_sources(app_state).await {
            debug_if_enabled!("panel_api boot/update reload sources failed: {}", err);
        }
    }
}

pub(crate) async fn sync_panel_api_alias_pool_for_target(
    app_state: &Arc<AppState>,
    target_name: &str,
) {
    let sources_file_path = app_state.app_config.paths.load().sources_file_path.clone();
    let sources_path = PathBuf::from(&sources_file_path);
    let mut any_change = false;

    let sources = app_state.app_config.sources.load();
    for source in &sources.sources {
        let target_match = source
            .targets
            .iter()
            .any(|target| target.name.eq_ignore_ascii_case(target_name));
        if !target_match {
            continue;
        }

        for input_name in &source.inputs {
            let Some(input) = sources.get_input_by_name(input_name) else {
                continue;
            };
            let Some(panel_cfg) = input.panel_api.as_ref() else {
                continue;
            };
            if !panel_cfg.enabled || panel_cfg.url.trim().is_empty() {
                continue;
            }
            if let Err(err) = validate_panel_api_config(panel_cfg) {
                debug_if_enabled!(
                    "panel_api user sync skipped for {}: {}",
                    sanitize_sensitive_info(&input.name),
                    sanitize_sensitive_info(err.to_string().as_str())
                );
                continue;
            }
            if !alias_pool_has_min(panel_cfg) {
                continue;
            }

            if sync_panel_api_for_input_on_boot(app_state, input, sources_path.as_path()).await {
                any_change = true;
            }
        }
    }

    if any_change && should_reload_sources_after_internal_write(app_state.as_ref()) {
        if let Err(err) = ConfigFile::load_sources(app_state).await {
            debug_if_enabled!("panel_api user sync reload sources failed: {}", err);
        }
    }
}

fn provisioning_method_to_reqwest(method: PanelApiProvisioningMethod) -> Method {
    match method {
        PanelApiProvisioningMethod::Head => Method::HEAD,
        PanelApiProvisioningMethod::Get => Method::GET,
        PanelApiProvisioningMethod::Post => Method::POST,
    }
}

fn build_player_api_action_url(
    base_url: &str,
    username: &str,
    password: &str,
    action: &str,
) -> Option<Url> {
    let url = Url::parse(base_url).ok()?;
    let host = url.host_str()?;
    let scheme = url.scheme();
    let mut base = concat_string!(scheme, "://", host);
    if let Some(port) = url.port() {
        base.push(':');
        base.push_str(port.to_string().as_str());
    }
    base.push_str("/player_api.php");
    let mut test_url = Url::parse(&base).ok()?;
    test_url
        .query_pairs_mut()
        .append_pair("username", username)
        .append_pair("password", password)
        .append_pair("action", action);
    Some(test_url)
}

fn build_panel_api_test_url(base_url: &str, username: &str, password: &str) -> Option<Url> {
    build_player_api_action_url(base_url, username, password, "account_info")
}

enum PanelApiProbeTarget {
    PlayerApi { action: &'static str, url: Url },
}

impl PanelApiProbeTarget {
    fn action(&self) -> &'static str {
        match self {
            PanelApiProbeTarget::PlayerApi { action, .. } => action,
        }
    }
}

fn build_panel_api_probe_targets(
    input: &ConfigInput,
    username: &str,
    password: &str,
) -> Vec<PanelApiProbeTarget> {
    let mut targets = Vec::new();
    for action in [
        "client_info",
        "get_live_categories",
        "get_series_categories",
        "get_vod_categories",
    ] {
        if let Some(url) =
            build_player_api_action_url(input.url.as_str(), username, password, action)
        {
            targets.push(PanelApiProbeTarget::PlayerApi { action, url });
        }
    }
    targets
}

async fn probe_panel_api_targets(
    app_state: &Arc<AppState>,
    probe_method: PanelApiProvisioningMethod,
    targets: &[PanelApiProbeTarget],
    done: &mut HashSet<&'static str>,
) -> bool {
    for target in targets {
        let action = target.action();
        if done.contains(action) {
            continue;
        }
        match target {
            PanelApiProbeTarget::PlayerApi { action, url } => {
                match probe_panel_api_test_url(app_state, url, probe_method).await {
                    Ok(status) => {
                        debug_if_enabled!(
                            "panel_api probe status: '{}' action={} url: {}",
                            format_http_status(status),
                            action,
                            sanitize_sensitive_info(url.as_str())
                        );
                        if status.is_success() {
                            done.insert(action);
                        }
                    }
                    Err(err) => {
                        if err.is_timeout() {
                            debug_if_enabled!(
                                "panel_api probe timeout action={} url: {}",
                                action,
                                sanitize_sensitive_info(url.as_str())
                            );
                        } else {
                            debug_if_enabled!(
                                "panel_api probe failed action={} url: {}: {err}",
                                action,
                                sanitize_sensitive_info(url.as_str())
                            );
                        }
                    }
                }
            }
        }
    }
    done.len() == targets.len()
}

async fn probe_panel_api_test_url(
    app_state: &Arc<AppState>,
    test_url: &Url,
    method: PanelApiProvisioningMethod,
) -> Result<StatusCode, reqwest::Error> {
    let client = app_state.http_client.load();
    let request_method = provisioning_method_to_reqwest(method);
    let response = client
        .request(request_method, test_url.clone())
        .send()
        .await?;
    Ok(response.status())
}

async fn apply_provisioning_cooldown(
    panel_cfg: &PanelApiConfig,
    account_name: &str,
    input_name: &Arc<str>,
) {
    let cooldown_secs = panel_cfg.provisioning.cooldown_sec;
    if cooldown_secs == 0 {
        return;
    }
    debug_if_enabled!(
        "panel_api provisioning cooldown for {} (input={}): {}s",
        sanitize_sensitive_info(account_name),
        sanitize_sensitive_info(input_name),
        cooldown_secs
    );
    tokio::time::sleep(Duration::from_secs(cooldown_secs)).await;
}

async fn wait_for_panel_api_account_ready(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    account_name: &str,
    username: &str,
    password: &str,
) -> bool {
    let max_wait_secs = panel_cfg.provisioning.timeout_sec;
    let probe_interval_secs = panel_cfg.provisioning.probe_interval_sec.max(1);
    let probe_method = panel_cfg.provisioning.method;

    let probe_targets = build_panel_api_probe_targets(input, username, password);
    if probe_targets.is_empty() {
        debug_if_enabled!(
            "panel_api probe skipped for {} (input={}): no probe targets",
            sanitize_sensitive_info(account_name),
            sanitize_sensitive_info(&input.name)
        );
        return false;
    }

    let targets_list = probe_targets
        .iter()
        .map(PanelApiProbeTarget::action)
        .collect::<Vec<_>>()
        .join(",");
    debug_if_enabled!(
        "panel_api probe start for {} (input={} timeout={}s interval={}s method={}) targets={}",
        sanitize_sensitive_info(account_name),
        sanitize_sensitive_info(&input.name),
        max_wait_secs,
        probe_interval_secs,
        probe_method,
        targets_list
    );

    let deadline = Instant::now() + Duration::from_secs(max_wait_secs);
    let probe_delay = Duration::from_secs(probe_interval_secs);
    let mut done_targets = HashSet::new();
    let mut attempt = 0u64;
    loop {
        attempt += 1;
        debug_if_enabled!("panel_api probe attempt {}", attempt);
        if probe_panel_api_targets(app_state, probe_method, &probe_targets, &mut done_targets).await
        {
            apply_provisioning_cooldown(panel_cfg, account_name, &input.name).await;
            return true;
        }

        if max_wait_secs == 0 {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline.checked_duration_since(now).unwrap_or_default();
        let sleep_for = if remaining < probe_delay {
            remaining
        } else {
            probe_delay
        };
        tokio::time::sleep(sleep_for).await;
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run_panel_api_provisioning_probe(
    app_state: Arc<AppState>,
    input: ConfigInput,
    stop_signal: Arc<AtomicOnceFlag>,
    addr: SocketAddr,
    virtual_id: VirtualId,
) {
    let Some(panel_cfg) = input.panel_api.as_ref() else {
        debug_if_enabled!(
            "panel_api provisioning probe skipped (missing config) for input {}",
            sanitize_sensitive_info(&input.name)
        );
        stop_signal.notify();
        let _ = app_state
            .connection_manager
            .kick_connection(&addr, virtual_id, 0)
            .await;
        return;
    };
    if !panel_cfg.enabled {
        debug_if_enabled!(
            "panel_api provisioning probe skipped (panel_api.enabled false) for input {}",
            sanitize_sensitive_info(&input.name)
        );
        stop_signal.notify();
        let _ = app_state
            .connection_manager
            .kick_connection(&addr, virtual_id, 0)
            .await;
        return;
    }
    if panel_cfg.url.trim().is_empty() {
        debug_if_enabled!(
            "panel_api provisioning probe skipped (panel_api.url empty) for input {}",
            sanitize_sensitive_info(&input.name)
        );
        stop_signal.notify();
        let _ = app_state
            .connection_manager
            .kick_connection(&addr, virtual_id, 0)
            .await;
        return;
    }

    let max_wait_secs = panel_cfg.provisioning.timeout_sec;
    let probe_interval_secs = panel_cfg.provisioning.probe_interval_sec.max(1);
    let probe_method = panel_cfg.provisioning.method;

    debug_if_enabled!(
        "panel_api provisioning probe start for input {} (timeout={}s interval={}s method={})",
        sanitize_sensitive_info(&input.name),
        max_wait_secs,
        probe_interval_secs,
        probe_method
    );

    let deadline = Instant::now() + Duration::from_secs(max_wait_secs);
    let outcome = try_provision_account_on_exhausted(&app_state, &input).await;
    let credentials = outcome.as_ref().map(PanelApiProvisionOutcome::credentials);

    if let Some(outcome) = outcome.as_ref() {
        debug_if_enabled!(
            "panel_api provisioning {} completed for input {}",
            outcome.kind_label(),
            sanitize_sensitive_info(&input.name)
        );
    } else {
        debug_if_enabled!(
            "panel_api provisioning failed for input {}; waiting for timeout",
            sanitize_sensitive_info(&input.name)
        );
    }

    let Some((username, password)) = credentials else {
        if max_wait_secs > 0 {
            tokio::time::sleep(Duration::from_secs(max_wait_secs)).await;
        }
        debug_if_enabled!(
            "panel_api provisioning probe timeout reached for input {} (no credentials)",
            sanitize_sensitive_info(&input.name)
        );
        stop_signal.notify();
        debug_if_enabled!(
            "panel_api provisioning closing client connection for input {} addr={}",
            sanitize_sensitive_info(&input.name),
            sanitize_sensitive_info(addr.to_string().as_str())
        );
        let _ = app_state
            .connection_manager
            .kick_connection(&addr, virtual_id, 0)
            .await;
        return;
    };

    let Some(test_url) = build_panel_api_test_url(input.url.as_str(), username, password) else {
        if max_wait_secs > 0 {
            tokio::time::sleep(Duration::from_secs(max_wait_secs)).await;
        }
        debug_if_enabled!(
            "panel_api provisioning probe failed to build test url for input {}",
            sanitize_sensitive_info(&input.name)
        );
        stop_signal.notify();
        let _ = app_state
            .connection_manager
            .kick_connection(&addr, virtual_id, 0)
            .await;
        return;
    };

    let probe_delay = Duration::from_secs(probe_interval_secs);
    let mut attempt = 0u64;
    let mut ready = false;
    while Instant::now() < deadline {
        attempt += 1;
        debug_if_enabled!("panel_api provisioning probe attempt {}", attempt);
        match probe_panel_api_test_url(&app_state, &test_url, probe_method).await {
            Ok(status) => {
                debug_if_enabled!(
                    "panel_api provisioning probe status: '{}' url: {}",
                    format_http_status(status),
                    sanitize_sensitive_info(test_url.as_str())
                );
                if status.is_success() {
                    ready = true;
                    break;
                }
            }
            Err(err) => {
                if err.is_timeout() {
                    debug_if_enabled!(
                        "panel_api provisioning probe timeout for {}",
                        sanitize_sensitive_info(test_url.as_str())
                    );
                } else {
                    debug_if_enabled!(
                        "panel_api provisioning probe failed for {}: {err}",
                        sanitize_sensitive_info(test_url.as_str())
                    );
                }
            }
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.checked_duration_since(now).unwrap_or_default();
        let sleep_for = if remaining < probe_delay {
            remaining
        } else {
            probe_delay
        };
        tokio::time::sleep(sleep_for).await;
    }

    if ready {
        debug_if_enabled!(
            "panel_api provisioning ready for input {} (attempts={})",
            sanitize_sensitive_info(&input.name),
            attempt
        );
    } else {
        debug_if_enabled!(
            "panel_api provisioning probe timeout reached for input {} (attempts={})",
            sanitize_sensitive_info(&input.name),
            attempt
        );
    }
    stop_signal.notify();
    debug_if_enabled!(
        "panel_api provisioning closing client connection for input {} addr={}",
        sanitize_sensitive_info(&input.name),
        sanitize_sensitive_info(addr.to_string().as_str())
    );
    let _ = app_state
        .connection_manager
        .kick_connection(&addr, virtual_id, 0)
        .await;
}

pub fn create_panel_api_provisioning_stream_details(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    provider_name: Option<Arc<str>>,
    grace_period_options: &GracePeriodOptions,
    addr: SocketAddr,
    virtual_id: VirtualId,
) -> StreamDetails {
    let stop_signal = Arc::new(AtomicOnceFlag::new());
    let headers = [("connection".to_string(), "close".to_string())];
    let (stream, stream_info) = create_panel_api_provisioning_stream_with_stop(
        &app_state.app_config,
        &headers,
        Arc::clone(&stop_signal),
    );

    if stream.is_none() {
        debug_if_enabled!(
            "panel_api provisioning stream missing; falling back to provider exhausted for input {}",
            sanitize_sensitive_info(&input.name)
        );
        let (stream, stream_info) =
            create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
        return StreamDetails {
            stream,
            stream_info,
            provider_name,
            grace_period: *grace_period_options,
            disable_provider_grace: true,
            reconnect_flag: None,
            provider_handle: None,
        };
    }

    let app_state_clone = Arc::clone(app_state);
    let input_clone = input.clone();
    let stop_clone = Arc::clone(&stop_signal);
    tokio::spawn(async move {
        run_panel_api_provisioning_probe(
            app_state_clone,
            input_clone,
            stop_clone,
            addr,
            virtual_id,
        )
        .await;
    });

    StreamDetails {
        stream,
        stream_info,
        provider_name,
        grace_period: *grace_period_options,
        disable_provider_grace: true,
        reconnect_flag: None,
        provider_handle: None,
    }
}
