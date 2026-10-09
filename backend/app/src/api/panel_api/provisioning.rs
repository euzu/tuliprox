use super::{
    build_panel_api_probe_targets, collect_accounts, compare_alias_exp_date_config, format_probe_target_actions,
    is_alias_pool_max_reached, is_expiring_with_offset_at, panel_client_adult_content, panel_client_info,
    panel_client_new, panel_client_renew, provisioning_method_to_reqwest, refresh_internal_sources,
    require_batch_alias_path, run_panel_api_provisioning_probe, should_reload_sources_after_internal_write,
    validate_panel_api_config, PanelApiOptionalFlags, PanelApiOptionalFlagsSet, PanelApiProbeTarget,
    PanelApiProvisionOutcome,
};
use crate::{
    api::{
        internal_csv::{
            csv_patch_batch_append, csv_patch_batch_sort_by_exp_date, csv_patch_batch_update_credentials,
            csv_patch_batch_update_exp_date,
        },
        model::{
            create_panel_api_provisioning_stream_with_stop, create_provider_connections_exhausted_stream, AppState,
            StreamDetails,
        },
        source_yml_patch::{derive_unique_alias_name, resolve_provisioned_account_base_url, SourcesYmlPatch},
    },
    model::{
        is_input_expired, is_input_expired_at, ConfigInput, ConfigInputAlias, GracePeriodOptions, InputSource,
        PanelApiConfig,
    },
    repository::AliasExpDateSortOrder,
    utils::{debug_if_enabled, format_http_status, request},
};
use axum::http::StatusCode;
use jsonwebtoken::get_current_timestamp;
use log::{error, warn};
use shared::{
    error::{string_to_io_error, TuliproxError},
    model::{InputType, PanelApiProvisioningMethod, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use url::Url;

pub(super) fn is_date_only_yyyy_mm_dd(value: &str) -> bool {
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

pub(super) fn resolve_panel_api_optional_flags(cfg: &PanelApiConfig, input_name: &str) -> PanelApiOptionalFlagsSet {
    let mut flags = PanelApiOptionalFlagsSet::new();
    if !cfg.query_parameter.account_info.is_empty() {
        flags.set(PanelApiOptionalFlags::AccountInfo);
    }
    if !cfg.query_parameter.client_new.is_empty() {
        flags.set(PanelApiOptionalFlags::ClientNew);
    }
    if !cfg.query_parameter.client_renew.is_empty() {
        flags.set(PanelApiOptionalFlags::ClientRenew);
    }
    if !cfg.query_parameter.client_adult_content.is_empty() {
        flags.set(PanelApiOptionalFlags::AdultContent);
    }

    let name = sanitize_sensitive_info(input_name);
    if !flags.contains(PanelApiOptionalFlags::ClientRenew) {
        debug_if_enabled!("panel_api request for client_renew disabled due to missing arguments for {}", name);
    }
    if !flags.contains(PanelApiOptionalFlags::ClientNew) {
        debug_if_enabled!("panel_api request for client_new disabled due to missing arguments for {}", name);
    }
    if !flags.contains(PanelApiOptionalFlags::AdultContent) {
        debug_if_enabled!("panel_api request for client_adult_content disabled due to missing arguments for {}", name);
    }
    if !flags.contains(PanelApiOptionalFlags::AccountInfo) {
        debug_if_enabled!("panel_api request for account_info disabled due to missing arguments for {}", name);
    }
    flags
}

pub(super) fn parse_panel_api_provisioning_offset_secs(offset: &str) -> Result<u64, TuliproxError> {
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
            return Err(TuliproxError::ConfigPanelApi(format!(
                "panel_api.provisioning.offset must be a number with optional suffix s/m/h/d (e.g. 30m, 12h), got '{raw}'"
            )));
        }
    };
    let num_part = num_part.trim();
    if num_part.is_empty() {
        return Err(TuliproxError::ConfigPanelApi(format!(
            "panel_api.provisioning.offset must be a number with optional suffix s/m/h/d (e.g. 30m, 12h), got '{raw}'"
        )));
    }
    let value: u64 = num_part.parse().map_err(|_| {
        TuliproxError::ConfigPanelApi(format!("panel_api.provisioning.offset is not a valid number: '{raw}'"))
    })?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| TuliproxError::ConfigPanelApi(format!("panel_api.provisioning.offset is too large: '{raw}'")))
}

