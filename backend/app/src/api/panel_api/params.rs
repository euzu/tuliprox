use super::{alias_pool_limit_values, parse_panel_api_provisioning_offset_secs, resolve_batch_alias_path};
use crate::model::{ConfigInput, PanelApiConfig, PanelApiQueryParam};
use serde_json::Value;
use shared::{error::TuliproxError, model::PanelApiAliasPoolSizeValue};
use std::{path::PathBuf, sync::Arc};
use url::Url;

#[derive(Debug, Clone)]
pub(super) struct AccountCredentials {
    pub(super) name: Arc<str>,
    pub(super) username: String,
    pub(super) password: String,
    pub(super) exp_date: Option<i64>,
}

pub(super) fn parse_boolish(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        Value::String(s) => matches!(s.trim().to_lowercase().as_str(), "true" | "1" | "yes" | "y" | "ok"),
        _ => false,
    }
}

pub(super) fn extract_stringish(value: &Value) -> Option<String> {
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

pub(super) fn first_json_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    match value {
        Value::Array(arr) => arr.first().and_then(|v| v.as_object()),
        Value::Object(obj) => Some(obj),
        _ => None,
    }
}

pub(super) fn extract_username_password_from_json(obj: &serde_json::Map<String, Value>) -> Option<(String, String)> {
    let username = obj.get("username").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let password = obj.get("password").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    match (username, password) {
        (Some(u), Some(p)) => Some((u.to_string(), p.to_string())),
        _ => None,
    }
}

pub(super) fn validate_type_is_m3u(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    let typ = params.iter().find(|p| p.key.trim().eq_ignore_ascii_case("type")).map(|p| p.value.trim().to_string());
    let typ_str = typ.as_deref().unwrap_or_default();
    if typ_str.trim().eq_ignore_ascii_case("m3u") {
        Ok(())
    } else if typ_str.is_empty() {
        Err(TuliproxError::ConfigPanelApi("panel_api: missing required query param 'type=m3u'".to_string()))
    } else {
        Err(TuliproxError::ConfigPanelApi(format!("panel_api: unsupported type={typ_str}, only m3u is supported")))
    }
}

pub(super) fn require_api_key_param(params: &[PanelApiQueryParam], section: &str) -> Result<(), TuliproxError> {
    let api_key = params.iter().find(|p| p.key.trim().eq_ignore_ascii_case("api_key"));
    let Some(api_key) = api_key else {
        return Err(TuliproxError::ConfigPanelApi(format!(
            "panel_api: {section} must contain query param 'api_key' (use value 'auto')"
        )));
    };
    if api_key.value.trim().is_empty() {
        return Err(TuliproxError::ConfigPanelApi(format!(
            "panel_api: {section} query param 'api_key' must not be empty (use value 'auto')"
        )));
    }
    Ok(())
}

pub(super) fn require_username_password_params_auto(
    params: &[PanelApiQueryParam],
    section: &str,
) -> Result<(), TuliproxError> {
    let username = params.iter().find(|p| p.key.trim().eq_ignore_ascii_case("username"));
    let password = params.iter().find(|p| p.key.trim().eq_ignore_ascii_case("password"));
    if username.is_none() || password.is_none() {
        return Err(TuliproxError::ConfigPanelApi(format!(
            "panel_api: {section} must contain query params 'username' and 'password' (use value 'auto')"
        )));
    }
    if !username.is_some_and(|p| p.value.trim().eq_ignore_ascii_case("auto"))
        || !password.is_some_and(|p| p.value.trim().eq_ignore_ascii_case("auto"))
    {
        return Err(TuliproxError::ConfigPanelApi(format!(
            "panel_api: {section} requires 'username: auto' and 'password: auto' (credentials must not be hardcoded)"
        )));
    }
    Ok(())
}

pub(super) fn validate_client_new_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.client_new")?;
    validate_type_is_m3u(params)?;
    if params.iter().any(|p| p.key.trim().eq_ignore_ascii_case("user")) {
        return Err(TuliproxError::ConfigPanelApi(
            "panel_api: client_new must not contain query param 'user'".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn validate_client_renew_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.client_renew")?;
    validate_type_is_m3u(params)?;
    require_username_password_params_auto(params, "query_parameter.client_renew")?;
    Ok(())
}

pub(super) fn validate_client_info_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.client_info")?;
    require_username_password_params_auto(params, "query_parameter.client_info")?;
    Ok(())
}

pub(super) fn validate_account_info_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.account_info")?;
    let has_user = params.iter().any(|p| p.key.trim().eq_ignore_ascii_case("username"));
    let has_pass = params.iter().any(|p| p.key.trim().eq_ignore_ascii_case("password"));
    if has_user || has_pass {
        require_username_password_params_auto(params, "query_parameter.account_info")?;
    }
    Ok(())
}

