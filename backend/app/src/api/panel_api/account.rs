use super::{
    build_panel_url, build_player_api_action_url, extract_account_creds_from_input, extract_stringish,
    extract_username_password_from_json, first_json_object, normalize_panel_expire, panel_get_json, parse_boolish,
    resolve_query_params, user_api_get_json, validate_account_info_params, validate_client_adult_content_params,
    validate_client_info_params, validate_client_new_params, validate_client_renew_params, PanelApiTimeContext,
};
use crate::{
    api::model::AppState,
    model::{ConfigInput, InputSource, PanelApiConfig},
    utils::debug_if_enabled,
};
use chrono_tz::Tz;
use shared::{
    error::TuliproxError,
    utils::{
        get_base_url_from_str, get_credentials_from_url_str, get_i64_from_serde_value, get_string_from_serde_value,
        sanitize_sensitive_info,
    },
};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(super) struct UserApiAccountInfo {
    pub(super) exp_date: Option<i64>,
    pub(super) server_now_ts: Option<i64>,
    pub(super) server_tz: Option<Tz>,
}

pub(super) fn build_user_api_account_info_input_source(
    input: &ConfigInput,
    username: &str,
    password: &str,
) -> Result<InputSource, TuliproxError> {
    let url = build_player_api_action_url(input.url.as_str(), username, password, "account_info").ok_or_else(|| {
        TuliproxError::ConfigPanelApi(format!(
            "panel_api: invalid user_api base_url: {}",
            sanitize_sensitive_info(input.url.as_str())
        ))
    })?;

    Ok(InputSource::from(input).with_url(url.to_string()))
}

pub(super) async fn panel_client_new(
    app_state: &AppState,
    cfg: &PanelApiConfig,
) -> Result<(String, String, Option<String>), TuliproxError> {
    validate_client_new_params(&cfg.query_parameter.client_new)?;
    let params = resolve_query_params(&cfg.query_parameter.client_new, cfg.api_key.as_deref(), None)?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return Err(TuliproxError::ConfigPanelApi(
            "panel_api: client_new response is not a JSON object/array".to_string(),
        ));
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return Err(TuliproxError::ConfigPanelApi("panel_api: client_new status=false".to_string()));
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
    Err(TuliproxError::ConfigPanelApi(
        "panel_api: client_new response missing username/password (and no parsable url)".to_string(),
    ))
}

pub(super) async fn panel_client_renew(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    username: &str,
    password: &str,
) -> Result<(), TuliproxError> {
    validate_client_renew_params(&cfg.query_parameter.client_renew)?;
    let params =
        resolve_query_params(&cfg.query_parameter.client_renew, cfg.api_key.as_deref(), Some((username, password)))?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return Err(TuliproxError::ConfigPanelApi(
            "panel_api: client_renew response is not a JSON object/array".to_string(),
        ));
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return Err(TuliproxError::ConfigPanelApi("panel_api: client_renew status=false".to_string()));
    }
    Ok(())
}

pub(super) async fn panel_client_info_raw(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    username: &str,
    password: &str,
) -> Result<Option<String>, TuliproxError> {
    validate_client_info_params(&cfg.query_parameter.client_info)?;
    let params =
        resolve_query_params(&cfg.query_parameter.client_info, cfg.api_key.as_deref(), Some((username, password)))?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return Err(TuliproxError::ConfigPanelApi(
            "panel_api: client_info response is not a JSON object/array".to_string(),
        ));
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return Err(TuliproxError::ConfigPanelApi("panel_api: client_info status=false".to_string()));
    }
    let expire = obj.get("expire").and_then(|v| v.as_str()).unwrap_or_default().trim().to_string();
    if expire.is_empty() {
        Ok(None)
    } else {
        Ok(Some(expire))
    }
}

pub(super) async fn panel_client_info(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    username: &str,
    password: &str,
    time_ctx: Option<&PanelApiTimeContext>,
) -> Result<Option<i64>, TuliproxError> {
    let expire = panel_client_info_raw(app_state, cfg, username, password).await?;
    Ok(expire.as_deref().and_then(|value| normalize_panel_expire(value, time_ctx)))
}

pub(super) async fn fetch_root_user_api_info(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
) -> Result<Option<UserApiAccountInfo>, TuliproxError> {
    let Some((username, password)) = extract_account_creds_from_input(input) else {
        return Ok(None);
    };

    let input_source = build_user_api_account_info_input_source(input, username.as_ref(), password.as_ref())?;

    let json = match user_api_get_json(app_state, &input_source).await {
        Ok(json) => json,
        Err(err) => {
            debug_if_enabled!(
                "panel_api user_api account_info failed for {}: {}",
                sanitize_sensitive_info(&input.name),
                sanitize_sensitive_info(&err.to_string())
            );
            return Ok(None);
        }
    };

    let exp_date = json.get("user_info").and_then(|v| v.get("exp_date")).and_then(get_i64_from_serde_value);
    let server_now_ts = json.get("server_info").and_then(|v| v.get("timestamp_now")).and_then(get_i64_from_serde_value);
    let server_tz = json
        .get("server_info")
        .and_then(|v| v.get("timezone"))
        .and_then(get_string_from_serde_value)
        .and_then(|tz| tz.parse::<Tz>().ok());

    Ok(Some(UserApiAccountInfo { exp_date, server_now_ts, server_tz }))
}

pub(super) async fn panel_account_info(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    creds: Option<(&str, &str)>,
) -> Result<Option<String>, TuliproxError> {
    if cfg.query_parameter.account_info.is_empty() {
        return Ok(None);
    }
    validate_account_info_params(&cfg.query_parameter.account_info)?;
    let params = resolve_query_params(&cfg.query_parameter.account_info, cfg.api_key.as_deref(), creds)?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return Err(TuliproxError::ConfigPanelApi(
            "panel_api: account_info response is not a JSON object/array".to_string(),
        ));
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return Err(TuliproxError::ConfigPanelApi("panel_api: account_info status=false".to_string()));
    }
    let Some(credits) = obj.get("credits").and_then(extract_stringish) else {
        return Err(TuliproxError::ConfigPanelApi("panel_api: account_info response missing credits".to_string()));
    };
    Ok(Some(credits))
}

pub(super) async fn panel_client_adult_content(
    app_state: &AppState,
    cfg: &PanelApiConfig,
    creds: Option<(&str, &str)>,
) -> Result<(), TuliproxError> {
    if cfg.query_parameter.client_adult_content.is_empty() {
        return Ok(());
    }
    validate_client_adult_content_params(&cfg.query_parameter.client_adult_content)?;
    let params = resolve_query_params(&cfg.query_parameter.client_adult_content, cfg.api_key.as_deref(), creds)?;
    let url = build_panel_url(cfg.url.as_ref(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return Err(TuliproxError::ConfigPanelApi(
            "panel_api: client_adult_content response is not a JSON object/array".to_string(),
        ));
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return Err(TuliproxError::ConfigPanelApi("panel_api: client_adult_content status=false".to_string()));
    }
    Ok(())
}