pub(super) fn aliases_need_sort_config(aliases: &[ConfigInputAlias]) -> bool {
    if aliases.len() < 2 {
        return false;
    }
    aliases.windows(2).any(|pair| compare_alias_exp_date_config(&pair[0], &pair[1]) == Ordering::Greater)
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
        debug_if_enabled!("panel_api config invalid: {}", sanitize_sensitive_info(&err.to_string()));
        return false;
    }
    !is_alias_pool_max_reached(app_state, input)
}

pub(crate) fn find_input_by_provider_name(app_state: &AppState, provider_name: &str) -> Option<Arc<ConfigInput>> {
    let sources = app_state.app_config.sources.load();
    for input in &sources.inputs {
        if &*input.name == provider_name {
            return Some(Arc::clone(input));
        }
        if input.aliases.as_ref().is_some_and(|aliases| aliases.iter().any(|alias| &*alias.name == provider_name)) {
            return Some(Arc::clone(input));
        }
    }
    None
}

impl PanelApiProvisionOutcome {
    pub(crate) fn kind_label(&self) -> &'static str {
        match self {
            Self::Renewed => "client_renew",
            Self::Created => "client_new",
        }
    }
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_renew_expired_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    is_batch: bool,
    sources_path: &Path,
    treat_missing_exp_date_as_expired: bool,
    include_root: bool,
    optional: PanelApiOptionalFlagsSet,
) -> Option<PanelApiProvisionOutcome> {
    if !optional.contains(PanelApiOptionalFlags::ClientRenew) {
        return None;
    }
    let adult_enabled = optional.contains(PanelApiOptionalFlags::AdultContent);
    let mut candidates = collect_accounts(input);
    if !include_root {
        candidates.retain(|acct| acct.name != input.name);
    }
    for acct in &mut candidates {
        if treat_missing_exp_date_as_expired && acct.exp_date.is_none() {
            acct.exp_date =
                panel_client_info(app_state, panel_cfg, acct.username.as_str(), acct.password.as_str(), None)
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
        match panel_client_renew(app_state, panel_cfg, acct.username.as_str(), acct.password.as_str()).await {
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
                            sanitize_sensitive_info(&err.to_string())
                        );
                    }
                }
                let refreshed_exp =
                    panel_client_info(app_state, panel_cfg, acct.username.as_str(), acct.password.as_str(), None)
                        .await
                        .ok()
                        .flatten();

                if let Some(new_exp) = refreshed_exp.or(acct.exp_date) {
                    if is_batch {
                        match require_batch_alias_path(input) {
                            Ok(csv_path) => {
                                let csv_lock = app_state.app_config.file_locks.write_lock(&csv_path).await;
                                if let Err(err) = csv_patch_batch_update_exp_date(
                                    app_state,
                                    &csv_lock,
                                    input.input_type,
                                    &csv_path,
                                    &acct.name,
                                    &acct.username,
                                    &acct.password,
                                    new_exp,
                                )
                                .await
                                {
                                    debug_if_enabled!("panel_api failed to persist renew exp_date to csv: {}", err);
                                }
                                if let Err(err) = csv_patch_batch_sort_by_exp_date(
                                    app_state,
                                    &csv_lock,
                                    input.input_type,
                                    &csv_path,
                                    AliasExpDateSortOrder::NewestFirst,
                                )
                                .await
                                {
                                    debug_if_enabled!("panel_api failed to sort csv accounts after renew: {}", err);
                                }
                            }
                            Err(err) => debug_if_enabled!("panel_api cannot resolve batch csv path: {}", err),
                        }
                    } else {
                        let patches = [
                            SourcesYmlPatch::UpdatePanelAccountExpiry {
                                input_name: input.name.clone(),
                                account_name: Arc::clone(&acct.name),
                                exp_date: new_exp,
                            },
                            SourcesYmlPatch::SortAliases {
                                input_name: input.name.clone(),
                                order: AliasExpDateSortOrder::NewestFirst,
                            },
                        ];
                        if let Err(err) = crate::api::source_yml_patch::execute_account_source_patches(
                            app_state,
                            sources_path,
                            &patches,
                        )
                        .await
                        {
                            debug_if_enabled!("panel_api failed to persist renew exp_date to source.yml: {}", err);
                        }
                    }
                }

                if should_reload_sources_after_internal_write(app_state.as_ref()) {
                    refresh_internal_sources(app_state);
                }
                return Some(PanelApiProvisionOutcome::Renewed);
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
    None
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn try_refresh_root_account_on_exhausted(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    is_batch: bool,
    sources_path: &Path,
    optional: PanelApiOptionalFlagsSet,
) -> Option<PanelApiProvisionOutcome> {
    let renew_enabled = optional.contains(PanelApiOptionalFlags::ClientRenew);
    let new_enabled = optional.contains(PanelApiOptionalFlags::ClientNew);
    if !renew_enabled && !new_enabled {
        return None;
    }

    let now = get_current_timestamp();
    let offset_secs = panel_cfg
        .provisioning
        .offset
        .as_deref()
        .and_then(|value| parse_panel_api_provisioning_offset_secs(value).ok())
        .unwrap_or(0);
    let root_exp_missing = input.exp_date.is_none();
    let root_expired = is_input_expired_at(input.exp_date, now);
    let root_expiring = is_expiring_with_offset_at(input.exp_date, offset_secs, now);
    if !root_exp_missing && !root_expired && !root_expiring {
        return None;
    }

    let old_username = input.username.clone().unwrap_or_default();
    let old_password = input.password.clone().unwrap_or_default();
    let adult_enabled = optional.contains(PanelApiOptionalFlags::AdultContent);

    let (outcome, active_username, active_password, credentials_changed) =
        if renew_enabled && !old_username.is_empty() && !old_password.is_empty() {
            match panel_client_renew(app_state, panel_cfg, old_username.as_str(), old_password.as_str()).await {
                Ok(()) => (PanelApiProvisionOutcome::Renewed, old_username.clone(), old_password.clone(), false),
                Err(err) => {
                    debug_if_enabled!(
                        "panel_api client_renew failed for root {}: {}",
                        sanitize_sensitive_info(&input.name),
                        sanitize_sensitive_info(&err.to_string())
                    );
                    if !new_enabled {
                        return None;
                    }
                    match panel_client_new(app_state, panel_cfg).await {
                        Ok((username, password, _base_url_from_resp)) => {
                            (PanelApiProvisionOutcome::Created, username, password, true)
                        }
                        Err(err) => {
                            debug_if_enabled!(
                                "panel_api client_new failed for root {}: {}",
                                sanitize_sensitive_info(&input.name),
                                sanitize_sensitive_info(&err.to_string())
                            );
                            return None;
                        }
                    }
                }
            }
        } else if new_enabled {
            match panel_client_new(app_state, panel_cfg).await {
                Ok((username, password, _base_url_from_resp)) => {
                    (PanelApiProvisionOutcome::Created, username, password, true)
                }
                Err(err) => {
                    debug_if_enabled!(
                        "panel_api client_new failed for root {}: {}",
                        sanitize_sensitive_info(&input.name),
                        sanitize_sensitive_info(&err.to_string())
                    );
                    return None;
                }
            }
        } else {
            return None;
        };

    if adult_enabled {
        if let Err(err) =
            panel_client_adult_content(app_state, panel_cfg, Some((active_username.as_str(), active_password.as_str())))
                .await
        {
            debug_if_enabled!(
                "panel_api client_adult_content failed for root {}: {}",
                sanitize_sensitive_info(&input.name),
                sanitize_sensitive_info(&err.to_string())
            );
        }
    }

    if !wait_for_panel_api_account_ready(
        app_state,
        input,
        panel_cfg,
        input.name.as_ref(),
        active_username.as_str(),
        active_password.as_str(),
    )
    .await
    {
        debug_if_enabled!(
            "panel_api root account not ready after probe/cooldown for {}",
            sanitize_sensitive_info(&input.name)
        );
        return None;
    }

    let refreshed_exp_date =
        panel_client_info(app_state, panel_cfg, active_username.as_str(), active_password.as_str(), None)
            .await
            .ok()
            .flatten();
    let exp_date = if credentials_changed { refreshed_exp_date } else { refreshed_exp_date.or(input.exp_date) };

    if is_batch {
        let Ok(csv_path) = require_batch_alias_path(input) else {
            return None;
        };
        let csv_lock = app_state.app_config.file_locks.write_lock(&csv_path).await;
        let result = if credentials_changed {
            csv_patch_batch_update_credentials(
                app_state,
                &csv_lock,
                input.input_type,
                &csv_path,
                &input.name,
                old_username.as_str(),
                old_password.as_str(),
                active_username.as_str(),
                active_password.as_str(),
                exp_date,
            )
            .await
        } else if let Some(exp_date) = exp_date {
            csv_patch_batch_update_exp_date(
                app_state,
                &csv_lock,
                input.input_type,
                &csv_path,
                &input.name,
                active_username.as_str(),
                active_password.as_str(),
                exp_date,
            )
            .await
        } else {
            Ok(())
        };
        if let Err(err) = result {
            debug_if_enabled!("panel_api failed to persist root provisioning to csv: {}", err);
            return None;
        }
        if let Err(err) = csv_patch_batch_sort_by_exp_date(
            app_state,
            &csv_lock,
            input.input_type,
            &csv_path,
            AliasExpDateSortOrder::NewestFirst,
        )
        .await
        {
            debug_if_enabled!("panel_api failed to sort csv accounts after root provisioning: {}", err);
            return None;
        }
    } else {
        let patch = SourcesYmlPatch::PersistProvisionedAccount {
            input_name: input.name.clone(),
            username: active_username,
            password: active_password,
            exp_date,
        };
        if let Err(err) =
            crate::api::source_yml_patch::execute_account_source_patches(app_state, sources_path, &[patch]).await
        {
            debug_if_enabled!("panel_api failed to persist root provisioning to source.yml: {}", err);
            return None;
        }
    }

    if should_reload_sources_after_internal_write(app_state.as_ref()) {
        refresh_internal_sources(app_state);
    }

    Some(outcome)
}

#[allow(clippy::too_many_lines)]
pub(super) async fn try_create_new_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    panel_cfg: &PanelApiConfig,
    is_batch: bool,
    sources_path: &Path,
    optional: PanelApiOptionalFlagsSet,
) -> Option<PanelApiProvisionOutcome> {
    if !optional.contains(PanelApiOptionalFlags::ClientNew) {
        return None;
    }
    let adult_enabled = optional.contains(PanelApiOptionalFlags::AdultContent);
    match panel_client_new(app_state, panel_cfg).await {
        Ok((username, password, base_url_from_resp)) => {
            let base_url = resolve_provisioned_account_base_url(
                input.url.as_str(),
                base_url_from_resp.as_deref(),
                &username,
                &password,
            );

            let mut existing_names: Vec<Arc<str>> = vec![input.name.clone()];
            if let Some(aliases) = input.aliases.as_ref() {
                existing_names.extend(aliases.iter().map(|a| a.name.clone()));
            }
            let alias_name = derive_unique_alias_name(&existing_names, &input.name, &username);

            if adult_enabled {
                if let Err(err) = panel_client_adult_content(app_state, panel_cfg, Some((&username, &password))).await {
                    debug_if_enabled!(
                        "panel_api client_adult_content failed for {}: {}",
                        sanitize_sensitive_info(&alias_name),
                        sanitize_sensitive_info(&err.to_string())
                    );
                }
            }

            let exp_date = panel_client_info(app_state, panel_cfg, &username, &password, None).await.ok().flatten();

            if !wait_for_panel_api_account_ready(
                app_state,
                input,
                panel_cfg,
                alias_name.as_ref(),
                username.as_str(),
                password.as_str(),
            )
            .await
            {
                debug_if_enabled!(
                    "panel_api client_new account not ready after probe/cooldown for {}",
                    sanitize_sensitive_info(alias_name.as_ref())
                );
                return None;
            }

            if is_batch {
                match require_batch_alias_path(input) {
                    Ok(csv_path) => {
                        let batch_type = if input.input_type == InputType::Xtream {
                            InputType::XtreamBatch
                        } else {
                            InputType::M3uBatch
                        };
                        let csv_lock = app_state.app_config.file_locks.write_lock(&csv_path).await;
                        if let Err(err) = csv_patch_batch_append(
                            app_state,
                            &csv_lock,
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
                        if let Err(err) = csv_patch_batch_sort_by_exp_date(
                            app_state,
                            &csv_lock,
                            batch_type,
                            &csv_path,
                            AliasExpDateSortOrder::NewestFirst,
                        )
                        .await
                        {
                            warn!("panel_api failed to sort csv accounts after append: {err}");
                            return None;
                        }
                    }
                    Err(err) => {
                        warn!(
                            "panel_api cannot resolve batch csv path for {}: {}",
                            sanitize_sensitive_info(input.name.as_ref()),
                            err,
                        );
                        return None;
                    }
                }
            } else {
                let patches = [
                    SourcesYmlPatch::AddAlias {
                        input_name: input.name.clone(),
                        alias_name: Arc::clone(&alias_name),
                        base_url,
                        username,
                        password,
                        exp_date,
                    },
                    SourcesYmlPatch::SortAliases {
                        input_name: input.name.clone(),
                        order: AliasExpDateSortOrder::NewestFirst,
                    },
                ];
                if let Err(err) =
                    crate::api::source_yml_patch::execute_account_source_patches(app_state, sources_path, &patches)
                        .await
                {
                    warn!("panel_api failed to persist new alias to source.yml: {err}");
                    return None;
                }
            }

            if should_reload_sources_after_internal_write(app_state.as_ref()) {
                refresh_internal_sources(app_state);
            }
            Some(PanelApiProvisionOutcome::Created)
        }
        Err(err) => {
            debug_if_enabled!("panel_api client_new failed: {}", sanitize_sensitive_info(&err.to_string()));
            None
        }
    }
}

pub async fn try_provision_account_on_exhausted(
    app_state: &Arc<AppState>,
    input_name: &Arc<str>,
) -> Option<PanelApiProvisionOutcome> {
    let _input_lock = app_state.app_config.file_locks.write_lock_str(format!("panel_api:{input_name}").as_str()).await;
    let current_input = find_input_by_provider_name(app_state.as_ref(), input_name.as_ref());
    let Some(input) = current_input.as_deref() else {
        debug_if_enabled!(
            "panel_api: skipped (input no longer exists) for input {}",
            sanitize_sensitive_info(input_name.as_ref())
        );
        return None;
    };

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

    if let Err(err) = validate_panel_api_config(panel_cfg) {
        debug_if_enabled!("panel_api config invalid: {}", sanitize_sensitive_info(&err.to_string()));
        return None;
    }
    let optional = resolve_panel_api_optional_flags(panel_cfg, &input.name);
    debug_if_enabled!(
        "panel_api: exhausted -> provisioning for input {} (aliases={})",
        sanitize_sensitive_info(&input.name),
        input.aliases.as_ref().map_or(0, Vec::len)
    );

    let is_batch = input.t_batch_url.as_ref().is_some_and(|u| !u.trim().is_empty());
    let sources_file_path = app_state.app_config.paths.load().sources_file_path.clone();
    let sources_path = PathBuf::from(&sources_file_path);

    if let Some(outcome) =
        try_refresh_root_account_on_exhausted(app_state, input, panel_cfg, is_batch, sources_path.as_path(), optional)
            .await
    {
        debug_if_enabled!(
            "panel_api: provisioning succeeded via root {} for input {}",
            outcome.kind_label(),
            sanitize_sensitive_info(&input.name)
        );
        return Some(outcome);
    }

    if is_alias_pool_max_reached(app_state, input) {
        return None;
    }

    if let Some(outcome) =
        try_renew_expired_account(app_state, input, panel_cfg, is_batch, sources_path.as_path(), true, false, optional)
            .await
    {
        debug_if_enabled!(
            "panel_api: provisioning succeeded via client_renew for input {}",
            sanitize_sensitive_info(&input.name)
        );
        return Some(outcome);
    }
    let created = try_create_new_account(app_state, input, panel_cfg, is_batch, sources_path.as_path(), optional).await;
    debug_if_enabled!(
        "panel_api: provisioning via client_new for input {} => {}",
        sanitize_sensitive_info(&input.name),
        if created.is_some() { "success" } else { "failed" }
    );
    created
}

pub(super) async fn probe_panel_api_targets(
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
            PanelApiProbeTarget::PlayerApi { action, input_source } => {
                match probe_panel_api_test_url(app_state, input_source, probe_method).await {
                    Ok(status) => {
                        debug_if_enabled!(
                            "panel_api probe status: '{}' action={} url: {}",
                            format_http_status(status),
                            action,
                            sanitize_sensitive_info(input_source.url.as_str())
                        );
                        if status.is_success() {
                            done.insert(action);
                        }
                    }
                    Err(err) => {
                        if err.kind() == io::ErrorKind::TimedOut {
                            debug_if_enabled!(
                                "panel_api probe timeout action={} url: {}",
                                action,
                                sanitize_sensitive_info(input_source.url.as_str())
                            );
                        } else {
                            debug_if_enabled!(
                                "panel_api probe failed action={} url: {}: {err}",
                                action,
                                sanitize_sensitive_info(input_source.url.as_str())
                            );
                        }
                    }
                }
            }
        }
    }
    done.len() == targets.len()
}

