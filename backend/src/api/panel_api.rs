use crate::api::config_file::ConfigFile;
use crate::api::model::AppState;
use crate::model::{is_input_expired, ConfigInput, PanelApiConfig, PanelApiQueryParam};
use crate::utils::{debug_if_enabled, persist_source_config, read_sources_file_from_path};
use crate::utils::get_csv_file_path;
use log::{debug, error, warn};
use serde_json::Value;
use shared::error::{info_err_res, info_err, TuliproxError};
use shared::model::{ConfigInputAliasDto, InputType};
use shared::utils::{get_credentials_from_url, parse_timestamp, sanitize_sensitive_info, trim_last_slash};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use url::Url;

#[derive(Debug, Clone)]
struct AccountCredentials {
    name: String,
    username: String,
    password: String,
    exp_date: Option<i64>,
}

fn parse_boolish(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        Value::String(s) => matches!(s.trim().to_lowercase().as_str(), "true" | "1" | "yes" | "y" | "ok"),
        _ => false,
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

fn first_json_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    match value {
        Value::Array(arr) => arr.first().and_then(|v| v.as_object()),
        Value::Object(obj) => Some(obj),
        _ => None,
    }
}

fn extract_username_password_from_json(obj: &serde_json::Map<String, Value>) -> Option<(String, String)> {
    let username = obj.get("username").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let password = obj.get("password").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    match (username, password) {
        (Some(u), Some(p)) => Some((u.to_string(), p.to_string())),
        _ => None,
    }
}

fn extract_username_password_from_url(url_str: &str) -> Option<(String, String)> {
    Url::parse(url_str).ok().and_then(|url| {
        let (u, p) = get_credentials_from_url(&url);
        match (u, p) {
            (Some(u), Some(p)) if !u.trim().is_empty() && !p.trim().is_empty() => Some((u, p)),
            _ => None,
        }
    })
}

fn extract_base_url(url_str: &str) -> Option<String> {
    Url::parse(url_str).ok().map(|u| u.origin().ascii_serialization())
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
                    return info_err_res!("panel_api: query param {key} uses 'auto' but panel_api.api_key is missing");
                };
                value = k.to_string();
            } else if key.eq_ignore_ascii_case("username") {
                let Some((u, _)) = creds else {
                    return info_err_res!("panel_api: query param {key} uses 'auto' but no account username is available");
                };
                value = u.to_string();
            } else if key.eq_ignore_ascii_case("password") {
                let Some((_, pw)) = creds else {
                    return info_err_res!("panel_api: query param {key} uses 'auto' but no account password is available");
                };
                value = pw.to_string();
            }
        }
        out.push((key.to_string(), value));
    }
    Ok(out)
}

fn build_panel_url(base_url: &str, query_params: &[(String, String)]) -> Result<Url, TuliproxError> {
    let mut url = Url::parse(base_url).map_err(|e| info_err!("panel_api: invalid url {base_url}: {e}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query_params {
            pairs.append_pair(k, v);
        }
    }
    Ok(url)
}

fn sanitize_panel_api_json_for_log(value: &Value) -> Value {
    match value {
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_panel_api_json_for_log).collect()),
        Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                if k.eq_ignore_ascii_case("api_key") || k.eq_ignore_ascii_case("apikey") || k.eq_ignore_ascii_case("token") {
                    out.insert(k.clone(), Value::String("***".to_string()));
                    continue;
                }
                if k.eq_ignore_ascii_case("username") || k.eq_ignore_ascii_case("password") {
                    out.insert(k.clone(), Value::String("***".to_string()));
                    continue;
                }
                if k.eq_ignore_ascii_case("url") {
                    if let Some(s) = v.as_str() {
                        out.insert(k.clone(), Value::String(sanitize_sensitive_info(s).into_owned()));
                        continue;
                    }
                }
                out.insert(k.clone(), sanitize_panel_api_json_for_log(v));
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
        .map_err(|e|info_err!("panel_api request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| info_err!("panel_api read response failed: {e}"))?;
    let json: Value = serde_json::from_str(&body)
        .map_err(|e| info_err!("panel_api invalid json (http {status}): {e}"))?;
    let json_for_log = sanitize_panel_api_json_for_log(&json);
    if let Ok(json_str) = serde_json::to_string(&json_for_log) {
        debug_if_enabled!("panel_api response (http {}): {}", status, sanitize_sensitive_info(&json_str));
    }
    Ok(json)
}

async fn panel_client_new(app_state: &AppState, cfg: &PanelApiConfig) -> Result<(String, String, Option<String>), TuliproxError> {
    let params = resolve_query_params(&cfg.query_parameter.client_new, cfg.api_key.as_deref(), None)?;
    let url = build_panel_url(cfg.url.as_str(), &params)?;
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
        if let Some((u, p)) = extract_username_password_from_url(url_str) {
            let base = extract_base_url(url_str);
            return Ok((u, p, base));
        }
    }
    info_err_res!("panel_api: client_new response missing username/password (and no parsable url)")
}