pub(super) fn validate_client_adult_content_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
    require_api_key_param(params, "query_parameter.client_adult_content")?;
    let has_user = params.iter().any(|p| p.key.trim().eq_ignore_ascii_case("username"));
    let has_pass = params.iter().any(|p| p.key.trim().eq_ignore_ascii_case("password"));
    if has_user || has_pass {
        require_username_password_params_auto(params, "query_parameter.client_adult_content")?;
    }
    Ok(())
}

pub(super) fn validate_panel_api_config(cfg: &PanelApiConfig) -> Result<(), TuliproxError> {
    if !cfg.enabled {
        return Ok(());
    }
    if cfg.url.trim().is_empty() {
        return Err(TuliproxError::ConfigPanelApi("panel_api: url is missing".to_string()));
    }
    if cfg.api_key.as_ref().is_none_or(|k| k.trim().is_empty()) {
        return Err(TuliproxError::ConfigPanelApi("panel_api: api_key is missing".to_string()));
    }
    if cfg.query_parameter.client_info.is_empty() {
        return Err(TuliproxError::ConfigPanelApi(
            "panel_api: query_parameter.client_info must be configured".to_string(),
        ));
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
            return Err(TuliproxError::ConfigPanelApi(
                "panel_api.alias_pool.size.min must be greater than 0".to_string(),
            ));
        }
    }
    if let Some(PanelApiAliasPoolSizeValue::Number(value)) = max_val {
        if *value == 0 {
            return Err(TuliproxError::ConfigPanelApi(
                "panel_api.alias_pool.size.max must be greater than 0".to_string(),
            ));
        }
    }
    let min = min_val.and_then(PanelApiAliasPoolSizeValue::as_number);
    let max = max_val.and_then(PanelApiAliasPoolSizeValue::as_number);
    if let (Some(min), Some(max)) = (min, max) {
        if min > max {
            return Err(TuliproxError::ConfigPanelApi(
                "panel_api.alias_pool.size.min must be <= panel_api.alias_pool.size.max".to_string(),
            ));
        }
    }
    if cfg.provisioning.probe_interval_sec == 0 {
        return Err(TuliproxError::ConfigPanelApi(
            "panel_api.provisioning.probe_interval_sec must be greater than 0".to_string(),
        ));
    }
    if let Some(offset) = cfg.provisioning.offset.as_deref() {
        let _secs = parse_panel_api_provisioning_offset_secs(offset)?;
    }
    Ok(())
}

pub(super) fn resolve_query_params(
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
                    return Err(TuliproxError::ConfigPanelApi(format!(
                        "panel_api: query param {key} uses 'auto' but panel_api.api_key is missing"
                    )));
                };
                value = k.to_string();
            } else if key.eq_ignore_ascii_case("username") {
                let Some((u, _)) = creds else {
                    return Err(TuliproxError::ConfigPanelApi(format!(
                        "panel_api: query param {key} uses 'auto' but no account username is available"
                    )));
                };
                value = u.to_string();
            } else if key.eq_ignore_ascii_case("password") {
                let Some((_, pw)) = creds else {
                    return Err(TuliproxError::ConfigPanelApi(format!(
                        "panel_api: query param {key} uses 'auto' but no account password is available"
                    )));
                };
                value = pw.to_string();
            }
        }
        out.push((key.to_string(), value));
    }
    Ok(out)
}

pub(super) fn build_panel_url(base_url: &str, query_params: &[(String, String)]) -> Result<Url, TuliproxError> {
    let mut url = Url::parse(base_url)
        .map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api: invalid url {base_url}: {e}")))?;
    {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query_params {
            pairs.append_pair(k, v);
        }
    }
    Ok(url)
}

pub(super) fn require_batch_alias_path(input: &ConfigInput) -> Result<PathBuf, TuliproxError> {
    resolve_batch_alias_path(input.t_batch_url.as_deref())?.ok_or_else(|| {
        TuliproxError::ConfigInput(format!("batch input '{}' does not define a CSV alias path", input.name))
    })
}
