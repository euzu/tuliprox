use super::{
    panel_client_adult_content, panel_client_info, panel_client_new, panel_client_renew, refresh_internal_sources,
    should_reload_sources_after_internal_write, sync_panel_api_for_input_on_boot, validate_panel_api_config,
    AccountCredentials, PanelApiOptionalFlags, PanelApiOptionalFlagsSet, PanelApiTimeContext,
};
use crate::{
    api::{
        internal_csv::{csv_patch_batch_append, csv_patch_batch_update_exp_date},
        model::AppState,
        source_yml_patch::{derive_unique_alias_name_set, resolve_provisioned_account_base_url, SourcesYmlPatch},
    },
    model::{
        is_input_expired, is_input_expired_at, ConfigInput, ConfigInputAlias, PanelApiConfig, ProxyUserCredentials,
    },
    utils::debug_if_enabled,
};
use log::warn;
use shared::{
    error::TuliproxError,
    model::{InputType, PanelApiAliasPoolSizeValue, ProxyUserStatus},
    utils::{get_credentials_from_url, sanitize_sensitive_info},
};
use smallvec::SmallVec;
use std::{
    cmp::Ordering,
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use url::Url;

pub(super) fn extract_account_creds_from_input(input: &ConfigInput) -> Option<(String, String)> {
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

pub(super) fn alias_pool_limit_values(
    cfg: &PanelApiConfig,
) -> (Option<&PanelApiAliasPoolSizeValue>, Option<&PanelApiAliasPoolSizeValue>) {
    let size = cfg.alias_pool.as_ref().and_then(|p| p.size.as_ref());
    let min = size.and_then(|s| s.min.as_ref());
    let max = size.and_then(|s| s.max.as_ref());
    (min, max)
}

pub(super) fn alias_pool_has_min(cfg: &PanelApiConfig) -> bool {
    let (min, _) = alias_pool_limit_values(cfg);
    min.is_some()
}

pub(super) fn resolve_alias_pool_limit_value(
    value: Option<&PanelApiAliasPoolSizeValue>,
    auto_value: Option<u16>,
) -> Option<u16> {
    match value {
        Some(PanelApiAliasPoolSizeValue::Number(v)) => Some(*v),
        Some(PanelApiAliasPoolSizeValue::Auto(_)) => auto_value,
        None => None,
    }
}

pub(super) fn is_proxy_user_enabled(user: &ProxyUserCredentials) -> bool {
    if let Some(status) = user.status {
        if !matches!(status, ProxyUserStatus::Active | ProxyUserStatus::Trial) {
            return false;
        }
    }
    !is_input_expired(user.exp_date)
}

pub(super) fn find_input_target_names(app_state: &AppState, input_name: &Arc<str>) -> Vec<String> {
    let sources = app_state.app_config.sources.load();
    for source in &sources.sources {
        if source.inputs.iter().any(|name| name == input_name) {
            let mut names: SmallVec<[String; 8]> = SmallVec::new();
            for target in &source.targets {
                names.push(target.name.clone());
            }
            return names.into_vec();
        }
    }
    Vec::new()
}

pub(super) fn count_enabled_proxy_users(app_state: &AppState, input_name: &Arc<str>) -> usize {
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
        .filter(|target_user| target_names.iter().any(|target| target.eq_ignore_ascii_case(&target_user.target)))
        .map(|target_user| target_user.credentials.iter().filter(|cred| is_proxy_user_enabled(cred)).count())
        .sum()
}

pub(super) fn resolve_alias_pool_auto_value(app_state: &AppState, input_name: &Arc<str>) -> u16 {
    let enabled_users = count_enabled_proxy_users(app_state, input_name);
    u16::try_from(enabled_users).unwrap_or(u16::MAX)
}

pub(crate) fn target_has_alias_pool_min(app_state: &AppState, target_name: &str) -> bool {
    let sources = app_state.app_config.sources.load();
    for source in &sources.sources {
        let target_match = source.targets.iter().any(|target| target.name.eq_ignore_ascii_case(target_name));
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

pub(super) fn resolve_alias_pool_limits(
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
            return Err(TuliproxError::ConfigPanelApi(
                "panel_api.alias_pool.size.min must be <= panel_api.alias_pool.size.max".to_string(),
            ));
        }
    }
    Ok((min, max))
}

pub(super) fn resolve_alias_pool_min(app_state: &AppState, input_name: &Arc<str>, cfg: &PanelApiConfig) -> Option<u16> {
    let (min_val, _) = alias_pool_limit_values(cfg);
    let min_val = min_val?;
    let auto_value = min_val.is_auto().then(|| resolve_alias_pool_auto_value(app_state, input_name));
    resolve_alias_pool_limit_value(Some(min_val), auto_value)
}

pub(super) fn alias_pool_remove_expired(cfg: &PanelApiConfig) -> bool {
    cfg.alias_pool.as_ref().is_some_and(|p| p.remove_expired)
}

pub(super) fn collect_accounts(input: &ConfigInput) -> Vec<AccountCredentials> {
    let mut out = Vec::new();
    if let Some((u, p)) = extract_account_creds_from_input(input) {
        out.push(AccountCredentials { name: input.name.clone(), username: u, password: p, exp_date: input.exp_date });
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

pub(super) fn compare_alias_exp_date_config(a: &ConfigInputAlias, b: &ConfigInputAlias) -> Ordering {
    compare_named_exp_date(a.exp_date, a.name.as_ref(), b.exp_date, b.name.as_ref())
}

pub(super) fn compare_account_exp_date(a: &AccountCredentials, b: &AccountCredentials) -> Ordering {
    compare_named_exp_date(a.exp_date, a.name.as_ref(), b.exp_date, b.name.as_ref())
}

pub(super) fn compare_named_exp_date(
    a_exp_date: Option<i64>,
    a_name: &str,
    b_exp_date: Option<i64>,
    b_name: &str,
) -> Ordering {
    let a_ts = a_exp_date.unwrap_or(i64::MIN);
    let b_ts = b_exp_date.unwrap_or(i64::MIN);
    b_ts.cmp(&a_ts).then_with(|| a_name.cmp(b_name))
}

pub(super) fn sort_account_aliases_keep_root_first(accounts: &mut Vec<AccountCredentials>, root_name: &str) {
    let root = accounts.iter().find(|acct| acct.name.as_ref() == root_name).cloned();
    let mut aliases: Vec<AccountCredentials> =
        accounts.iter().filter(|acct| acct.name.as_ref() != root_name).cloned().collect();
    aliases.sort_by(compare_account_exp_date);
    accounts.clear();
    if let Some(root) = root {
        accounts.push(root);
    }
    accounts.extend(aliases);
}

pub(super) fn is_account_valid(exp_date: Option<i64>) -> bool { exp_date.is_some() && !is_input_expired(exp_date) }

pub(super) fn count_valid_accounts(accounts: &[AccountCredentials]) -> usize {
    accounts.iter().filter(|acct| is_account_valid(acct.exp_date)).count()
}

pub(super) fn root_counts_towards_pool(accounts: &[AccountCredentials], input_name: &Arc<str>) -> bool {
    accounts.iter().find(|acct| &acct.name == input_name).is_some_and(|acct| is_account_valid(acct.exp_date))
}

pub(super) fn count_valid_accounts_at(accounts: &[AccountCredentials], now: u64) -> usize {
    accounts.iter().filter(|acct| acct.exp_date.is_some() && !is_input_expired_at(acct.exp_date, now)).count()
}

pub(super) fn count_valid_alias_accounts_at(accounts: &[AccountCredentials], input_name: &Arc<str>, now: u64) -> usize {
    accounts
        .iter()
        .filter(|acct| &acct.name != input_name && acct.exp_date.is_some() && !is_input_expired_at(acct.exp_date, now))
        .count()
}

pub(super) fn root_counts_towards_pool_at(accounts: &[AccountCredentials], input_name: &Arc<str>, now: u64) -> bool {
    accounts
        .iter()
        .find(|acct| &acct.name == input_name)
        .is_some_and(|acct| acct.exp_date.is_some() && !is_input_expired_at(acct.exp_date, now))
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

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) async fn ensure_alias_pool_min(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    accounts: &mut Vec<AccountCredentials>,
    min_pool: u16,
    csv_path: Option<&Path>,
    sources_yml_patches: &mut Vec<SourcesYmlPatch>,
    time_ctx: Option<&PanelApiTimeContext>,
    effective_now: u64,
    optional: PanelApiOptionalFlagsSet,
) -> (bool, u16) {
    if min_pool == 0 {
        return (false, 0);
    }

    let renew_enabled = optional.contains(PanelApiOptionalFlags::ClientRenew);
    let new_enabled = optional.contains(PanelApiOptionalFlags::ClientNew);
    let adult_enabled = optional.contains(PanelApiOptionalFlags::AdultContent);
    if !renew_enabled && !new_enabled {
        return (false, 0);
    }

    let mut changed = false;
    let mut provisioned = 0_u16;
    let max_pool = resolve_alias_pool_limits(app_state.as_ref(), &input.name, panel_cfg).ok().and_then(|(_, max)| max);
    let mut existing_names: HashSet<Arc<str>> = accounts.iter().map(|a| a.name.clone()).collect();
    let max_attempts = usize::from(min_pool).saturating_add(10);
    for _ in 0..max_attempts {
        let current_valid = count_valid_alias_accounts_at(accounts, &input.name, effective_now);
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
            .filter(|(_, acct)| acct.name != input.name && is_input_expired_at(acct.exp_date, effective_now))
            .min_by_key(|(_, acct)| acct.exp_date.unwrap_or(i64::MAX))
            .map(|(idx, _)| idx);

        if let Some(idx) = expired_index {
            let acct = accounts.get(idx).cloned();
            let Some(acct) = acct else {
                break;
            };
            if renew_enabled {
                match panel_client_renew(app_state.as_ref(), panel_cfg, acct.username.as_str(), acct.password.as_str())
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
                                    sanitize_sensitive_info(&err.to_string())
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
                                let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                                if let Err(err) = csv_patch_batch_update_exp_date(
                                    app_state,
                                    &csv_lock,
                                    input.input_type,
                                    csv_path,
                                    &acct.name,
                                    acct.username.as_str(),
                                    acct.password.as_str(),
                                    new_exp,
                                )
                                .await
                                {
                                    debug_if_enabled!("panel_api failed to persist renew exp_date to csv: {}", err);
                                } else {
                                    changed = true;
                                }
                            } else {
                                sources_yml_patches.push(SourcesYmlPatch::UpdatePanelAccountExpiry {
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
                            sanitize_sensitive_info(&err.to_string())
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
                let base_url = resolve_provisioned_account_base_url(
                    input.url.as_str(),
                    base_url_from_resp.as_deref(),
                    &username,
                    &password,
                );

                let alias_name = derive_unique_alias_name_set(&existing_names, &input.name, &username);
                existing_names.insert(alias_name.clone().into());

                if adult_enabled {
                    if let Err(err) =
                        panel_client_adult_content(app_state.as_ref(), panel_cfg, Some((&username, &password))).await
                    {
                        debug_if_enabled!(
                            "panel_api client_adult_content failed for {}: {}",
                            sanitize_sensitive_info(&alias_name),
                            sanitize_sensitive_info(&err.to_string())
                        );
                    }
                }

                let exp_date = panel_client_info(app_state.as_ref(), panel_cfg, &username, &password, time_ctx)
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
                    let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                    if let Err(err) = csv_patch_batch_append(
                        app_state,
                        &csv_lock,
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
                debug_if_enabled!("panel_api client_new failed: {}", sanitize_sensitive_info(&err.to_string()));
                break;
            }
        }
    }

    (changed, provisioned)
}

pub(crate) async fn sync_panel_api_exp_dates(app_state: &Arc<AppState>) {
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
        // Account writes already synchronize prepared sources; refresh allocation without restarting services.
        refresh_internal_sources(app_state);
    }
}

pub(crate) async fn sync_panel_api_exp_dates_on_boot(app_state: &Arc<AppState>) {
    sync_panel_api_exp_dates(app_state).await;
}

pub(crate) async fn sync_panel_api_alias_pool_for_target(app_state: &Arc<AppState>, target_name: &str) {
    let sources_file_path = app_state.app_config.paths.load().sources_file_path.clone();
    let sources_path = PathBuf::from(&sources_file_path);
    let mut any_change = false;

    let sources = app_state.app_config.sources.load();
    for source in &sources.sources {
        let target_match = source.targets.iter().any(|target| target.name.eq_ignore_ascii_case(target_name));
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
                    sanitize_sensitive_info(&err.to_string())
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
        refresh_internal_sources(app_state);
    }
}
