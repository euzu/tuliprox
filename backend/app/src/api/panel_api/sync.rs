use super::{
    alias_pool_has_min, alias_pool_remove_expired, aliases_need_sort_config, apply_clock_skew, collect_accounts,
    count_enabled_proxy_users, count_valid_accounts_at, ensure_alias_pool_min, extract_account_creds_from_input,
    fetch_root_user_api_info, is_expiring_with_offset_at, load_panel_api_time_cache, panel_account_info,
    panel_api_time_cache_path, panel_client_adult_content, panel_client_info, panel_client_info_raw, panel_client_new,
    panel_client_renew, parse_cached_tz, parse_panel_api_provisioning_offset_secs, persist_panel_api_time_cache,
    resolve_alias_pool_min, resolve_panel_api_optional_flags, resolve_panel_expire_mode, root_counts_towards_pool,
    root_counts_towards_pool_at, sort_account_aliases_keep_root_first, validate_panel_api_config,
    wait_for_panel_api_account_ready, AccountCredentials, PanelApiOptionalFlags, PanelApiTimeCacheEntry,
    PanelApiTimeContext,
};
use crate::{
    api::{
        internal_csv::{
            csv_patch_batch_append, csv_patch_batch_remove_expired, csv_patch_batch_sort_by_exp_date,
            csv_patch_batch_update_credentials, csv_patch_batch_update_exp_date,
        },
        model::AppState,
        source_yml_patch::{derive_unique_alias_name_set, resolve_provisioned_account_base_url, SourcesYmlPatch},
    },
    model::{is_input_expired_at, ConfigInput},
    repository::{get_csv_file_path, AliasExpDateSortOrder},
    utils::debug_if_enabled,
};
use chrono_tz::Tz;
use jsonwebtoken::get_current_timestamp;
use log::warn;
use shared::{error::TuliproxError, model::InputType, utils::sanitize_sensitive_info};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) fn refresh_internal_sources(app_state: &Arc<AppState>) {
    app_state.active_provider.update_config(&app_state.app_config);
}

pub(super) fn should_reload_sources_after_internal_write(app_state: &AppState) -> bool {
    !app_state.app_config.config.load().config_hot_reload
}

pub(super) fn resolve_batch_alias_path(batch_url: Option<&str>) -> Result<Option<PathBuf>, TuliproxError> {
    let Some(batch_url) = batch_url.filter(|url| !url.trim().is_empty()) else {
        return Ok(None);
    };
    get_csv_file_path(batch_url).map(Some).map_err(|err| TuliproxError::ConfigInput(format!("{err}")))
}