pub(super) async fn probe_panel_api_test_url(
    app_state: &Arc<AppState>,
    input_source: &InputSource,
    method: PanelApiProvisioningMethod,
) -> Result<StatusCode, io::Error> {
    let client = app_state.http_clients.default.load();
    let request_method = provisioning_method_to_reqwest(method);
    let test_url = Url::parse(input_source.url.as_str()).map_err(|err| {
        string_to_io_error(format!(
            "Malformed URL {}: {}",
            sanitize_sensitive_info(input_source.url.as_str()),
            sanitize_sensitive_info(err.to_string().as_str())
        ))
    })?;

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
    let response = request::send_with_retry_and_provider(
        &app_state.app_config,
        &test_url,
        input_source.get_provider(),
        false,
        |resolved_url| client.request(request_method.clone(), resolved_url.clone()).headers(headers.clone()),
    )
    .await?;
    Ok(response.status())
}

pub(super) async fn apply_provisioning_cooldown(panel_cfg: &PanelApiConfig, account_name: &str, input_name: &Arc<str>) {
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

pub(crate) async fn wait_for_panel_api_account_ready(
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

    let targets_list = format_probe_target_actions(&probe_targets);
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
        if probe_panel_api_targets(app_state, probe_method, &probe_targets, &mut done_targets).await {
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
        let sleep_for = if remaining < probe_delay { remaining } else { probe_delay };
        tokio::time::sleep(sleep_for).await;
    }
}

pub fn create_panel_api_provisioning_stream_details(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    provider_name: Option<Arc<str>>,
    grace_period_options: &GracePeriodOptions,
    addr: SocketAddr,
    virtual_id: VirtualId,
) -> StreamDetails {
    let stop_signal = CancellationToken::new();
    let headers = [("connection".to_string(), "close".to_string())];
    let (stream, stream_info) =
        create_panel_api_provisioning_stream_with_stop(&app_state.app_config, &headers, stop_signal.clone());

    if stream.is_none() {
        debug_if_enabled!(
            "panel_api provisioning stream missing; falling back to provider exhausted for input {}",
            sanitize_sensitive_info(&input.name)
        );
        let (stream, stream_info) = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
        return StreamDetails {
            shared_subscriber_id: None,
            stream,
            stream_info,
            provider_name,
            request_url: None,
            session_headers: None,
            provider_session_headers: tuliprox_session::ProviderSessionHeaders::default(),
            user_agent_stream_index: None,
            grace_period: *grace_period_options,
            provider_grace_active: false,
            disable_provider_grace: true,
            reconnect_flag: None,
            provider_handle: None,
            content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
            grace_resolution_context: None,
            custom_reason: None,
            response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
            session_registration: None,
        };
    }

    let app_state_clone = Arc::clone(app_state);
    let input_name = Arc::clone(&input.name);
    let stop_clone = stop_signal.clone();
    tokio::spawn(async move {
        if let Err(err) =
            run_panel_api_provisioning_probe(app_state_clone, input_name, stop_clone, addr, virtual_id).await
        {
            error!("Error running Probe: {err:?}");
        }
    });

    StreamDetails {
        shared_subscriber_id: None,
        stream,
        stream_info,
        provider_name,
        request_url: None,
        session_headers: None,
        provider_session_headers: tuliprox_session::ProviderSessionHeaders::default(),
        user_agent_stream_index: None,
        grace_period: *grace_period_options,
        provider_grace_active: false,
        disable_provider_grace: true,
        reconnect_flag: None,
        provider_handle: None,
        content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
        grace_resolution_context: None,
        custom_reason: None,
        response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
        session_registration: None,
    }
}