async fn panel_client_renew(app_state: &AppState, cfg: &PanelApiConfig, username: &str, password: &str) -> Result<(), TuliproxError> {
    let params = resolve_query_params(
        &cfg.query_parameter.client_renew,
        cfg.api_key.as_deref(),
        Some((username, password)),
    )?;
    let url = build_panel_url(cfg.url.as_str(), &params)?;
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

async fn panel_client_info(app_state: &AppState, cfg: &PanelApiConfig, username: &str, password: &str) -> Result<Option<i64>, TuliproxError> {
    let params = resolve_query_params(
        &cfg.query_parameter.client_info,
        cfg.api_key.as_deref(),
        Some((username, password)),
    )?;
    let url = build_panel_url(cfg.url.as_str(), &params)?;
    let json = panel_get_json(app_state, url).await?;
    let Some(obj) = first_json_object(&json) else {
        return info_err_res!("panel_api: client_info response is not a JSON object/array");
    };
    let status_ok = obj.get("status").is_some_and(parse_boolish);
    if !status_ok {
        return info_err_res!("panel_api: client_info status=false");
    }
    let expire = obj.get("expire").and_then(|v| v.as_str()).unwrap_or_default().trim();
    let parsed = parse_timestamp(expire).ok().flatten();
    if parsed.is_some() {
        return Ok(parsed);
    }

    // Some panels return only a date ("YYYY-MM-DD") without time. Normalize to midnight.
    if is_date_only_yyyy_mm_dd(expire) {
        let normalized = format!("{expire} 00:00:00");
        return Ok(parse_timestamp(&normalized).ok().flatten());
    }

    Ok(None)
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
            (Some(uu), Some(pp)) if !uu.trim().is_empty() && !pp.trim().is_empty() => Some((uu, pp)),
            _ => None,
        }
    })
}