#[allow(clippy::too_many_lines)]
pub(super) async fn sync_panel_api_for_input_on_boot(
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
            sanitize_sensitive_info(&err.to_string())
        );
        return false;
    }
    let optional = resolve_panel_api_optional_flags(panel_cfg, &input.name);

    let input_name = &input.name;
    let _input_lock = app_state.app_config.file_locks.write_lock_str(format!("panel_api:{input_name}").as_str()).await;

    let mut any_change = false;
    let csv_path = match resolve_batch_alias_path(input.t_batch_url.as_deref()) {
        Ok(path) => path,
        Err(err) => {
            warn!(
                "panel_api boot sync skipped alias mutations for batch input {}: {}",
                sanitize_sensitive_info(&input.name),
                sanitize_sensitive_info(&err.to_string())
            );
            return false;
        }
    };
    let mut sources_yml_patches: Vec<SourcesYmlPatch> = Vec::new();
    let mut pending_sources_yml = false;
    let mut source_yml_sort_aliases_requested = false;

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

    if panel_cfg.alias_pool.is_some()
        && csv_path.is_none()
        && input.aliases.as_ref().is_some_and(|aliases| aliases_need_sort_config(aliases))
    {
        sources_yml_patches.push(SourcesYmlPatch::SortAliases {
            input_name: input.name.clone(),
            order: AliasExpDateSortOrder::NewestFirst,
        });
        pending_sources_yml = true;
        source_yml_sort_aliases_requested = true;
    }

    if let Some((root_username, root_password)) = extract_account_creds_from_input(input.as_ref()) {
        let user_info = fetch_root_user_api_info(app_state, input.as_ref()).await;
        let (root_exp_date, server_tz, skew_secs) = if let Ok(Some(info)) = user_info {
            let local_now = i64::try_from(get_current_timestamp()).unwrap_or(0);
            let skew_secs = info.server_now_ts.unwrap_or(local_now) - local_now;
            (info.exp_date, info.server_tz, Some(skew_secs))
        } else {
            (None, None, None)
        };

        let panel_expire_raw =
            match panel_client_info_raw(app_state.as_ref(), panel_cfg, root_username.as_str(), root_password.as_str())
                .await
            {
                Ok(expire) => expire,
                Err(err) => {
                    debug_if_enabled!(
                        "panel_api root client_info (raw) failed for {}: {}",
                        sanitize_sensitive_info(&input.name),
                        sanitize_sensitive_info(&err.to_string())
                    );
                    None
                }
            };

        let mut expire_mode = resolve_panel_expire_mode(root_exp_date, panel_expire_raw.as_deref(), server_tz);
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

        time_ctx = Some(PanelApiTimeContext { expire_mode, server_tz });
        effective_now = apply_clock_skew(get_current_timestamp(), skew_secs.unwrap_or_default());

        time_cache.inputs.insert(
            input.name.to_string(),
            PanelApiTimeCacheEntry { expire_mode, server_tz: server_tz.map(|tz| tz.name().to_string()), skew_secs },
        );
        if let Err(err) = persist_panel_api_time_cache(app_state.as_ref(), &cache_path, &time_cache).await {
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
        time_ctx = Some(PanelApiTimeContext { expire_mode: cached.expire_mode, server_tz });
        effective_now = apply_clock_skew(get_current_timestamp(), cached.skew_secs.unwrap_or_default());
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
        let new_exp =
            match panel_client_info(app_state.as_ref(), panel_cfg, &acct.username, &acct.password, time_ctx.as_ref())
                .await
            {
                Ok(v) => v,
                Err(err) => {
                    debug_if_enabled!(
                        "panel_api client_info failed for {}: {}",
                        sanitize_sensitive_info(&acct.name),
                        sanitize_sensitive_info(&err.to_string())
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
            let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
            if let Err(err) = csv_patch_batch_update_exp_date(
                app_state,
                &csv_lock,
                input.input_type,
                csv_path,
                &acct.name,
                &acct.username,
                &acct.password,
                new_exp,
            )
            .await
            {
                debug_if_enabled!("panel_api boot sync failed to persist exp_date to csv: {}", err);
                continue;
            }
            any_change = true;
        } else {
            sources_yml_patches.push(SourcesYmlPatch::UpdatePanelAccountExpiry {
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
    let renew_enabled = optional.contains(PanelApiOptionalFlags::ClientRenew);
    let new_enabled = optional.contains(PanelApiOptionalFlags::ClientNew);
    let adult_enabled = optional.contains(PanelApiOptionalFlags::AdultContent);
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
            Some(ts) => u64::try_from(ts).map_or(true, |exp_ts| exp_ts <= now),
            None => false,
        };
        let root_expiring = match root_exp_date {
            Some(ts) => u64::try_from(ts).map_or(true, |exp_ts| exp_ts > now && exp_ts <= offset_deadline),
            None => false,
        };
        let should_refresh_root = root_exp_missing || root_expired || root_expiring;

        let root_exp_display = root_exp_date.map_or_else(|| "None".to_string(), |ts| ts.to_string());
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
                                sanitize_sensitive_info(&err.to_string())
                            );
                            if new_enabled {
                                match panel_client_new(app_state.as_ref(), panel_cfg).await {
                                    Ok((new_username, new_password, _base_url_from_resp)) => {
                                        provisioned_root = 1;
                                        // Variant B: if the old root is still valid but within offset window,
                                        // keep it as a new alias entry so we don't lose usable credentials.
                                        let park_old_root_as_alias =
                                            root_expiring && !root_expired && root_exp_date.is_some();

                                        if park_old_root_as_alias {
                                            let base_url = resolve_provisioned_account_base_url(
                                                input.url.as_str(),
                                                None,
                                                &old_username,
                                                &old_password,
                                            );
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
                                                let csv_lock =
                                                    app_state.app_config.file_locks.write_lock(csv_path).await;
                                                if let Err(err) = csv_patch_batch_append(
                                                    app_state,
                                                    &csv_lock,
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
                                            let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                                            if let Err(err) = csv_patch_batch_update_credentials(
                                                app_state,
                                                &csv_lock,
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
                                            sanitize_sensitive_info(&err.to_string())
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
                            let park_old_root_as_alias = root_expiring && !root_expired && root_exp_date.is_some();

                            if park_old_root_as_alias {
                                let base_url = resolve_provisioned_account_base_url(
                                    input.url.as_str(),
                                    None,
                                    &old_username,
                                    &old_password,
                                );
                                let alias_name =
                                    derive_unique_alias_name_set(&existing_names, &input.name, old_username.as_str());
                                existing_names.insert(alias_name.clone().into());

                                if let Some(csv_path) = csv_path.as_ref() {
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
                                let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                                if let Err(err) = csv_patch_batch_update_credentials(
                                    app_state,
                                    &csv_lock,
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
                                sanitize_sensitive_info(&err.to_string())
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
                            sanitize_sensitive_info(&err.to_string())
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
                        let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                        let result = if creds_changed {
                            csv_patch_batch_update_credentials(
                                app_state,
                                &csv_lock,
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
                                app_state,
                                &csv_lock,
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
                            sources_yml_patches.push(SourcesYmlPatch::UpdatePanelAccountExpiry {
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

    // Refresh aliases after the root operation to avoid over-provisioning.
    let now = effective_now;
    let offset_deadline = now.saturating_add(offset_secs);
    let root_valid = root_counts_towards_pool_at(&accounts, &input.name, now);
    let desired_aliases = min_pool.filter(|m| *m > 0).map_or_else(
        || u16::try_from(accounts.iter().filter(|a| a.name != input.name).count()).unwrap_or(u16::MAX),
        |min_pool| min_pool.saturating_sub(u16::from(root_valid)),
    );

    let expiring_aliases = accounts
        .iter()
        .filter(|a| a.name != input.name && is_expiring_with_offset_at(a.exp_date, offset_secs, now))
        .count();
    let expired_aliases =
        accounts.iter().filter(|a| a.name != input.name && is_input_expired_at(a.exp_date, now)).count();

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
                Some(ts) => u64::try_from(ts).is_ok_and(|exp_ts| exp_ts > offset_deadline),
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
                    Some(ts) => u64::try_from(ts).map_or(true, |exp_ts| exp_ts <= offset_deadline),
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

        let valid_aliases_beyond_offset_u16 = u16::try_from(valid_aliases_beyond_offset).unwrap_or(u16::MAX);
        let needed_refresh_aliases_u16 = desired_aliases_u16.saturating_sub(valid_aliases_beyond_offset_u16);
        let planned_refresh_aliases = refresh_candidates.len().min(usize::from(needed_refresh_aliases_u16));
        let refresh_plan: Vec<usize> = refresh_candidates.into_iter().take(planned_refresh_aliases).collect();
        (refresh_plan, planned_refresh_aliases)
    } else {
        (Vec::new(), 0)
    };

    let log_pool = alias_pool_has_min(panel_cfg);
    let enabled_users = if log_pool { count_enabled_proxy_users(app_state.as_ref(), &input.name) } else { 0 };
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
            match panel_client_renew(app_state.as_ref(), panel_cfg, old_username.as_str(), old_password.as_str()).await
            {
                Ok(()) => {
                    provisioned_aliases = provisioned_aliases.saturating_add(1);
                    (old_username.clone(), old_password.clone(), false)
                }
                Err(err) => {
                    debug_if_enabled!(
                        "panel_api client_renew failed for alias {}: {}",
                        sanitize_sensitive_info(&account_name),
                        sanitize_sensitive_info(&err.to_string())
                    );
                    if new_enabled {
                        match panel_client_new(app_state.as_ref(), panel_cfg).await {
                            Ok((new_username, new_password, base_url_from_resp)) => {
                                let base_url = resolve_provisioned_account_base_url(
                                    input.url.as_str(),
                                    base_url_from_resp.as_deref(),
                                    &new_username,
                                    &new_password,
                                );

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
                                            sanitize_sensitive_info(&err.to_string())
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
                                    let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                                    if let Err(err) = csv_patch_batch_append(
                                        app_state,
                                        &csv_lock,
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
                                    sanitize_sensitive_info(&err.to_string())
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
                    let base_url = resolve_provisioned_account_base_url(
                        input.url.as_str(),
                        base_url_from_resp.as_deref(),
                        &new_username,
                        &new_password,
                    );

                    let alias_name = derive_unique_alias_name_set(&existing_names, &input.name, &new_username);
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
                                sanitize_sensitive_info(&err.to_string())
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
                        let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                        if let Err(err) = csv_patch_batch_append(
                            app_state,
                            &csv_lock,
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
                        sanitize_sensitive_info(&err.to_string())
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
                    sanitize_sensitive_info(&err.to_string())
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
                let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
                let result = if creds_changed {
                    csv_patch_batch_update_credentials(
                        app_state,
                        &csv_lock,
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
                        app_state,
                        &csv_lock,
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
                    sources_yml_patches.push(SourcesYmlPatch::UpdatePanelAccountExpiry {
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
                    sanitize_sensitive_info(&err.to_string())
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
            let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
            match csv_patch_batch_remove_expired(app_state, &csv_lock, input.input_type, csv_path).await {
                Ok(true) => any_change = true,
                Ok(false) => {}
                Err(err) => debug_if_enabled!("panel_api boot sync failed to remove expired csv accounts: {}", err),
            }
        } else {
            sources_yml_patches.push(SourcesYmlPatch::RemoveExpiredAliases { input_name: input.name.clone() });
            pending_sources_yml = true;
        }
    }

    if optional.contains(PanelApiOptionalFlags::AccountInfo) {
        let creds = accounts.first().map(|acct| (acct.username.as_str(), acct.password.as_str()));
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
                    sanitize_sensitive_info(&err.to_string())
                );
            }
        }
    }

    if panel_cfg.alias_pool.is_some() {
        if let Some(csv_path) = csv_path.as_ref() {
            let csv_lock = app_state.app_config.file_locks.write_lock(csv_path).await;
            match csv_patch_batch_sort_by_exp_date(
                app_state,
                &csv_lock,
                input.input_type,
                csv_path,
                AliasExpDateSortOrder::NewestFirst,
            )
            .await
            {
                Ok(true) => any_change = true,
                Ok(false) => {}
                Err(err) => debug_if_enabled!(
                    "panel_api boot/update final sort failed for csv alias pool {}: {}",
                    sanitize_sensitive_info(&input.name),
                    err
                ),
            }
        } else if pending_sources_yml && !source_yml_sort_aliases_requested {
            sources_yml_patches.push(SourcesYmlPatch::SortAliases {
                input_name: input.name.clone(),
                order: AliasExpDateSortOrder::NewestFirst,
            });
        }
    }

    if pending_sources_yml {
        match crate::api::source_yml_patch::execute_account_source_patches(
            app_state,
            sources_path,
            &sources_yml_patches,
        )
        .await
        {
            Ok(true) => any_change = true,
            Ok(false) => {}
            Err(err) => debug_if_enabled!("panel_api boot sync failed to persist source.yml patches: {}", err),
        }
    }

    any_change
}