fn collect_expired_accounts(input: &ConfigInput) -> Vec<AccountCredentials> {
    let mut out = Vec::new();
    if is_input_expired(input.exp_date) {
        if let Some((u, p)) = extract_account_creds_from_input(input) {
            out.push(AccountCredentials {
                name: input.name.clone(),
                username: u,
                password: p,
                exp_date: input.exp_date,
            });
        }
    }
    if let Some(aliases) = input.aliases.as_ref() {
        for a in aliases {
            if is_input_expired(a.exp_date) {
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
    }
    out.sort_by_key(|a| a.exp_date.unwrap_or(i64::MAX));
    out
}

#[allow(clippy::too_many_arguments)]
async fn patch_source_yml_add_alias(
    app_state: &Arc<AppState>,
    source_file_path: &Path,
    input_name: &str,
    alias_name: &str,
    base_url: &str,
    username: &str,
    password: &str,
    exp_date: Option<i64>,
) -> Result<(), TuliproxError> {
    let mut sources = match read_sources_file_from_path(source_file_path, false, false, None) {
        Ok(sources) => sources,
        Err(e) => return info_err_res!("panel_api: failed to read source file: {e}"),
    };

    let Some(input) = sources.inputs.iter_mut().find(|i| i.name == input_name) else {
        return info_err_res!("panel_api: could not find input '{input_name}' in source.yml");
    };

    let alias = ConfigInputAliasDto {
        id: 0,
        name: alias_name.to_string(),
        url: base_url.to_string(),
        username: Some(username.to_string()),
        password: Some(password.to_string()),
        priority: 0,
        max_connections: 0,
        exp_date,
    };

    input.upsert_alias(alias)?;

    persist_source_config(app_state, Some(source_file_path), sources).await?;
    Ok(())
}

async fn patch_source_yml_update_exp_date(
    app_state: &Arc<AppState>,
    source_file_path: &Path,
    input_name: &str,
    account_name: &str,
    exp_date: i64,
) -> Result<(), TuliproxError> {
    let mut sources = match read_sources_file_from_path(source_file_path, false, false, None) {
        Ok(sources) => sources,
        Err(e) => return info_err_res!("panel_api: failed to read source file: {e}"),
    };

    let Some(input) = sources.inputs.iter_mut().find(|i| i.name == input_name) else {
        return info_err_res!("panel_api: could not find input '{input_name}' in source.yml");
    };

    if input.update_account_expiration_date(input_name, account_name, exp_date).is_err() {
        return info_err_res!("panel_api: could not find account '{account_name}' under input '{input_name}' in source.yml");
    }

    persist_source_config(app_state, Some(source_file_path), sources).await?;
    Ok(())
}

async fn patch_batch_csv_append(
    csv_path: &Path,
    batch_type: InputType,
    alias_name: &str,
    base_url: &str,
    username: &str,
    password: &str,
    exp_date: Option<i64>,
) -> Result<(), TuliproxError> {
    let raw = tokio::fs::read_to_string(csv_path).await.unwrap_or_default();
    let mut lines: Vec<String> = raw.lines().map(ToString::to_string).collect();
    let header_line_idx = lines.iter().position(|l| l.trim_start().starts_with('#'));
    let header = header_line_idx
        .and_then(|idx| lines.get(idx).map(|s| s.trim_start_matches('#').trim().to_string()))
        .unwrap_or_else(|| match batch_type {
            InputType::XtreamBatch => "name;username;password;url;max_connections;priority;exp_date".to_string(),
            _ => "url;max_connections;priority;name;username;password;exp_date".to_string(),
        });
    let cols: Vec<String> = header.split(';').map(|s| s.trim().to_lowercase()).collect();
    if header_line_idx.is_none() {
        lines.insert(0, format!("#{header}"));
    }
    let mut record: Vec<String> = vec![String::new(); cols.len()];
    for (i, c) in cols.iter().enumerate() {
        record[i] = match c.as_str() {
            "name" => alias_name.to_string(),
            "username" => username.to_string(),
            "password" => password.to_string(),
            "url" => {
                if batch_type == InputType::M3uBatch {
                    format!(
                        "{}/get.php?username={}&password={}&type=m3u_plus",
                        trim_last_slash(base_url),
                        username,
                        password
                    )
                } else {
                    base_url.to_string()
                }
            }
            "max_connections" => "1".to_string(),
            "priority" => "0".to_string(),
            "exp_date" => exp_date.map_or(String::new(), |ts| ts.to_string()),
            _ => String::new(),
        };
    }
    lines.push(record.join(";"));
    tokio::fs::write(csv_path, lines.join("\n") + "\n")
        .await
        .map_err(|e| info_err!("panel_api: failed to write csv: {e}"))?;
    Ok(())
}

async fn patch_batch_csv_update_exp_date(
    csv_path: &Path,
    account_name: &str,
    username: &str,
    password: &str,
    exp_date: i64,
) -> Result<(), TuliproxError> {
    let raw = tokio::fs::read_to_string(csv_path)
        .await
        .map_err(|e| info_err!("panel_api: failed to read csv: {e}"))?;
    let mut lines: Vec<String> = raw.lines().map(ToString::to_string).collect();
    let header_line_idx = lines.iter().position(|l| l.trim_start().starts_with('#'));
    let Some(header_idx) = header_line_idx else {
        return info_err_res!("panel_api: csv missing header line");
    };
    let header = lines[header_idx].trim_start_matches('#').trim();
    let cols: Vec<String> = header.split(';').map(|s| s.trim().to_lowercase()).collect();
    let exp_idx = cols.iter().position(|c| c == "exp_date");
    let name_idx = cols.iter().position(|c| c == "name");
    let user_idx = cols.iter().position(|c| c == "username");
    let pass_idx = cols.iter().position(|c| c == "password");
    let url_idx = cols.iter().position(|c| c == "url");
    let Some(exp_idx) = exp_idx else {
        debug!("panel_api: csv has no exp_date column; skipping exp_date persistence");
        return Ok(());
    };

    for i in (header_idx + 1)..lines.len() {
        let line = lines[i].trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields: Vec<String> = line.split(';').map(ToString::to_string).collect();
        fields.resize(cols.len(), String::new());

        let mut matches = false;
        if let Some(n_idx) = name_idx {
            if fields.get(n_idx).map(|s| s.trim()) == Some(account_name) {
                matches = true;
            }
        }
        if !matches {
            if let (Some(u_idx), Some(p_idx)) = (user_idx, pass_idx) {
                matches = fields.get(u_idx).map(|s| s.trim()) == Some(username)
                    && fields.get(p_idx).map(|s| s.trim()) == Some(password);
            } else if let Some(u_idx) = url_idx {
                if let Some(url_str) = fields.get(u_idx) {
                    if let Some((u, p)) = extract_username_password_from_url(url_str) {
                        matches = u == username && p == password;
                    }
                }
            }
        }
        if matches {
            fields[exp_idx] = exp_date.to_string();
            lines[i] = fields.join(";");
            tokio::fs::write(csv_path, lines.join("\n") + "\n")
                .await
                .map_err(|e| info_err!("panel_api: failed to write csv: {e}"))?;
            return Ok(());
        }
    }
    warn!("panel_api: could not find batch csv row for account {account_name}");
    Ok(())
}

const MAX_ALIAS_NAME_ATTEMPTS: usize = 1000;

fn derive_unique_alias_name(existing: &[String], input_name: &str, username: &str) -> String {
    let base = format!("{input_name}-{username}");
    if !existing.contains(&base) {
        return base;
    }
    for i in 2..MAX_ALIAS_NAME_ATTEMPTS {
        let cand = format!("{base}-{i}");
        if !existing.contains(&cand) {
            return cand;
        }
    }
    warn!("derive_unique_alias_name: exhausted {MAX_ALIAS_NAME_ATTEMPTS} attempts for base '{base}'; returning potentially duplicate name");
    base
}

async fn try_renew_expired_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    is_batch: bool,
    sources_path: &Path,
) -> bool {
    let expired = collect_expired_accounts(input);
    for acct in &expired {
        match panel_client_renew(app_state, panel_cfg, acct.username.as_str(), acct.password.as_str()).await {
            Ok(()) => {
                let refreshed_exp = panel_client_info(app_state, panel_cfg, acct.username.as_str(), acct.password.as_str())
                    .await
                    .ok()
                    .flatten();

                if let Some(new_exp) = refreshed_exp.or(acct.exp_date) {
                    if is_batch {
                        let batch_url = input.t_batch_url.as_deref().unwrap_or_default();
                        if let Ok(csv_path) = get_csv_file_path(batch_url) {
                            let _csv_lock = app_state.app_config.file_locks.write_lock(&csv_path).await;
                            if let Err(err) = patch_batch_csv_update_exp_date(&csv_path, &acct.name, &acct.username, &acct.password, new_exp).await {
                                debug_if_enabled!("panel_api failed to persist renew exp_date to csv: {}", err);
                            }
                        }
                    } else {
                        let _src_lock = app_state.app_config.file_locks.write_lock(sources_path).await;
                        if let Err(err) = patch_source_yml_update_exp_date(app_state, sources_path, &input.name, &acct.name, new_exp).await {
                            debug_if_enabled!("panel_api failed to persist renew exp_date to source.yml: {}", err);
                        }
                    }
                }

                if let Err(err) = ConfigFile::load_sources(app_state).await {
                    debug_if_enabled!("panel_api reload sources failed: {}", err);
                }
                return true;
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
    false
}

async fn try_create_new_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    is_batch: bool,
    sources_path: &Path,
) -> bool {
    match panel_client_new(app_state, panel_cfg).await {
        Ok((username, password, base_url_from_resp)) => {
            let base_url = base_url_from_resp.unwrap_or_else(|| input.url.clone());
            let base_url = extract_base_url(base_url.as_str()).unwrap_or_else(|| base_url.clone());

            let mut existing_names: Vec<String> = vec![input.name.clone()];
            if let Some(aliases) = input.aliases.as_ref() {
                existing_names.extend(aliases.iter().map(|a| a.name.clone()));
            }
            let alias_name = derive_unique_alias_name(&existing_names, &input.name, &username);

            let exp_date = panel_client_info(app_state, panel_cfg, &username, &password)
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
                        if let Err(err) =
                            patch_batch_csv_append(&csv_path, batch_type, &alias_name, &base_url, &username, &password, exp_date).await
                        {
                            warn!("panel_api failed to append new account to csv: {err}");
                            return false;
                        }
                    }
                    Err(err) => {
                        warn!(
                            "panel_api cannot resolve batch csv path {}: {}",
                            sanitize_sensitive_info(batch_url),
                            err
                        );
                        return false;
                    }
                }
            } else {
                let _src_lock = app_state.app_config.file_locks.write_lock(sources_path).await;
                if let Err(err) =
                    patch_source_yml_add_alias(app_state, sources_path, &input.name, &alias_name, &base_url, &username, &password, exp_date).await
                {
                    warn!("panel_api failed to persist new alias to source.yml: {err}");
                    return false;
                }
            }

            if let Err(err) = ConfigFile::load_sources(app_state).await {
                error!("panel_api reload sources failed: {err}");
                return false;
            }
            true
        }
        Err(err) => {
            debug_if_enabled!("panel_api client_new failed: {}", sanitize_sensitive_info(err.to_string().as_str()));
            false
        }
    }
}

pub async fn try_provision_account_on_exhausted(app_state: &Arc<AppState>, input: &ConfigInput) -> bool {
    let Some(panel_cfg) = input.panel_api.as_ref() else {
        debug_if_enabled!("panel_api: skipped (no panel_api config) for input {}", sanitize_sensitive_info(&input.name));
        return false;
    };
    if !panel_cfg.enabled {
        debug_if_enabled!("panel_api: skipped (panel_api.enabled false) for input {}", sanitize_sensitive_info(&input.name));
        return false;
    }

    if panel_cfg.url.trim().is_empty() {
        debug_if_enabled!("panel_api: skipped (panel_api.url empty) for input {}", sanitize_sensitive_info(&input.name));
        return false;
    }

    let _input_lock = app_state
        .app_config
        .file_locks
        .write_lock_str(format!("panel_api:{}", input.name).as_str())
        .await;

    debug_if_enabled!(
        "panel_api: exhausted -> provisioning for input {} (aliases={})",
        sanitize_sensitive_info(&input.name),
        input.aliases.as_ref().map_or(0, Vec::len)
    );

    let is_batch = input.t_batch_url.as_ref().is_some_and(|u| !u.trim().is_empty());
    let sources_file_path = app_state.app_config.paths.load().sources_file_path.clone();
    let sources_path = PathBuf::from(&sources_file_path);

    if try_renew_expired_account(app_state, input, panel_cfg, is_batch, sources_path.as_path()).await {
        debug_if_enabled!("panel_api: provisioning succeeded via client_renew for input {}", sanitize_sensitive_info(&input.name));
        return true;
    }
    let created = try_create_new_account(app_state, input, panel_cfg, is_batch, sources_path.as_path()).await;
    debug_if_enabled!(
        "panel_api: provisioning via client_new for input {} => {}",
        sanitize_sensitive_info(&input.name),
        if created { "success" } else { "failed" }
    );
    created
}

pub(crate) async fn sync_panel_api_exp_dates_on_boot(app_state: &Arc<AppState>) {
    let sources_file_path = app_state.app_config.paths.load().sources_file_path.clone();
    let sources_path = PathBuf::from(&sources_file_path);
    let mut any_change = false;

    let sources = app_state.app_config.sources.load();
    for input in &sources.inputs {
        let Some(panel_cfg) = input.panel_api.as_ref() else { continue; };
        if !panel_cfg.enabled || panel_cfg.url.trim().is_empty() {
            continue;
        }

        let is_batch = input.t_batch_url.as_ref().is_some_and(|u| !u.trim().is_empty());
        let batch_url = input.t_batch_url.as_deref().unwrap_or_default();
        let csv_path = if is_batch { get_csv_file_path(batch_url).ok() } else { None };

        let mut accounts: Vec<AccountCredentials> = vec![];
        if let Some((u, p)) = extract_account_creds_from_input(input.as_ref()) {
            accounts.push(AccountCredentials {
                name: input.name.clone(),
                username: u,
                password: p,
                exp_date: input.exp_date,
            });
        }
        if let Some(aliases) = input.aliases.as_ref() {
            for a in aliases {
                if let (Some(u), Some(p)) = (a.username.as_deref(), a.password.as_deref()) {
                    accounts.push(AccountCredentials {
                        name: a.name.clone(),
                        username: u.to_string(),
                        password: p.to_string(),
                        exp_date: a.exp_date,
                    });
                }
            }
        }

        for acct in &accounts {
            let new_exp = match panel_client_info(app_state.as_ref(), panel_cfg, &acct.username, &acct.password).await {
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
            let Some(new_exp) = new_exp else { continue; };
            if acct.exp_date == Some(new_exp) {
                continue;
            }

            if let Some(csv_path) = csv_path.as_ref() {
                let _csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                if let Err(err) = patch_batch_csv_update_exp_date(csv_path, &acct.name, &acct.username, &acct.password, new_exp).await {
                    debug_if_enabled!("panel_api boot sync failed to persist exp_date to csv: {}", err);
                    continue;
                }
            } else {
                let _src_lock = app_state.app_config.file_locks.write_lock(&sources_path).await;
                if let Err(err) = patch_source_yml_update_exp_date(app_state, &sources_path, &input.name, &acct.name, new_exp).await {
                    debug_if_enabled!("panel_api boot sync failed to persist exp_date to source.yml: {}", err);
                    continue;
                }
            }
            any_change = true;
        }
    }

    if any_change {
        if let Err(err) = ConfigFile::load_sources(app_state).await {
            debug_if_enabled!("panel_api boot sync reload sources failed: {}", err);
        }
    }
}
