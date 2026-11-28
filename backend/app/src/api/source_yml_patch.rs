//! Central orchestration for targeted `source.yml` patches.
//!
//! This module owns the full patch transaction: acquire lock → read → semantic
//! apply → text patch → verify → backup/replace → mark internal revision.
//! Background tasks (expiry worker, Panel API) construct `SourcesYmlPatch`
//! commands and call [`execute_source_yml_patches`] — they never read, mutate,
//! or serialize the file themselves.

use crate::{
    config_loader::{
        source_patch::{
            apply_scalar_edits, build_alias_addition_edit, build_alias_removal_edits, build_alias_sequence_edit,
            build_alias_sort_edit, build_field_insertion_edit, comment_starts_in, detect_newline, find_input,
            flow_mapping_start_at, is_flow_style_mapping_at, line_content_end, line_end_offset, line_indent_at,
            parse_and_validate_patched_text, parse_patch_document, serialize_yaml_scalar, sibling_key_indent,
            span_byte_range, FlowScopeScanner, TextEdit,
        },
        write_config_text_file,
    },
    model::{is_input_expired, is_input_expired_at},
    repository::AliasExpDateSortOrder,
};
use log::warn;
use shared::{
    error::TuliproxError,
    model::{
        ConfigInputAliasDto, ConfigInputDto, ConfigSortDto, ConfigSortRuleDto, ItemField, Prepare, SortOrder,
        SortTarget, SourcesConfigDto, TargetGroupCluster,
    },
    utils::Internable,
};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    ops::Range,
    path::Path,
    sync::Arc,
};
use tuliprox_core::model::{AppConfig, ConfigSort};
use url::Url;

// ---------------------------------------------------------------------------
// Command enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum SourcesYmlPatch {
    SetObservedAccount {
        input_name: Arc<str>,
        observation: crate::model::ProviderAccountObservation,
    },
    SetAccountDisabled {
        input_name: Arc<str>,
        account_name: Arc<str>,
    },
    SetFetchedExpiry {
        input_name: Arc<str>,
        account_name: Arc<str>,
        exp_date: i64,
        disable: bool,
    },
    UpdatePanelAccountExpiry {
        input_name: Arc<str>,
        account_name: Arc<str>,
        exp_date: i64,
    },
    UpdatePanelApiCredits {
        input_name: Arc<str>,
        credits: String,
    },
    SortAliases {
        input_name: Arc<str>,
        order: AliasExpDateSortOrder,
    },
    UpdateRootCredentials {
        input_name: Arc<str>,
        username: String,
        password: String,
        exp_date: Option<i64>,
    },
    PersistProvisionedAccount {
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
    /// Stores the Explorer group order of one cluster of one target as a managed `sort.rules`
    /// group rule. An empty `groups` list removes the managed rule again.
    SetTargetGroupOrder {
        target_name: Arc<str>,
        cluster: TargetGroupCluster,
        groups: Vec<String>,
    },
}

impl SourcesYmlPatch {
    /// Structural edits change subsequent YAML byte spans and therefore form their own planning
    /// step. Consecutive scalar-only commands can be planned and validated together without
    /// reparsing the whole document.
    const fn changes_yaml_structure(&self) -> bool {
        matches!(
            self,
            Self::SortAliases { .. }
                | Self::PersistProvisionedAccount { .. }
                | Self::AddAlias { .. }
                | Self::RemoveExpiredAliases { .. }
                | Self::SetTargetGroupOrder { .. }
        )
    }

    /// Group-order commands rebuild the target's runtime sort from the patched document instead of
    /// mutating the runtime rules directly.
    const fn is_target_group_order(&self) -> bool { matches!(self, Self::SetTargetGroupOrder { .. }) }
}

#[derive(Debug)]
pub(crate) enum TargetGroupOrderPatchError {
    TargetNotFound(String),
    UnsupportedLayout(String),
    RevisionChanged,
    ValidationFailed(String),
    Other(TuliproxError),
}

impl TargetGroupOrderPatchError {
    fn into_tuliprox_error(self) -> TuliproxError {
        match self {
            Self::TargetNotFound(message) | Self::UnsupportedLayout(message) | Self::ValidationFailed(message) => {
                TuliproxError::ConfigPanelApi(message)
            }
            Self::RevisionChanged => TuliproxError::Config(
                "source.yml changed while an internal patch was being prepared; retrying later".to_string(),
            ),
            Self::Other(err) => err,
        }
    }
}

impl From<TuliproxError> for TargetGroupOrderPatchError {
    fn from(err: TuliproxError) -> Self { Self::Other(err) }
}

// ---------------------------------------------------------------------------
// Semantic application (operates on the DTO clone)
// ---------------------------------------------------------------------------

fn update_url_query_credentials_if_present(url: &mut String, username: &str, password: &str) {
    let Ok(mut parsed) = Url::parse(url.as_str()) else {
        return;
    };
    let mut pairs: Vec<(String, String)> = parsed.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect();
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

fn sort_aliases_by_exp_date_order(aliases: &mut [ConfigInputAliasDto], order: AliasExpDateSortOrder) -> bool {
    if aliases.len() < 2 {
        return false;
    }
    let compare = |a: &ConfigInputAliasDto, b: &ConfigInputAliasDto| {
        crate::repository::compare_alias_exp_date_with_order(a, b, order)
    };
    if aliases.windows(2).all(|pair| compare(&pair[0], &pair[1]) != std::cmp::Ordering::Greater) {
        return false;
    }
    aliases.sort_by(compare);
    true
}

const MAX_ALIAS_NAME_ATTEMPTS: usize = 1000;

pub(crate) fn derive_unique_alias_name(existing: &[Arc<str>], input_name: &Arc<str>, username: &str) -> Arc<str> {
    derive_unique_alias_name_with(
        |candidate| existing.iter().any(|name| name.as_ref() == candidate),
        input_name,
        username,
    )
    .intern()
}

pub(crate) fn resolve_provisioned_account_base_url(
    input_url: &str,
    base_url_from_response: Option<&str>,
    username: &str,
    password: &str,
) -> String {
    use shared::utils::{get_base_url_from_str, PROVIDER_SCHEME_PREFIX};
    if input_url.starts_with(PROVIDER_SCHEME_PREFIX) {
        let mut provider_url = input_url.to_string();
        update_url_query_credentials_if_present(&mut provider_url, username, password);
        return provider_url;
    }

    let base_url = base_url_from_response.map_or_else(|| input_url.to_string(), ToString::to_string);
    if let Some(origin) = get_base_url_from_str(base_url.as_str()) {
        let trimmed_origin = origin.trim();
        if !trimmed_origin.is_empty() && !trimmed_origin.eq_ignore_ascii_case("null") {
            return origin;
        }
    }

    let trimmed_base = base_url.trim();
    if !trimmed_base.is_empty() && !trimmed_base.eq_ignore_ascii_case("null") {
        return base_url;
    }

    input_url.to_string()
}

fn append_sources_yml_alias(
    input_name: &Arc<str>,
    input: &mut ConfigInputDto,
    alias_name: Arc<str>,
    base_url: String,
    username: String,
    password: String,
    exp_date: Option<i64>,
) -> Result<usize, TuliproxError> {
    let input_type = input.input_type;
    let aliases = input.aliases.get_or_insert_with(Vec::new);
    let next_index = aliases.iter().map(|alias| alias.id).max().unwrap_or(0);
    if next_index == u16::MAX {
        return Err(TuliproxError::ConfigPanelApi(format!(
            "panel_api: cannot add alias for '{input_name}': alias id overflow"
        )));
    }

    let mut alias = ConfigInputAliasDto {
        id: 0,
        name: alias_name,
        url: base_url,
        username: Some(username),
        password: Some(password),
        priority: 0,
        max_connections: 1,
        exp_date,
        enabled: true,
        stalker: None,
    };
    alias.prepare(next_index, &input_type)?;
    aliases.push(alias);
    Ok(aliases.len().saturating_sub(1))
}

pub(crate) fn derive_unique_alias_name_set(
    existing: &std::collections::HashSet<Arc<str>>,
    input_name: &Arc<str>,
    username: &str,
) -> String {
    derive_unique_alias_name_with(|candidate| existing.contains(candidate), input_name, username)
}

fn derive_unique_alias_name_with(mut contains: impl FnMut(&str) -> bool, input_name: &str, username: &str) -> String {
    let base = format!("{input_name}-{username}");
    if !contains(base.as_str()) {
        return base;
    }
    for i in 2..MAX_ALIAS_NAME_ATTEMPTS {
        let cand = format!("{base}-{i}");
        if !contains(cand.as_str()) {
            return cand;
        }
    }
    warn!(
        "derive_unique_alias_name: exhausted {MAX_ALIAS_NAME_ATTEMPTS} attempts for base '{base}'; returning potentially duplicate name"
    );
    base
}

pub(crate) fn apply_sources_yml_patches(
    doc: &mut SourcesConfigDto,
    patches: &[SourcesYmlPatch],
) -> Result<bool, TuliproxError> {
    apply_sources_yml_patches_inner(doc, patches).map_err(TargetGroupOrderPatchError::into_tuliprox_error)
}

#[allow(clippy::too_many_lines)]
fn apply_sources_yml_patches_inner(
    doc: &mut SourcesConfigDto,
    patches: &[SourcesYmlPatch],
) -> Result<bool, TargetGroupOrderPatchError> {
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
            SourcesYmlPatch::SetTargetGroupOrder { target_name, cluster, groups } => {
                changed |= apply_target_group_order(doc, target_name, *cluster, groups)?;
            }
            _ => {
                changed |= apply_account_source_patch(&inputs_by_name, &mut alias_indices, doc, patch)?;
            }
        }
    }

    Ok(changed)
}

#[allow(clippy::too_many_lines)]
fn apply_account_source_patch(
    inputs_by_name: &HashMap<Arc<str>, usize>,
    alias_indices: &mut [HashMap<Arc<str>, usize>],
    doc: &mut SourcesConfigDto,
    patch: &SourcesYmlPatch,
) -> Result<bool, TuliproxError> {
    let mut changed = false;
    match patch {
        SourcesYmlPatch::SetObservedAccount { input_name, observation } => {
            let Some(input) = doc.inputs.iter().find(|input| input.name == *input_name) else {
                return Ok(false);
            };
            let account = if input.name == observation.name {
                Some((&input.url, input.username.as_deref(), input.password.as_deref()))
            } else {
                input
                    .aliases
                    .iter()
                    .flatten()
                    .find(|alias| alias.name == observation.name)
                    .map(|alias| (&alias.url, alias.username.as_deref(), alias.password.as_deref()))
            };
            let Some((url, username, password)) = account else {
                return Ok(false);
            };
            let url = crate::utils::resolve_env_var(url);
            let username = username.map(crate::utils::resolve_env_var);
            let password = password.map(crate::utils::resolve_env_var);
            if shared::model::ProviderAccountIdentity::new(url.trim(), username.as_deref(), password.as_deref())
                != observation.identity
            {
                return Ok(false);
            }
            // Only confirmed terminal states are persisted; transient ones stay runtime exclusions.
            let disable = observation.is_persistent_block();
            let command = match observation.exp_date {
                Some(exp_date) => SourcesYmlPatch::SetFetchedExpiry {
                    input_name: Arc::clone(input_name),
                    account_name: Arc::clone(&observation.name),
                    exp_date,
                    disable,
                },
                None if disable => SourcesYmlPatch::SetAccountDisabled {
                    input_name: Arc::clone(input_name),
                    account_name: Arc::clone(&observation.name),
                },
                None => return Ok(false),
            };
            changed |= apply_sources_yml_patches(doc, std::slice::from_ref(&command))?;
        }
        SourcesYmlPatch::SetAccountDisabled { input_name, account_name } => {
            let idx = *inputs_by_name
                .get(input_name.as_ref())
                .ok_or_else(|| TuliproxError::ConfigInput(format!("Missing input {input_name}")))?;
            let input = &mut doc.inputs[idx];
            if input.name == *account_name {
                changed |= !input.account_disabled;
                input.account_disabled = true;
                return Ok(changed);
            }
            let alias = input
                .aliases
                .as_mut()
                .and_then(|aliases| aliases.iter_mut().find(|alias| alias.name == *account_name))
                .ok_or_else(|| TuliproxError::ConfigInput(format!("Missing alias {account_name}")))?;
            changed |= std::mem::replace(&mut alias.enabled, false);
        }
        SourcesYmlPatch::SetFetchedExpiry { input_name, account_name, exp_date, disable } => {
            let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
            })?;
            let account_changed =
                doc.inputs[idx].update_account_expiration_date(account_name.as_ref(), *exp_date, *disable)?;
            if account_changed {
                changed = true;
            }
        }
        SourcesYmlPatch::UpdatePanelAccountExpiry { input_name, account_name, exp_date } => {
            let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
            })?;
            if account_name == input_name {
                if doc.inputs[idx].exp_date != Some(*exp_date)
                    || doc.inputs[idx].account_disabled
                    || !doc.inputs[idx].enabled
                    || doc.inputs[idx].max_connections != 1
                {
                    doc.inputs[idx].exp_date = Some(*exp_date);
                    doc.inputs[idx].enabled = true;
                    doc.inputs[idx].account_disabled = false;
                    doc.inputs[idx].max_connections = 1;
                    changed = true;
                }
                return Ok(changed);
            }
            let Some(alias_idx) = alias_indices[idx].get(account_name).copied() else {
                return Err(TuliproxError::ConfigPanelApi(format!(
                    "source.yml patch target alias '{account_name}' under input '{input_name}' was not found"
                )));
            };
            let aliases = doc.inputs[idx].aliases.as_mut().ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch: input '{input_name}' has no aliases"))
            })?;
            let alias = &mut aliases[alias_idx];
            // A panel renewal is an explicit decision for this account and lifts a stored exclusion.
            if alias.exp_date != Some(*exp_date) || alias.max_connections != 1 || !alias.enabled {
                alias.exp_date = Some(*exp_date);
                alias.max_connections = 1;
                alias.enabled = true;
                changed = true;
            }
        }
        SourcesYmlPatch::UpdatePanelApiCredits { input_name, credits } => {
            let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
            })?;
            let Some(panel_api) = doc.inputs[idx].panel_api.as_mut() else {
                return Err(TuliproxError::ConfigPanelApi(format!(
                    "source.yml patch: could not find panel_api for input '{input_name}'"
                )));
            };
            if panel_api.credits.as_deref().map(str::trim) != Some(credits.trim()) {
                panel_api.credits = Some(credits.trim().to_string());
                changed = true;
            }
        }
        SourcesYmlPatch::SortAliases { input_name, order } => {
            let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
            })?;
            let Some(aliases) = doc.inputs[idx].aliases.as_mut() else {
                return Ok(false);
            };
            if sort_aliases_by_exp_date_order(aliases, *order) {
                alias_indices[idx] = aliases.iter().enumerate().map(|(idx, alias)| (alias.name.clone(), idx)).collect();
                changed = true;
            }
        }
        SourcesYmlPatch::UpdateRootCredentials { input_name, username, password, exp_date } => {
            let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
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
                input.account_disabled = false;
                input.max_connections = 1;
                if let Some(exp_date) = *exp_date {
                    input.exp_date = Some(exp_date);
                }
                update_url_query_credentials_if_present(&mut input.url, username, password);
                changed = true;
            }
        }
        SourcesYmlPatch::PersistProvisionedAccount { input_name, username, password, exp_date } => {
            let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
            })?;
            let input = &mut doc.inputs[idx];
            let current_root_is_usable =
                input.exp_date.is_some() && !is_input_expired_at(input.exp_date, jsonwebtoken::get_current_timestamp());
            if current_root_is_usable {
                let mut existing_names = vec![input.name.clone()];
                if let Some(aliases) = input.aliases.as_ref() {
                    existing_names.extend(aliases.iter().map(|alias| alias.name.clone()));
                }
                let alias_name = derive_unique_alias_name(&existing_names, &input.name, username);
                let base_url = resolve_provisioned_account_base_url(input.url.as_str(), None, username, password);
                let alias_idx = append_sources_yml_alias(
                    input_name,
                    input,
                    Arc::clone(&alias_name),
                    base_url,
                    username.clone(),
                    password.clone(),
                    *exp_date,
                )?;
                alias_indices[idx].insert(Arc::clone(&alias_name), alias_idx);
                if let Some(aliases) = input.aliases.as_mut() {
                    if sort_aliases_by_exp_date_order(aliases, AliasExpDateSortOrder::NewestFirst) {
                        alias_indices[idx] =
                            aliases.iter().enumerate().map(|(idx, alias)| (alias.name.clone(), idx)).collect();
                    }
                }
                return Ok(true);
            }
            let exp_date_changed = exp_date.is_some() && input.exp_date != *exp_date;
            if input.username.as_deref() != Some(username.as_str())
                || input.password.as_deref() != Some(password.as_str())
                || exp_date_changed
            {
                input.username = Some(username.clone());
                input.password = Some(password.clone());
                input.enabled = true;
                input.account_disabled = false;
                input.max_connections = 1;
                if let Some(exp_date) = *exp_date {
                    input.exp_date = Some(exp_date);
                }
                update_url_query_credentials_if_present(&mut input.url, username, password);
                changed = true;
            }
        }
        SourcesYmlPatch::UpdateAliasCredentials { input_name, alias_name, username, password, exp_date } => {
            let idx = *inputs_by_name.get(input_name.as_ref()).ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
            })?;
            let Some(alias_idx) = alias_indices[idx].get(alias_name).copied() else {
                return Err(TuliproxError::ConfigPanelApi(format!(
                    "source.yml patch target alias '{alias_name}' under input '{input_name}' was not found"
                )));
            };
            let aliases = doc.inputs[idx].aliases.as_mut().ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch: input '{input_name}' has no aliases"))
            })?;
            let alias = &mut aliases[alias_idx];
            let exp_date_changed = exp_date.is_some() && alias.exp_date != *exp_date;
            if alias.username.as_deref() != Some(username.as_str())
                || alias.password.as_deref() != Some(password.as_str())
                || exp_date_changed
                || !alias.enabled
            {
                alias.username = Some(username.clone());
                alias.password = Some(password.clone());
                alias.enabled = true;
                alias.max_connections = 1;
                if let Some(exp_date) = *exp_date {
                    alias.exp_date = Some(exp_date);
                }
                update_url_query_credentials_if_present(&mut alias.url, username, password);
                changed = true;
            }
        }
        SourcesYmlPatch::AddAlias { input_name, alias_name, base_url, username, password, exp_date } => {
            let idx = *inputs_by_name.get(input_name).ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
            })?;
            let mut existing =
                doc.inputs[idx].aliases.iter().flatten().filter(|alias| alias.name.trim() == alias_name.trim());
            if let Some(alias) = existing.next() {
                if existing.next().is_some() {
                    return Err(TuliproxError::ConfigPanelApi(format!(
                        "source.yml patch target alias '{alias_name}' under input '{input_name}' is ambiguous"
                    )));
                }
                if alias.url == *base_url
                    && alias.username.as_deref() == Some(username.as_str())
                    && alias.password.as_deref() == Some(password.as_str())
                    && alias.exp_date == *exp_date
                {
                    return Ok(false);
                }
                return Err(TuliproxError::ConfigPanelApi(format!(
                        "source.yml patch: alias '{alias_name}' already exists under input '{input_name}' with different data"
                    )));
            }
            let alias_idx = append_sources_yml_alias(
                input_name,
                &mut doc.inputs[idx],
                Arc::clone(alias_name),
                base_url.clone(),
                username.clone(),
                password.clone(),
                *exp_date,
            )?;
            alias_indices[idx].insert(Arc::clone(alias_name), alias_idx);
            changed = true;
        }
        SourcesYmlPatch::RemoveExpiredAliases { input_name } => {
            let idx = *inputs_by_name.get(input_name).ok_or_else(|| {
                TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
            })?;
            let Some(aliases) = doc.inputs[idx].aliases.as_mut() else {
                return Ok(false);
            };
            let before = aliases.len();
            aliases.retain(|a| !is_input_expired(a.exp_date));
            if aliases.len() != before {
                alias_indices[idx] = aliases.iter().enumerate().map(|(idx, alias)| (alias.name.clone(), idx)).collect();
                changed = true;
            }
        }
        SourcesYmlPatch::SetTargetGroupOrder { .. } => {}
    }

    Ok(changed)
}

// ---------------------------------------------------------------------------
// Explorer group order (managed sort rule)
// ---------------------------------------------------------------------------

/// Filters a managed group-order rule of `cluster` is written with.
fn cluster_filter_text(cluster: TargetGroupCluster) -> &'static str {
    match cluster {
        TargetGroupCluster::Live => "Type = live",
        TargetGroupCluster::Vod => "Type = vod",
        TargetGroupCluster::Series => "Type = series",
    }
}

/// Canonical rendering of the filter of a managed rule.
///
/// `Type = vod` parses to the movie item type, whose canonical rendering is `Type = movie`; the
/// prepared runtime configuration therefore reports the second spelling.
fn cluster_filter_canonical(cluster: TargetGroupCluster) -> &'static str {
    match cluster {
        TargetGroupCluster::Live => "Type = live",
        TargetGroupCluster::Vod => "Type = movie",
        TargetGroupCluster::Series => "Type = series",
    }
}

/// Whether a stored filter text means the cluster's own type filter.
///
/// The comparison goes through the filter parser, so a spelling such as `Type=live` or
/// `type = LIVE` is recognized as the same rule instead of turning it into a user rule that a save
/// would not update.
fn matches_cluster_filter(filter_text: &str, cluster: TargetGroupCluster) -> bool {
    shared::foundation::get_filter(filter_text, None)
        .is_ok_and(|filter| filter.to_string().eq_ignore_ascii_case(cluster_filter_canonical(cluster)))
}

/// Escapes a literal group name for an anchored regex, encoding `!` as `\x21` so `!name!`
/// template placeholders cannot appear in stored sort sequences.
fn encode_group_literal(name: &str) -> String { regex::escape(name).replace('!', "\\x21") }

/// Sequence entries of a managed rule: one anchored, escaped literal per group name.
///
/// `regex::escape` turns every metacharacter of the name into a literal and `!` becomes `\x21`,
/// so a group called `News !HD!` matches only itself without triggering template expansion.
fn managed_sequence(groups: &[String]) -> Vec<String> {
    groups.iter().map(|name| format!("^{}$", encode_group_literal(name))).collect()
}

/// Recovers the group name from one managed sequence entry `^<escaped group name>$`.
///
/// An entry that only looks anchored is rejected: the unescaped text must encode back to the exact
/// entry, so `^A.B$`, `^A\d$`, and `^A\x41$` are no managed literals.
pub(crate) fn decode_group_sequence_entry(pattern: &str) -> Option<String> {
    let inner = pattern.strip_prefix('^')?.strip_suffix('$')?;
    if inner.is_empty() {
        return None;
    }
    let mut name = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let escaped = chars.next()?;
            if escaped == 'x' {
                let d1 = chars.next()?;
                let d2 = chars.next()?;
                if (d1, d2) != ('2', '1') {
                    return None;
                }
                name.push('!');
            } else {
                name.push(escaped);
            }
        } else {
            name.push(ch);
        }
    }
    (encode_group_literal(&name) == inner).then_some(name)
}

/// Whether the sequence is non-empty and every entry is an anchored escaped literal.
fn is_literal_sequence<'a>(patterns: impl IntoIterator<Item = &'a str>) -> bool {
    let mut iter = patterns.into_iter();
    let Some(first) = iter.next() else {
        return false;
    };
    decode_group_sequence_entry(first).is_some() && iter.all(|pattern| decode_group_sequence_entry(pattern).is_some())
}

/// Whether a rule carries the managed shape of the Explorer rule of `cluster`.
///
/// The shape is exact on purpose: every other rule of the target belongs to the user and is never
/// modified. A sequence entry that is a pattern rather than a literal name makes the rule a user
/// rule as well.
fn is_managed_group_rule(
    target: SortTarget,
    field: ItemField,
    order: SortOrder,
    natural: bool,
    filter: &str,
    literal_sequence: bool,
    cluster: TargetGroupCluster,
) -> bool {
    literal_sequence
        && target == SortTarget::Group
        && field == ItemField::Group
        && order == SortOrder::None
        && !natural
        && matches_cluster_filter(filter, cluster)
}

fn is_managed_dto_rule(rule: &ConfigSortRuleDto, cluster: TargetGroupCluster) -> bool {
    let literal_sequence =
        rule.sequence.as_ref().is_some_and(|sequence| is_literal_sequence(sequence.iter().map(String::as_str)));
    is_managed_group_rule(rule.target, rule.field, rule.order, rule.natural, &rule.filter, literal_sequence, cluster)
}

/// Decodes the stored Explorer group order of `cluster` from a raw (un-prepared) sort DTO.
pub(crate) fn stored_group_order_from_sort_dto(
    sort: Option<&ConfigSortDto>,
    cluster: TargetGroupCluster,
) -> Vec<String> {
    sort.and_then(|sort| sort.rules.iter().find(|rule| is_managed_dto_rule(rule, cluster)))
        .and_then(|rule| rule.sequence.as_ref())
        .map(|sequence| sequence.iter().filter_map(|pattern| decode_group_sequence_entry(pattern)).collect())
        .unwrap_or_default()
}

/// The managed rule as it is stored: target and field `group`, order `none` and one anchored
/// escaped literal per group name.
fn managed_group_rule_dto(cluster: TargetGroupCluster, groups: &[String]) -> ConfigSortRuleDto {
    ConfigSortRuleDto {
        target: SortTarget::Group,
        field: ItemField::Group,
        order: SortOrder::None,
        natural: false,
        sequence: Some(managed_sequence(groups)),
        filter: cluster_filter_text(cluster).to_string(),
        t_sequence: None,
        t_filter: None,
    }
}

fn find_target_dto_mut<'a>(
    doc: &'a mut SourcesConfigDto,
    target_name: &str,
) -> Result<&'a mut shared::model::ConfigTargetDto, TargetGroupOrderPatchError> {
    let mut found: Option<(usize, usize)> = None;
    for (source_idx, source) in doc.sources.iter().enumerate() {
        for (target_idx, target) in source.targets.iter().enumerate() {
            if target.name == target_name {
                if found.is_some() {
                    return Err(TargetGroupOrderPatchError::UnsupportedLayout(format!(
                        "source.yml patch: target '{target_name}' is ambiguous"
                    )));
                }
                found = Some((source_idx, target_idx));
            }
        }
    }
    let (source_idx, target_idx) = found.ok_or_else(|| {
        TargetGroupOrderPatchError::TargetNotFound(format!("source.yml patch: target '{target_name}' was not found"))
    })?;
    doc.sources.get_mut(source_idx).and_then(|source| source.targets.get_mut(target_idx)).ok_or_else(|| {
        TargetGroupOrderPatchError::Other(TuliproxError::ConfigPanelApi(
            "source.yml patch: target projection changed".to_string(),
        ))
    })
}

/// Sets or removes the managed group rule of one cluster of one target.
///
/// The managed rule is always the first rule of the target; the rules of the user keep their own
/// order after it. The `sort` block disappears only when the rule list becomes empty and no other
/// sort key is set.
fn apply_target_group_order(
    doc: &mut SourcesConfigDto,
    target_name: &str,
    cluster: TargetGroupCluster,
    groups: &[String],
) -> Result<bool, TargetGroupOrderPatchError> {
    let target = find_target_dto_mut(doc, target_name)?;
    let previous = target.sort.take();
    let mut sort = previous.clone().unwrap_or_default();
    let managed_idx = sort.rules.iter().position(|rule| is_managed_dto_rule(rule, cluster));
    if groups.is_empty() {
        let Some(index) = managed_idx else {
            target.sort = previous;
            return Ok(false);
        };
        sort.rules.remove(index);
    } else {
        if let Some(index) = managed_idx {
            sort.rules.remove(index);
        }
        sort.rules.insert(0, managed_group_rule_dto(cluster, groups));
    }
    let next = if sort.rules.is_empty() && !sort.match_as_ascii && sort.strategy.is_none() { None } else { Some(sort) };
    let changed = next != previous;
    target.sort = next;
    Ok(changed)
}

// ---------------------------------------------------------------------------
// Explorer group order text planning
// ---------------------------------------------------------------------------

/// Projection of `sources[].targets[].sort` with byte spans.
///
/// Only what the group-order planner reads is projected; every other target key is ignored, so a
/// target with unrelated settings parses unchanged.
#[derive(Debug, serde::Deserialize)]
struct TargetSortDocument {
    #[serde(default)]
    sources: Vec<serde_saphyr::Spanned<PatchSourceTargets>>,
}

#[derive(Debug, serde::Deserialize)]
struct PatchSourceTargets {
    #[serde(default)]
    targets: Vec<serde_saphyr::Spanned<PatchTargetSort>>,
}

#[derive(Debug, serde::Deserialize)]
struct PatchTargetSort {
    name: serde_saphyr::Spanned<String>,
    #[serde(default)]
    sort: Option<serde_saphyr::Spanned<PatchTargetSortBlock>>,
}

#[derive(Debug, serde::Deserialize)]
struct PatchTargetSortBlock {
    #[serde(default)]
    match_as_ascii: Option<serde_saphyr::Spanned<bool>>,
    #[serde(default)]
    rules: Option<serde_saphyr::Spanned<Vec<serde_saphyr::Spanned<PatchSortRule>>>>,
    #[serde(default)]
    strategy: Option<serde_saphyr::Spanned<String>>,
}

#[derive(Debug, serde::Deserialize)]
struct PatchSortRule {
    target: serde_saphyr::Spanned<SortTarget>,
    field: serde_saphyr::Spanned<ItemField>,
    order: serde_saphyr::Spanned<SortOrder>,
    #[serde(default)]
    natural: Option<serde_saphyr::Spanned<bool>>,
    filter: serde_saphyr::Spanned<String>,
    #[serde(default)]
    sequence: Option<serde_saphyr::Spanned<Vec<serde_saphyr::Spanned<String>>>>,
}

fn parse_target_sort_document(text: &str) -> Result<TargetSortDocument, TuliproxError> {
    shared::utils::yaml_from_str(text)
        .map_err(|err| TuliproxError::Config(format!("source.yml patch: target projection parse failed: {err}")))
}

const UNSUPPORTED_SORT_LAYOUT_MSG: &str =
    "the target's sort block uses a layout that cannot be edited automatically; edit sort.rules in source.yml by hand";

fn unsupported_layout() -> TargetGroupOrderPatchError {
    TargetGroupOrderPatchError::UnsupportedLayout(UNSUPPORTED_SORT_LAYOUT_MSG.to_string())
}

fn span_range(spanned: &serde_saphyr::Spanned<impl Sized>) -> Result<Range<usize>, TargetGroupOrderPatchError> {
    span_byte_range(spanned).map_err(|_| unsupported_layout())
}

fn find_patch_target<'a>(
    doc: &'a TargetSortDocument,
    target_name: &str,
) -> Result<&'a serde_saphyr::Spanned<PatchTargetSort>, TargetGroupOrderPatchError> {
    let mut found: Option<&serde_saphyr::Spanned<PatchTargetSort>> = None;
    for source in &doc.sources {
        for target in &source.value.targets {
            if target.value.name.value == target_name {
                if found.is_some() {
                    return Err(TargetGroupOrderPatchError::UnsupportedLayout(format!(
                        "source.yml patch: target '{target_name}' is ambiguous"
                    )));
                }
                found = Some(target);
            }
        }
    }
    found.ok_or_else(|| {
        TargetGroupOrderPatchError::TargetNotFound(format!("source.yml patch: target '{target_name}' was not found"))
    })
}

fn is_managed_projection_rule(rule: &PatchSortRule, cluster: TargetGroupCluster) -> bool {
    let literal_sequence = rule
        .sequence
        .as_ref()
        .is_some_and(|sequence| is_literal_sequence(sequence.value.iter().map(|pattern| pattern.value.as_str())));
    is_managed_group_rule(
        rule.target.value,
        rule.field.value,
        rule.order.value,
        rule.natural.as_ref().is_some_and(|natural| natural.value),
        &rule.filter.value,
        literal_sequence,
        cluster,
    )
}

/// Byte extent of one rule.
///
/// A `Spanned` collection reports the position where it starts, not its extent, so the extent of a
/// rule comes from the scalars it contains.
fn rule_extent(rule: &PatchSortRule) -> Result<Range<usize>, TargetGroupOrderPatchError> {
    let mut extent: Option<Range<usize>> = None;
    let mut scalars =
        vec![span_range(&rule.target)?, span_range(&rule.field)?, span_range(&rule.order)?, span_range(&rule.filter)?];
    if let Some(natural) = &rule.natural {
        scalars.push(span_range(natural)?);
    }
    for item in rule.sequence.iter().flat_map(|sequence| &sequence.value) {
        scalars.push(span_range(item)?);
    }
    for span in scalars {
        extent = Some(match extent {
            Some(current) => current.start.min(span.start)..current.end.max(span.end),
            None => span,
        });
    }
    extent.ok_or_else(unsupported_layout)
}

/// Byte offset where the last scalar of the `sort` block ends.
fn sort_block_end(sort: &PatchTargetSortBlock) -> Result<usize, TargetGroupOrderPatchError> {
    let mut ends: Vec<usize> = Vec::new();
    if let Some(flag) = &sort.match_as_ascii {
        ends.push(span_range(flag)?.end);
    }
    if let Some(strategy) = &sort.strategy {
        ends.push(span_range(strategy)?.end);
    }
    for rule in sort.rules.iter().flat_map(|rules| &rules.value) {
        ends.push(rule_extent(&rule.value)?.end);
    }
    ends.into_iter().max().ok_or_else(unsupported_layout)
}

fn line_start(text: &str, pos: usize) -> usize { text[..pos.min(text.len())].rfind('\n').map_or(0, |found| found + 1) }

fn strip_yaml_key<'a>(trimmed_line: &'a str, key: &str) -> Option<&'a str> {
    trimmed_line.strip_prefix(key)?.trim_start().strip_prefix(':')
}

fn block_sequence_lines(patterns: &[String], item_indent: usize, newline: &str) -> Result<String, TuliproxError> {
    let mut lines = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let scalar = serialize_yaml_scalar(pattern)?;
        lines.push(format!("{:item_indent$}- {scalar}", ""));
    }
    Ok(lines.join(newline))
}

/// The managed rule as YAML block lines of one sequence item, starting at the dash.
fn block_rule_lines(
    cluster: TargetGroupCluster,
    patterns: &[String],
    dash_indent: usize,
    newline: &str,
) -> Result<String, TuliproxError> {
    let key_indent = dash_indent + 2;
    let filter_scalar = serialize_yaml_scalar(&cluster_filter_text(cluster))?;
    let seq_lines = block_sequence_lines(patterns, key_indent + 2, newline)?;

    let mut out = format!("{:dash_indent$}- target: group", "");
    for (key, value) in [("field", "group"), ("order", "none")] {
        out.push_str(newline);
        let _ = write!(out, "{:key_indent$}{key}: {value}", "");
    }
    out.push_str(newline);
    let _ = write!(out, "{:key_indent$}filter: {filter_scalar}", "");
    out.push_str(newline);
    let _ = write!(out, "{:key_indent$}sequence:", "");
    if !seq_lines.is_empty() {
        out.push_str(newline);
        out.push_str(&seq_lines);
    }
    Ok(out)
}

/// Inserts a block mapping key (`key:`) followed by `build_body(indent)` after `anchor`.
fn insert_block_section_edit(
    text: &str,
    anchor: &Range<usize>,
    key: &str,
    build_body: impl FnOnce(usize) -> Result<String, TuliproxError>,
) -> Result<TextEdit, TargetGroupOrderPatchError> {
    let mut edit = build_field_insertion_edit(text, anchor, key, "")?;
    if edit.replacement.ends_with(' ') {
        edit.replacement.pop();
    }
    let indent = sibling_key_indent(text, anchor.start);
    let newline = detect_newline(text);
    edit.replacement.push_str(newline);
    edit.replacement.push_str(&build_body(indent)?);
    Ok(edit)
}

/// Indentation of the dash of a block sequence item whose content starts at `span_start`.
fn block_item_indent(text: &str, span_start: usize) -> Result<usize, TargetGroupOrderPatchError> {
    let start = line_start(text, span_start);
    let indent = line_indent_at(text, start);
    let prefix = text.get(start + indent..span_start).unwrap_or_default();
    let Some(after_dash) = prefix.strip_prefix('-') else {
        return Err(unsupported_layout());
    };
    if !after_dash.starts_with([' ', '\t']) || after_dash.trim_start().starts_with(['&', '!', '{', '[']) {
        return Err(unsupported_layout());
    }
    Ok(indent)
}

/// The whole lines a byte extent touches.
fn block_lines(text: &str, extent: &Range<usize>) -> Range<usize> {
    line_start(text, extent.start)..line_end_offset(text, extent.end)
}

/// Closing delimiter index within a single line, respecting YAML single and double quotes.
fn single_line_flow_close(line: &str, open_in_line: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = line.as_bytes();
    if bytes.get(open_in_line) != Some(&open) {
        return None;
    }
    let mut depth = 0usize;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut index = open_in_line;
    while let Some(&byte) = bytes.get(index) {
        if in_single_quote {
            if byte == b'\'' {
                if bytes.get(index + 1) == Some(&b'\'') {
                    index += 2;
                    continue;
                }
                in_single_quote = false;
            }
            index += 1;
            continue;
        }
        if in_double_quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            in_double_quote = byte != b'"';
            index += 1;
            continue;
        }
        if byte == b'\'' {
            in_single_quote = true;
        } else if byte == b'"' {
            in_double_quote = true;
        } else if byte == open {
            depth += 1;
        } else if byte == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

/// Byte bounds `target_start..target_end` and key indentation of a block-style target mapping.
fn target_block_bounds(
    text: &str,
    name_range: &Range<usize>,
) -> Result<(Range<usize>, usize), TargetGroupOrderPatchError> {
    let field_indent = sibling_key_indent(text, name_range.start);
    let mut cur_start = line_start(text, name_range.start);
    let target_start = loop {
        let indent = line_indent_at(text, cur_start);
        let content_end = line_content_end(text, cur_start);
        let trimmed = text[cur_start + indent..content_end].trim_start();
        if indent < field_indent && !trimmed.is_empty() && !trimmed.starts_with('#') {
            if trimmed.starts_with('-') {
                break cur_start;
            }
            return Err(unsupported_layout());
        }
        if cur_start == 0 {
            return Err(unsupported_layout());
        }
        cur_start = line_start(text, cur_start - 1);
    };

    let mut pos = line_end_offset(text, name_range.end);
    let target_end = loop {
        if pos >= text.len() {
            break text.len();
        }
        let content_end = line_content_end(text, pos);
        let indent = line_indent_at(text, pos);
        let trimmed = text[pos + indent..content_end].trim_start();
        if !trimmed.is_empty() && !trimmed.starts_with('#') && indent < field_indent {
            break pos;
        }
        pos = line_end_offset(text, pos);
    };

    Ok((target_start..target_end, field_indent))
}

/// Finds a block mapping key line at `key_indent` inside `range` and rejects non-empty header
/// trailers (flow values, `null`, inline comments, anchors, tags) or duplicate keys.
fn find_block_key_line(
    text: &str,
    range: Range<usize>,
    key_indent: usize,
    key: &str,
) -> Result<Option<Range<usize>>, TargetGroupOrderPatchError> {
    let mut found: Option<Range<usize>> = None;
    let mut pos = range.start;
    while pos < range.end {
        let next_pos = line_end_offset(text, pos).min(range.end);
        let content_end = line_content_end(text, pos).min(range.end);
        let indent = line_indent_at(text, pos);
        let line_body = &text[(pos + indent).min(content_end)..content_end];
        if pos == range.start && line_body.starts_with('-') {
            let after_dash = line_body[1..].trim_start();
            if strip_yaml_key(after_dash, key).is_some() {
                return Err(unsupported_layout());
            }
        } else if indent == key_indent {
            if let Some(after_colon) = strip_yaml_key(line_body, key) {
                if !after_colon.trim().is_empty() || found.is_some() {
                    return Err(unsupported_layout());
                }
                found = Some(pos..next_pos);
            }
        }
        pos = next_pos;
    }
    Ok(found)
}

/// Byte bounds `body_start..body_end` of a nested block value starting after `header_line`.
fn block_body_bounds(text: &str, header_end: usize, parent_end: usize, parent_key_indent: usize) -> Range<usize> {
    let mut pos = header_end;
    while pos < parent_end {
        let content_end = line_content_end(text, pos).min(parent_end);
        let indent = line_indent_at(text, pos);
        let trimmed = text[(pos + indent).min(content_end)..content_end].trim_start();
        if !trimmed.is_empty() && !trimmed.starts_with('#') && indent <= parent_key_indent {
            return header_end..pos;
        }
        pos = line_end_offset(text, pos).min(parent_end);
    }
    header_end..parent_end
}

/// First non-empty, non-comment line in `range`: `(indent, trimmed_body)`.
fn first_content_line(text: &str, range: Range<usize>) -> Option<(usize, &str)> {
    let mut pos = range.start;
    while pos < range.end {
        let content_end = line_content_end(text, pos).min(range.end);
        let indent = line_indent_at(text, pos);
        let body = text[(pos + indent).min(content_end)..content_end].trim_start();
        if !body.is_empty() && !body.starts_with('#') {
            return Some((indent, body));
        }
        pos = line_end_offset(text, pos).min(range.end);
    }
    None
}

fn validate_single_line_flow_rule(
    text: &str,
    flow_start: usize,
    extent: &Range<usize>,
) -> Result<(), TargetGroupOrderPatchError> {
    let item_line_start = line_start(text, flow_start);
    if line_start(text, extent.end) != item_line_start {
        return Err(unsupported_layout());
    }
    let indent = line_indent_at(text, item_line_start);
    let prefix = text.get(item_line_start + indent..flow_start).unwrap_or_default();
    let Some(after_dash) = prefix.strip_prefix('-') else {
        return Err(unsupported_layout());
    };
    if after_dash.is_empty() || !after_dash.chars().all(|ch| ch == ' ' || ch == '\t') {
        return Err(unsupported_layout());
    }
    let item_line_end = line_content_end(text, flow_start);
    let line = &text[item_line_start..item_line_end];
    let Some(close_in_line) = single_line_flow_close(line, flow_start - item_line_start, b'{', b'}') else {
        return Err(unsupported_layout());
    };
    let trailing = line[close_in_line + 1..].trim_start();
    if !trailing.is_empty() && !trailing.starts_with('#') {
        return Err(unsupported_layout());
    }
    Ok(())
}

fn validate_block_rule_sequence_field(text: &str, rule: &PatchSortRule) -> Result<(), TargetGroupOrderPatchError> {
    let Some(seq) = &rule.sequence else {
        return Ok(());
    };
    let seq_start = span_range(seq)?.start;
    if !text.get(seq_start..).is_some_and(|rest| rest.trim_start().starts_with('[')) {
        return Ok(());
    }
    let seq_line_start = line_start(text, seq_start);
    let seq_line = &text[seq_line_start..line_content_end(text, seq_start)];
    let Some(rel_open) = seq_line.get(seq_start - seq_line_start..).and_then(|s| s.find('[')) else {
        return Err(unsupported_layout());
    };
    let open_in_line = (seq_start - seq_line_start) + rel_open;
    let Some(close_in_line) = single_line_flow_close(seq_line, open_in_line, b'[', b']') else {
        return Err(unsupported_layout());
    };
    let trailing = seq_line[close_in_line + 1..].trim_start();
    if !trailing.is_empty() && !trailing.starts_with('#') {
        return Err(unsupported_layout());
    }
    Ok(())
}

/// Validates that every item in a block `rules:` sequence is either a block mapping or a
/// single-line flow mapping, with no multiline flow collections or YAML anchors/tags.
fn validate_rule_list_items(
    text: &str,
    rules: &[serde_saphyr::Spanned<PatchSortRule>],
) -> Result<(), TargetGroupOrderPatchError> {
    let mut flow_scanner = FlowScopeScanner::new(text);
    for rule in rules {
        let extent = rule_extent(&rule.value)?;
        if let Some(flow_start) = flow_scanner.flow_mapping_start_at(extent.start) {
            validate_single_line_flow_rule(text, flow_start, &extent)?;
        } else {
            block_item_indent(text, extent.start)?;
            validate_block_rule_sequence_field(text, &rule.value)?;
        }
    }
    Ok(())
}

struct ValidatedSortLayout {
    sort_line: Option<Range<usize>>,
    rules_line: Option<Range<usize>>,
}

/// Validates that the target and its `sort` block use a supported block layout before planning
/// any edit.
fn validate_target_sort_layout(
    text: &str,
    target: &PatchTargetSort,
) -> Result<ValidatedSortLayout, TargetGroupOrderPatchError> {
    let name_range = span_range(&target.name)?;
    if is_flow_style_mapping_at(text, &name_range) {
        return Err(unsupported_layout());
    }
    let (target_bounds, target_field_indent) = target_block_bounds(text, &name_range)?;
    let sort_line = find_block_key_line(text, target_bounds.clone(), target_field_indent, "sort")?;

    let Some(sort_spanned) = &target.sort else {
        if sort_line.is_some() {
            return Err(unsupported_layout());
        }
        return Ok(ValidatedSortLayout { sort_line: None, rules_line: None });
    };

    let Some(sort_line_range) = sort_line else {
        return Err(unsupported_layout());
    };
    span_range(sort_spanned)?;

    let sort = &sort_spanned.value;
    if sort.match_as_ascii.is_none() && sort.strategy.is_none() && sort.rules.is_none() {
        return Err(unsupported_layout());
    }
    for child in [
        sort.match_as_ascii.as_ref().map(span_range).transpose()?,
        sort.strategy.as_ref().map(span_range).transpose()?,
        sort.rules.as_ref().map(span_range).transpose()?,
    ]
    .into_iter()
    .flatten()
    {
        if is_flow_style_mapping_at(text, &child) {
            return Err(unsupported_layout());
        }
    }

    let sort_body = block_body_bounds(text, sort_line_range.end, target_bounds.end, target_field_indent);
    let Some((sort_child_indent, first_sort_line)) = first_content_line(text, sort_body.clone()) else {
        return Err(unsupported_layout());
    };
    if sort_child_indent <= target_field_indent || first_sort_line.starts_with(['{', '&', '!']) {
        return Err(unsupported_layout());
    }

    let rules_line = find_block_key_line(text, sort_body.clone(), sort_child_indent, "rules")?;
    match (&sort.rules, &rules_line) {
        (None, Some(_)) | (Some(_), None) => return Err(unsupported_layout()),
        (None, None) => {}
        (Some(rules_spanned), Some(rules_line_range)) => {
            if rules_spanned.value.is_empty() {
                return Err(unsupported_layout());
            }
            let rules_start = span_range(rules_spanned)?.start;
            if text.get(rules_start..).is_some_and(|rest| rest.trim_start().starts_with('[')) {
                return Err(unsupported_layout());
            }
            let rules_body = block_body_bounds(text, rules_line_range.end, sort_body.end, sort_child_indent);
            let Some((rule_indent, first_rule_line)) = first_content_line(text, rules_body) else {
                return Err(unsupported_layout());
            };
            if rule_indent < sort_child_indent || !first_rule_line.starts_with('-') {
                return Err(unsupported_layout());
            }
            validate_rule_list_items(text, &rules_spanned.value)?;
        }
    }

    Ok(ValidatedSortLayout { sort_line: Some(sort_line_range), rules_line })
}

/// Replaces the sequence of the managed block rule that is already first in the rule list.
fn plan_rule_sequence_edits(
    text: &str,
    rule: &PatchSortRule,
    cluster: TargetGroupCluster,
    patterns: &[String],
    edits: &mut Vec<TextEdit>,
) -> Result<(), TargetGroupOrderPatchError> {
    let Some(sequence) = &rule.sequence else {
        return Err(unsupported_layout());
    };
    let newline = detect_newline(text);
    let sequence_start = span_range(sequence)?.start;
    let rule_ext = rule_extent(rule)?;

    // Single-line flow managed rule or block rule with flow `sequence: [...]` is rewritten as a
    // canonical block managed rule so we never perform flow-collection editing.
    if flow_mapping_start_at(text, &rule_ext).is_some()
        || text.get(sequence_start..).is_some_and(|rest| rest.trim_start().starts_with('['))
    {
        // The canonical rule replaces every line the rule touches; a comment there would be lost.
        let range = block_lines(text, &rule_ext);
        if comment_starts_in(text, &range) {
            return Err(unsupported_layout());
        }
        let dash_indent = line_indent_at(text, line_start(text, rule_ext.start));
        edits.push(TextEdit {
            range,
            replacement: format!("{}{newline}", block_rule_lines(cluster, patterns, dash_indent, newline)?),
        });
        return Ok(());
    }

    // Reject anchors, tags, or inline comments on the `sequence:` header line itself.
    let rule_header_indent = sibling_key_indent(text, rule_ext.start);
    find_block_key_line(text, line_start(text, rule_ext.start)..sequence_start, rule_header_indent, "sequence")?;

    let (Some(first), Some(last)) = (sequence.value.first(), sequence.value.last()) else {
        return Err(unsupported_layout());
    };
    let first_span = span_range(first)?;
    let last_span = span_range(last)?;
    let item_indent = block_item_indent(text, first_span.start)?;
    let range = block_lines(text, &(first_span.start..last_span.end));
    if comment_starts_in(text, &range) {
        return Err(unsupported_layout());
    }
    edits.push(TextEdit {
        range,
        replacement: format!("{}{newline}", block_sequence_lines(patterns, item_indent, newline)?),
    });

    // A recognized rule may carry another spelling of the cluster filter (a user wrote
    // `Type = movie` for vod); storing it again normalizes the filter to the cluster's own text.
    if rule.filter.value != cluster_filter_text(cluster) {
        edits.push(TextEdit {
            range: span_range(&rule.filter)?,
            replacement: serialize_yaml_scalar(&cluster_filter_text(cluster))?,
        });
    }
    Ok(())
}

/// Plans the text edits that store the group order of one cluster of one target.
#[allow(clippy::too_many_lines)]
fn plan_target_group_order_edits(
    text: &str,
    doc: &TargetSortDocument,
    target_name: &str,
    cluster: TargetGroupCluster,
    groups: &[String],
    edits: &mut Vec<TextEdit>,
) -> Result<(), TargetGroupOrderPatchError> {
    let target = find_patch_target(doc, target_name)?;
    let layout = validate_target_sort_layout(text, &target.value)?;
    let patterns = managed_sequence(groups);
    let newline = detect_newline(text);

    let Some(sort_spanned) = &target.value.sort else {
        if patterns.is_empty() {
            return Ok(());
        }
        let anchor = span_range(&target.value.name)?;
        edits.push(insert_block_section_edit(text, &anchor, "sort", |indent| {
            Ok(format!(
                "{:sub_indent$}rules:{newline}{}",
                "",
                block_rule_lines(cluster, &patterns, indent + 4, newline)?,
                sub_indent = indent + 2
            ))
        })?);
        return Ok(());
    };

    let sort = &sort_spanned.value;
    let keeps_sort = sort.match_as_ascii.as_ref().is_some_and(|flag| flag.value) || sort.strategy.is_some();

    let Some(rules) = &sort.rules else {
        if patterns.is_empty() {
            return Ok(());
        }
        let anchor = match (&sort.match_as_ascii, &sort.strategy) {
            (Some(flag), Some(strategy)) => {
                let flag_span = span_range(flag)?;
                let strategy_span = span_range(strategy)?;
                if strategy_span.end > flag_span.end {
                    strategy_span
                } else {
                    flag_span
                }
            }
            (Some(flag), None) => span_range(flag)?,
            (None, Some(strategy)) => span_range(strategy)?,
            (None, None) => return Err(unsupported_layout()),
        };
        edits.push(insert_block_section_edit(text, &anchor, "rules", |indent| {
            block_rule_lines(cluster, &patterns, indent + 2, newline)
        })?);
        return Ok(());
    };

    let managed_idx = rules.value.iter().position(|rule| is_managed_projection_rule(&rule.value, cluster));
    if patterns.is_empty() && managed_idx.is_none() {
        return Ok(());
    }
    let rules_after =
        rules.value.len().saturating_sub(usize::from(managed_idx.is_some())) + usize::from(!patterns.is_empty());

    if rules_after == 0 {
        if keeps_sort {
            let rules_line = layout.rules_line.ok_or_else(unsupported_layout)?;
            let last = rules.value.last().ok_or_else(unsupported_layout)?;
            let last_end = rule_extent(&last.value)?.end;
            edits.push(TextEdit {
                range: rules_line.start..line_end_offset(text, last_end),
                replacement: String::new(),
            });
        } else {
            let sort_line = layout.sort_line.ok_or_else(unsupported_layout)?;
            let sort_end = sort_block_end(sort)?;
            edits
                .push(TextEdit { range: sort_line.start..line_end_offset(text, sort_end), replacement: String::new() });
        }
        return Ok(());
    }

    if managed_idx == Some(0) && !patterns.is_empty() {
        return plan_rule_sequence_edits(text, &rules.value[0].value, cluster, &patterns, edits);
    }

    // A removed managed rule leaves the user rules where they are; nothing is inserted for an
    // empty order.
    if let Some(index) = managed_idx {
        let rule_lines = block_lines(text, &rule_extent(&rules.value[index].value)?);
        edits.push(TextEdit { range: rule_lines, replacement: String::new() });
    }
    if patterns.is_empty() {
        return Ok(());
    }
    let first_extent = rule_extent(&rules.value[0].value)?;
    let dash_indent = line_indent_at(text, line_start(text, first_extent.start));
    let list_start = line_start(text, first_extent.start);
    edits.push(TextEdit {
        range: list_start..list_start,
        replacement: format!("{}{newline}", block_rule_lines(cluster, &patterns, dash_indent, newline)?),
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Text-edit planning (translates semantic diff into byte edits)
// ---------------------------------------------------------------------------

fn plan_text_edits(
    original_text: &str,
    before: &SourcesConfigDto,
    expected: &SourcesConfigDto,
    patches: &[SourcesYmlPatch],
) -> Result<Vec<TextEdit>, TargetGroupOrderPatchError> {
    let doc = parse_patch_document(original_text)?;
    let mut edits: Vec<TextEdit> = Vec::new();

    for (before_input, expected_input) in before.inputs.iter().zip(expected.inputs.iter()) {
        let input_name = &expected_input.name;

        // Scalar field diffs on the root input
        plan_scalar_field_edits(original_text, &doc, input_name, before_input, expected_input, &mut edits)?;

        // Alias structural changes
        let before_aliases = before_input.aliases.as_deref().unwrap_or_default();
        let expected_aliases = expected_input.aliases.as_deref().unwrap_or_default();

        if before_aliases.len() != expected_aliases.len()
            || before_aliases.iter().zip(expected_aliases.iter()).any(|(a, b)| a.name != b.name)
        {
            // Structural change: aliases were added, removed, or reordered
            plan_alias_structural_edits(original_text, &doc, input_name, before_aliases, expected_aliases, &mut edits)?;
        } else {
            // Same aliases in same order — check for scalar diffs within each alias
            for (alias_idx, (before_alias, expected_alias)) in
                before_aliases.iter().zip(expected_aliases.iter()).enumerate()
            {
                plan_alias_scalar_edits(
                    original_text,
                    &doc,
                    input_name,
                    alias_idx,
                    before_alias,
                    expected_alias,
                    &mut edits,
                )?;
            }
        }
    }

    let group_order_patches = patches
        .iter()
        .filter_map(|patch| match patch {
            SourcesYmlPatch::SetTargetGroupOrder { target_name, cluster, groups } => {
                Some((target_name, *cluster, groups))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if !group_order_patches.is_empty() {
        let target_doc = parse_target_sort_document(original_text)?;
        for (target_name, cluster, groups) in group_order_patches {
            plan_target_group_order_edits(original_text, &target_doc, target_name, cluster, groups, &mut edits)?;
        }
    }

    Ok(edits)
}

/// Byte span of an optional projected scalar.
fn opt_span<T>(spanned: Option<&serde_saphyr::Spanned<T>>) -> Result<Option<Range<usize>>, TuliproxError> {
    spanned.map(span_byte_range).transpose()
}

/// Plans a single scalar field change.
///
/// An existing field is replaced in place through its exact span. A missing field is inserted
/// below the first anchor that exists, so key order stays deterministic and no other byte moves.
fn plan_field(
    text: &str,
    existing: Option<Range<usize>>,
    anchors: &[Option<Range<usize>>],
    key: &str,
    value: &str,
    edits: &mut Vec<TextEdit>,
) -> Result<(), TuliproxError> {
    if let Some(range) = existing {
        edits.push(TextEdit { range, replacement: value.to_string() });
        return Ok(());
    }

    let anchor = anchors.iter().flatten().next().ok_or_else(|| {
        TuliproxError::Config(format!("source.yml patch: no insertion anchor found for field '{key}'"))
    })?;
    edits.push(build_field_insertion_edit(text, anchor, key, value)?);
    Ok(())
}

/// Plans the scalar edits shared by root inputs and aliases.
///
/// `name_span` anchors every field that has no better sibling to attach to.
struct AccountFieldSpans {
    name: Range<usize>,
    enabled: Option<Range<usize>>,
    url: Option<Range<usize>>,
    username: Option<Range<usize>>,
    password: Option<Range<usize>>,
    exp_date: Option<Range<usize>>,
    max_connections: Option<Range<usize>>,
}

impl AccountFieldSpans {
    fn from_input(value: &crate::config_loader::source_patch::PatchInput) -> Result<Self, TuliproxError> {
        Ok(Self {
            name: span_byte_range(&value.name)?,
            enabled: opt_span(value.enabled.as_ref())?,
            url: opt_span(value.url.as_ref())?,
            username: opt_span(value.username.as_ref())?,
            password: opt_span(value.password.as_ref())?,
            exp_date: opt_span(value.exp_date.as_ref())?,
            max_connections: opt_span(value.max_connections.as_ref())?,
        })
    }

    fn from_alias(value: &crate::config_loader::source_patch::PatchAlias) -> Result<Self, TuliproxError> {
        Ok(Self {
            name: span_byte_range(&value.name)?,
            enabled: opt_span(value.enabled.as_ref())?,
            url: opt_span(value.url.as_ref())?,
            username: opt_span(value.username.as_ref())?,
            password: opt_span(value.password.as_ref())?,
            exp_date: opt_span(value.exp_date.as_ref())?,
            max_connections: opt_span(value.max_connections.as_ref())?,
        })
    }

    fn name_anchor(&self) -> Range<usize> { self.name.clone() }
}

/// Values that changed between `before` and `expected` for one account.
struct AccountFieldChanges<'a> {
    enabled: Option<bool>,
    url: Option<&'a str>,
    username: Option<&'a str>,
    password: Option<&'a str>,
    exp_date: Option<i64>,
    max_connections: Option<u16>,
}

/// Plans every scalar edit of one account (root input or alias) from its spans and changes.
fn plan_account_scalar_edits(
    text: &str,
    spans: &AccountFieldSpans,
    changes: &AccountFieldChanges<'_>,
    edits: &mut Vec<TextEdit>,
) -> Result<(), TuliproxError> {
    if let Some(exp_date) = changes.exp_date {
        let value = serialize_yaml_scalar(&exp_date)?;
        let anchors = [spans.password.clone(), spans.username.clone(), Some(spans.name_anchor())];
        plan_field(text, spans.exp_date.clone(), &anchors, "exp_date", &value, edits)?;
    }

    if let Some(enabled) = changes.enabled {
        // A missing `enabled` already means `true`, so only the disabling case needs an insertion.
        if spans.enabled.is_some() || !enabled {
            let value = serialize_yaml_scalar(&enabled)?;
            let anchors = [Some(spans.name_anchor())];
            plan_field(text, spans.enabled.clone(), &anchors, "enabled", &value, edits)?;
        }
    }

    if let Some(max_connections) = changes.max_connections {
        // A missing `max_connections` already means `0`, so only a real limit needs an insertion.
        if spans.max_connections.is_some() || max_connections != 0 {
            let value = serialize_yaml_scalar(&max_connections)?;
            let anchors =
                [spans.exp_date.clone(), spans.password.clone(), spans.username.clone(), Some(spans.name_anchor())];
            plan_field(text, spans.max_connections.clone(), &anchors, "max_connections", &value, edits)?;
        }
    }

    if let Some(username) = changes.username {
        let value = serialize_yaml_scalar(&username)?;
        let anchors = [spans.url.clone(), Some(spans.name_anchor())];
        plan_field(text, spans.username.clone(), &anchors, "username", &value, edits)?;
    }

    if let Some(password) = changes.password {
        let value = serialize_yaml_scalar(&password)?;
        let anchors = [spans.username.clone(), spans.url.clone(), Some(spans.name_anchor())];
        plan_field(text, spans.password.clone(), &anchors, "password", &value, edits)?;
    }

    // `url` is mandatory in every account, so it is only ever replaced.
    if let Some(url) = changes.url {
        if let Some(range) = spans.url.clone() {
            edits.push(TextEdit { range, replacement: serialize_yaml_scalar(&url)? });
        }
    }

    Ok(())
}

fn plan_scalar_field_edits(
    text: &str,
    doc: &crate::config_loader::source_patch::SourcePatchDocument,
    input_name: &Arc<str>,
    before: &ConfigInputDto,
    expected: &ConfigInputDto,
    edits: &mut Vec<TextEdit>,
) -> Result<(), TuliproxError> {
    let patch_input = find_input(doc, input_name.as_ref())?;
    let value = &patch_input.value;

    let spans = AccountFieldSpans::from_input(value)?;
    let changes = AccountFieldChanges {
        enabled: (before.enabled != expected.enabled).then_some(expected.enabled),
        url: (before.url != expected.url).then_some(expected.url.as_str()),
        username: changed_credential(before.username.as_ref(), expected.username.as_ref()),
        password: changed_credential(before.password.as_ref(), expected.password.as_ref()),
        exp_date: (before.exp_date != expected.exp_date).then_some(expected.exp_date).flatten(),
        max_connections: (before.max_connections != expected.max_connections).then_some(expected.max_connections),
    };
    plan_account_scalar_edits(text, &spans, &changes, edits)?;

    if before.account_disabled != expected.account_disabled {
        plan_field(
            text,
            opt_span(value.account_disabled.as_ref())?,
            &[Some(spans.name.clone())],
            "account_disabled",
            if expected.account_disabled { "true" } else { "false" },
            edits,
        )?;
    }
    plan_panel_api_credits_edit(text, value, before, expected, edits)
}

/// Returns the new credential when it changed to a concrete value.
///
/// Clearing a credential is not expressible as a scalar edit, so it is left to the semantic
/// verification step to reject such a patch instead of silently dropping the key.
fn changed_credential<'a>(before: Option<&String>, expected: Option<&'a String>) -> Option<&'a str> {
    match expected {
        Some(value) if before.map(String::as_str) != Some(value.as_str()) => Some(value.as_str()),
        _ => None,
    }
}

fn plan_panel_api_credits_edit(
    text: &str,
    value: &crate::config_loader::source_patch::PatchInput,
    before: &ConfigInputDto,
    expected: &ConfigInputDto,
    edits: &mut Vec<TextEdit>,
) -> Result<(), TuliproxError> {
    let (Some(before_panel), Some(expected_panel)) = (&before.panel_api, &expected.panel_api) else {
        return Ok(());
    };
    if before_panel.credits == expected_panel.credits {
        return Ok(());
    }
    let Some(expected_credits) = &expected_panel.credits else {
        return Ok(());
    };
    let Some(panel_spanned) = &value.panel_api else {
        return Ok(());
    };

    let new_val = serialize_yaml_scalar(expected_credits)?;
    let existing = opt_span(panel_spanned.value.credits.as_ref())?;
    // Without an existing `credits` key the first child of the `panel_api` mapping is the anchor.
    let mapping_anchor = Some(span_byte_range(panel_spanned)?);
    plan_field(text, existing, &[mapping_anchor], "credits", &new_val, edits)
}

fn plan_alias_scalar_edits(
    text: &str,
    doc: &crate::config_loader::source_patch::SourcePatchDocument,
    input_name: &Arc<str>,
    alias_idx: usize,
    before: &ConfigInputAliasDto,
    expected: &ConfigInputAliasDto,
    edits: &mut Vec<TextEdit>,
) -> Result<(), TuliproxError> {
    let patch_input = find_input(doc, input_name.as_ref())?;
    let Some(aliases_spanned) = &patch_input.value.aliases else {
        return Ok(());
    };
    let Some(alias_spanned) = aliases_spanned.value.get(alias_idx) else {
        return Ok(());
    };

    let spans = AccountFieldSpans::from_alias(&alias_spanned.value)?;
    let changes = AccountFieldChanges {
        enabled: (before.enabled != expected.enabled).then_some(expected.enabled),
        url: (before.url != expected.url).then_some(expected.url.as_str()),
        username: changed_credential(before.username.as_ref(), expected.username.as_ref()),
        password: changed_credential(before.password.as_ref(), expected.password.as_ref()),
        exp_date: (before.exp_date != expected.exp_date).then_some(expected.exp_date).flatten(),
        max_connections: (before.max_connections != expected.max_connections).then_some(expected.max_connections),
    };
    plan_account_scalar_edits(text, &spans, &changes, edits)
}

fn plan_alias_structural_edits(
    text: &str,
    doc: &crate::config_loader::source_patch::SourcePatchDocument,
    input_name: &Arc<str>,
    before_aliases: &[ConfigInputAliasDto],
    expected_aliases: &[ConfigInputAliasDto],
    edits: &mut Vec<TextEdit>,
) -> Result<(), TuliproxError> {
    let before_names: Vec<&str> = before_aliases.iter().map(|a| a.name.as_ref()).collect();
    let expected_names: Vec<&str> = expected_aliases.iter().map(|a| a.name.as_ref()).collect();
    let before_set: HashSet<&str> = before_names.iter().copied().collect();
    let expected_set: HashSet<&str> = expected_names.iter().copied().collect();

    let expected_existing_order: Vec<&str> =
        expected_names.iter().copied().filter(|name| before_set.contains(name)).collect();
    let surviving_before_order: Vec<&str> =
        before_names.iter().copied().filter(|name| expected_set.contains(name)).collect();
    let added: Vec<&ConfigInputAliasDto> =
        expected_aliases.iter().filter(|alias| !before_set.contains(alias.name.as_ref())).collect();
    let appended_without_reordering = expected_existing_order == surviving_before_order
        && expected_names
            .get(expected_names.len().saturating_sub(added.len())..)
            .is_some_and(|suffix| suffix.iter().copied().eq(added.iter().map(|alias| alias.name.as_ref())));

    // A command such as PersistProvisionedAccount can add and sort in one semantic operation.
    // Rebuild the sequence from opaque existing blocks so the final order is represented by one
    // non-overlapping edit. Sequential command batches use this same path one command at a time.
    if !appended_without_reordering {
        if let Some(edit) = build_alias_sequence_edit(text, doc, input_name.as_ref(), expected_aliases)? {
            edits.push(edit);
            return Ok(());
        }
    }

    // Removed aliases
    let removed: Vec<&str> = before_names.iter().filter(|name| !expected_set.contains(**name)).copied().collect();
    if !removed.is_empty() {
        let removal_edits = build_alias_removal_edits(text, doc, input_name.as_ref(), &removed)?;
        edits.extend(removal_edits);
    }

    // Added aliases
    for expected_alias in added {
        let add_edit = build_alias_addition_edit(text, doc, input_name.as_ref(), expected_alias)?;
        edits.push(add_edit);
    }

    // Sort: if same set of names but different order
    if removed.is_empty() && before_aliases.len() == expected_aliases.len() {
        let mut sorted_before = before_names.clone();
        sorted_before.sort_unstable();
        let mut sorted_expected = expected_names.clone();
        sorted_expected.sort_unstable();
        if sorted_before == sorted_expected && before_names != expected_names {
            if let Some(sort_edit) = build_alias_sort_edit(text, doc, input_name.as_ref(), &expected_names)? {
                edits.push(sort_edit);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Central transaction
// ---------------------------------------------------------------------------

fn validate_group_order_layouts(text: &str, patches: &[SourcesYmlPatch]) -> Result<(), TargetGroupOrderPatchError> {
    let target_names: Vec<&str> = patches
        .iter()
        .filter_map(|patch| match patch {
            SourcesYmlPatch::SetTargetGroupOrder { target_name, .. } => Some(target_name.as_ref()),
            _ => None,
        })
        .collect();
    if target_names.is_empty() {
        return Ok(());
    }
    let target_doc = parse_target_sort_document(text)?;
    for target_name in target_names {
        let target = find_patch_target(&target_doc, target_name)?;
        validate_target_sort_layout(text, &target.value)?;
    }
    Ok(())
}

fn apply_patch_planning_step(
    expected: &mut SourcesConfigDto,
    patched_text: &mut String,
    patches: &[SourcesYmlPatch],
) -> Result<bool, TargetGroupOrderPatchError> {
    let mut next_expected = expected.clone();
    if !apply_sources_yml_patches_inner(&mut next_expected, patches)? {
        validate_group_order_layouts(patched_text, patches)?;
        return Ok(false);
    }
    let text_edits = plan_text_edits(patched_text, expected, &next_expected, patches)?;
    let next_text = apply_scalar_edits(patched_text, text_edits)?;
    parse_and_validate_patched_text(&next_text, &next_expected)
        .map_err(|err| TargetGroupOrderPatchError::ValidationFailed(err.to_string()))?;
    *expected = next_expected;
    *patched_text = next_text;
    Ok(true)
}

/// Builds the account fields needed for patches without rebuilding prepared inputs.
pub(crate) fn runtime_patch_document(sources: &crate::model::SourcesConfig) -> SourcesConfigDto {
    SourcesConfigDto {
        inputs: sources
            .inputs
            .iter()
            .map(|input| shared::model::ConfigInputDto {
                id: input.id,
                name: Arc::clone(&input.name),
                input_type: input.input_type,
                url: input.url.clone(),
                username: input.username.clone(),
                password: input.password.clone(),
                priority: input.priority,
                enabled: input.enabled,
                account_disabled: input.account_disabled,
                max_connections: input.max_connections,
                exp_date: input.exp_date,
                panel_api: input.panel_api.as_ref().map(shared::model::PanelApiConfigDto::from),
                aliases: input.aliases.as_ref().map(|aliases| {
                    aliases
                        .iter()
                        .map(|alias| shared::model::ConfigInputAliasDto {
                            id: alias.id,
                            name: Arc::clone(&alias.name),
                            url: alias.url.clone(),
                            username: alias.username.clone(),
                            password: alias.password.clone(),
                            enabled: alias.enabled,
                            max_connections: alias.max_connections,
                            priority: alias.priority,
                            exp_date: alias.exp_date,
                            stalker: None,
                        })
                        .collect()
                }),
                ..shared::model::ConfigInputDto::default()
            })
            .collect(),
        ..SourcesConfigDto::default()
    }
}

pub(crate) fn merge_runtime_document(
    current: &crate::model::SourcesConfig,
    doc: SourcesConfigDto,
) -> Result<crate::model::SourcesConfig, TuliproxError> {
    let mut next = current.clone();
    let mut next_id = next
        .inputs
        .iter()
        .flat_map(|input| std::iter::once(input.id).chain(input.aliases.iter().flatten().map(|alias| alias.id)))
        .max()
        .unwrap_or(0);
    for (input, updated) in next.inputs.iter_mut().zip(doc.inputs) {
        let input = Arc::make_mut(input);
        let root_changed =
            input.url != updated.url || input.username != updated.username || input.password != updated.password;
        input.url = updated.url;
        input.username = updated.username;
        input.password = updated.password;
        if let Some(stalker) = input.stalker.as_mut() {
            stalker.username.clone_from(&input.username);
            stalker.password.clone_from(&input.password);
        }
        input.exp_date = updated.exp_date;
        input.enabled = updated.enabled;
        input.account_disabled = updated.account_disabled;
        input.max_connections = updated.max_connections;
        input.priority = updated.priority;
        input.panel_api = updated.panel_api.as_ref().map(crate::model::PanelApiConfig::from);

        if let Some(aliases) = updated.aliases {
            let previous = input.aliases.take().unwrap_or_default();
            let mut converted = Vec::with_capacity(aliases.len());
            for alias in aliases {
                let mut runtime = crate::model::ConfigInputAlias::from(&alias);
                if let Some(old) = previous.iter().find(|old| old.name == alias.name) {
                    runtime.id = old.id;
                    runtime.stalker.clone_from(&old.stalker);
                    if let Some(stalker) = runtime.stalker.as_mut() {
                        stalker.username.clone_from(&runtime.username);
                        stalker.password.clone_from(&runtime.password);
                    }
                } else {
                    next_id = next_id
                        .checked_add(1)
                        .ok_or_else(|| TuliproxError::ConfigInput("Provider account ID space exhausted".to_string()))?;
                    runtime.id = next_id;
                }
                converted.push(runtime);
            }
            input.aliases = Some(converted);
        }
        if root_changed {
            input.refresh_auto_epg_sources()?;
        }
    }
    next.group_lookup = shared::model::provider_saturation::build_group_lookup(&next.inputs);
    Ok(next)
}

pub(crate) fn apply_runtime_source_patches(
    app_config: &AppConfig,
    patches: &[SourcesYmlPatch],
) -> Result<(), TuliproxError> {
    let account_patches: Vec<SourcesYmlPatch> =
        patches.iter().filter(|patch| !patch.is_target_group_order()).cloned().collect();
    if account_patches.is_empty() {
        return Ok(());
    }
    loop {
        let current = app_config.sources.load_full();
        let mut doc = runtime_patch_document(&current);
        if !apply_sources_yml_patches(&mut doc, &account_patches)? {
            return Ok(());
        }
        let next = merge_runtime_document(&current, doc)?;
        let previous = app_config.sources.compare_and_swap(&current, Arc::new(next));
        if Arc::ptr_eq(&previous, &current) {
            return Ok(());
        }
    }
}

async fn load_templates_for_sort_prepare(
    app_config: &AppConfig,
    sources_path: &Path,
    resolved_expected: &SourcesConfigDto,
) -> Result<Option<Vec<shared::model::PatternTemplate>>, TuliproxError> {
    let mut paths = (*app_config.paths.load_full()).clone();
    if paths.config_path.trim().is_empty() && paths.template_file_path.is_none() && paths.mapping_file_path.is_none() {
        return resolved_expected
            .templates
            .as_ref()
            .map(|templates| {
                let mut cloned = templates.clone();
                shared::foundation::prepare_templates(&mut cloned)
            })
            .transpose();
    }
    if paths.sources_file_path.trim().is_empty() {
        paths.sources_file_path = sources_path.to_string_lossy().into_owned();
    }
    let config = app_config.config.load();
    crate::config_loader::load_prepared_global_templates(&paths, &config).await
}

/// Parses patched `source.yml` text with `${env:VAR}` interpolation matching config reload.
fn parse_sources_with_env(text: &str) -> Result<SourcesConfigDto, TargetGroupOrderPatchError> {
    let reader = crate::utils::file_reader(std::io::Cursor::new(text.as_bytes()));
    shared::utils::yaml_from_reader(crate::utils::EnvResolvingReader::new(reader)).map_err(|err| {
        TargetGroupOrderPatchError::ValidationFailed(format!("source.yml patch: failed to parse source.yml: {err}"))
    })
}

/// Resolves env vars on the patched document and prepares the runtime `ConfigSort` for every
/// target touched by `SetTargetGroupOrder` before anything is written to disk.
async fn prepare_target_runtime_sorts(
    app_config: &AppConfig,
    sources_path: &Path,
    patched_text: &str,
    patches: &[SourcesYmlPatch],
) -> Result<Option<HashMap<String, Option<ConfigSort>>>, TargetGroupOrderPatchError> {
    let target_names: HashSet<&str> = patches
        .iter()
        .filter_map(|patch| match patch {
            SourcesYmlPatch::SetTargetGroupOrder { target_name, .. } => Some(target_name.as_ref()),
            _ => None,
        })
        .collect();
    if target_names.is_empty() {
        return Ok(None);
    }

    let resolved_dto = parse_sources_with_env(patched_text)?;
    let prepared_templates = load_templates_for_sort_prepare(app_config, sources_path, &resolved_dto)
        .await
        .map_err(|err| TargetGroupOrderPatchError::ValidationFailed(err.to_string()))?;
    let mut rebuilt: HashMap<String, Option<ConfigSort>> = HashMap::with_capacity(target_names.len());
    for source in &resolved_dto.sources {
        for target in &source.targets {
            if !target_names.contains(target.name.as_str()) {
                continue;
            }
            let prepared_sort = match target.sort.as_ref() {
                Some(sort_dto) => {
                    let mut sort = sort_dto.clone();
                    sort.prepare(prepared_templates.as_deref())
                        .map_err(|err| TargetGroupOrderPatchError::ValidationFailed(err.to_string()))?;
                    Some(ConfigSort::from(&sort))
                }
                None => None,
            };
            rebuilt.insert(target.name.clone(), prepared_sort);
        }
    }
    Ok(Some(rebuilt))
}

/// Atomically swaps prepared target sorts into the live `AppConfig`.
fn apply_target_runtime_sorts(app_config: &AppConfig, rebuilt: &HashMap<String, Option<ConfigSort>>) {
    loop {
        let current = app_config.sources.load_full();
        let mut next = (*current).clone();
        let mut updated = false;
        for source in &mut next.sources {
            for target in &mut source.targets {
                if let Some(new_sort) = rebuilt.get(target.name.as_str()) {
                    Arc::make_mut(target).sort.clone_from(new_sort);
                    updated = true;
                }
            }
        }
        if !updated {
            return;
        }
        let previous = app_config.sources.compare_and_swap(&current, Arc::new(next));
        if Arc::ptr_eq(&previous, &current) {
            return;
        }
    }
}

/// Executes account patches (renewals, credential updates) and resets the renewed accounts'
/// health while the file lock is still held, so a queued observation cannot write a stale ban
/// back between the write and the reset. Refreshes the provider lineup afterwards.
pub(crate) async fn execute_account_source_patches(
    app_state: &crate::api::model::AppState,
    sources_path: &Path,
    patches: &[SourcesYmlPatch],
) -> Result<bool, TuliproxError> {
    if patches.is_empty() {
        return Ok(false);
    }
    let guard = app_state.app_config.file_locks.write_lock(sources_path).await;
    let written = execute_source_yml_patches_locked(&app_state.app_config, sources_path, patches, &guard).await?;
    for patch in patches {
        let renewed = match patch {
            SourcesYmlPatch::UpdatePanelAccountExpiry { account_name, .. } => account_name,
            SourcesYmlPatch::UpdateRootCredentials { input_name, .. } => input_name,
            SourcesYmlPatch::UpdateAliasCredentials { alias_name, .. } => alias_name,
            _ => continue,
        };
        app_state.active_provider.reset_account_health(renewed);
    }
    drop(guard);
    app_state.active_provider.update_config(&app_state.app_config);
    Ok(written)
}

/// Executes patches under the source file's write lock and records the written revision.
#[cfg(test)]
pub(crate) async fn execute_source_yml_patches(
    app_config: &Arc<AppConfig>,
    sources_path: &Path,
    patches: &[SourcesYmlPatch],
) -> Result<bool, TuliproxError> {
    if patches.is_empty() {
        return Ok(false);
    }

    let guard = app_config.file_locks.write_lock(sources_path).await;
    execute_source_yml_patches_locked(app_config, sources_path, patches, &guard).await
}

pub(crate) async fn execute_source_yml_patches_locked(
    app_config: &Arc<AppConfig>,
    sources_path: &Path,
    patches: &[SourcesYmlPatch],
    guard: &crate::utils::FileWriteGuard,
) -> Result<bool, TuliproxError> {
    execute_source_yml_patches_inner(app_config, sources_path, patches, guard)
        .await
        .map_err(TargetGroupOrderPatchError::into_tuliprox_error)
}

pub(crate) async fn execute_target_group_order_patch_locked(
    app_config: &Arc<AppConfig>,
    sources_path: &Path,
    patch: SourcesYmlPatch,
    guard: &crate::utils::FileWriteGuard,
) -> Result<bool, TargetGroupOrderPatchError> {
    execute_source_yml_patches_inner(app_config, sources_path, std::slice::from_ref(&patch), guard).await
}

async fn classify_write_failure(
    sources_path: &Path,
    original_revision: blake3::Hash,
    err: TuliproxError,
) -> TargetGroupOrderPatchError {
    if let Ok(bytes) = tokio::fs::read(sources_path).await {
        if blake3::hash(&bytes) != original_revision {
            return TargetGroupOrderPatchError::RevisionChanged;
        }
    }
    TargetGroupOrderPatchError::Other(err)
}

async fn execute_source_yml_patches_inner(
    app_config: &Arc<AppConfig>,
    sources_path: &Path,
    patches: &[SourcesYmlPatch],
    _guard: &crate::utils::FileWriteGuard,
) -> Result<bool, TargetGroupOrderPatchError> {
    // Read and fingerprint the exact bytes that the transaction is based on.
    let original_bytes = tokio::fs::read(sources_path)
        .await
        .map_err(|err| TuliproxError::ConfigPanelApi(format!("source.yml patch: failed to read file: {err}")))?;
    let original_revision = blake3::hash(&original_bytes);
    let synchronized = app_config.file_locks.is_internal_revision_hash(sources_path, &original_revision).await;
    let original_text = String::from_utf8(original_bytes)
        .map_err(|_| TuliproxError::ConfigPanelApi("source.yml patch: file is not valid UTF-8".to_string()))?;

    // Validate the current document before applying any edits.
    let before: SourcesConfigDto = shared::utils::yaml_from_str(&original_text).map_err(|err| {
        TargetGroupOrderPatchError::ValidationFailed(format!("source.yml patch: failed to parse source.yml: {err}"))
    })?;

    // Batch consecutive scalar commands into one clone, parse, and validation pass.
    // Structural commands remain sequential because they change the byte spans used by
    // subsequent edits. The complete command list still produces one final atomic disk write.
    let mut expected = before;
    let mut patched_text = original_text;
    let mut changed = false;
    let mut patch_index = 0;
    while patch_index < patches.len() {
        let step_end = if patches[patch_index].changes_yaml_structure() {
            patch_index + 1
        } else {
            patches[patch_index..]
                .iter()
                .position(SourcesYmlPatch::changes_yaml_structure)
                .map_or(patches.len(), |offset| patch_index + offset)
        };
        changed |= apply_patch_planning_step(&mut expected, &mut patched_text, &patches[patch_index..step_end])?;
        patch_index = step_end;
    }

    if !changed {
        apply_runtime_source_patches(app_config, patches)?;
        return Ok(false);
    }

    let prepared_sorts = prepare_target_runtime_sorts(app_config, sources_path, &patched_text, patches).await?;

    let backup_dir = app_config.config.load().get_backup_dir().to_string();

    // Back up and atomically replace the file if its revision is still current.
    let written = match write_config_text_file(
        sources_path.to_string_lossy().as_ref(),
        &backup_dir,
        &patched_text,
        "source.yml",
        Some(original_revision),
    )
    .await
    {
        Ok(written) => written,
        Err(err) => return Err(classify_write_failure(sources_path, original_revision, err).await),
    };

    if written {
        if synchronized {
            app_config.file_locks.mark_internal_write_content(sources_path, patched_text.as_bytes()).await;
        }
        apply_runtime_source_patches(app_config, patches)?;
        if let Some(rebuilt) = prepared_sorts.as_ref() {
            apply_target_runtime_sorts(app_config, rebuilt);
        }
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_loader::source_patch::TextEdit;
    use shared::model::PanelApiConfigDto;

    fn edits_to_text(text: &str, edits: Vec<TextEdit>) -> String {
        crate::config_loader::source_patch::apply_scalar_edits(text, edits).expect("apply")
    }

    fn find_edit<'a>(edits: &'a [TextEdit], needle: &str) -> Option<&'a TextEdit> {
        edits.iter().find(|e| e.replacement.contains(needle))
    }

    #[test]
    fn insertion_does_not_swallow_the_next_line() {
        // Regression: the old code inserted after `line_end_offset`, which pointed past the newline,
        // so the replacement concatenated into the following line. With a non-empty line after the
        // anchor, the bug surfaces as `pass<inserted>` with no separator.
        let fixture = concat!(
            "inputs:\n",
            "  - name: provider\n",
            "    url: http://main.example\n",
            "    username: user\n",
            "    password: pass\n",
            "    aliases:\n",
            "      - name: alias-a\n",
            "        url: http://a.example\n",
        );
        let doc: crate::config_loader::source_patch::SourcePatchDocument =
            serde_saphyr::from_str(fixture).expect("parse");

        let before = ConfigInputDto {
            name: "provider".into(),
            url: "http://main.example".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            ..Default::default()
        };
        let mut expected = before.clone();
        expected.exp_date = Some(1_700_000_000);

        let mut edits = Vec::new();
        plan_scalar_field_edits(fixture, &doc, &Arc::from("provider"), &before, &expected, &mut edits).expect("plan");

        let patched_text = edits_to_text(fixture, edits);
        for line in fixture.lines() {
            assert!(patched_text.contains(line), "line lost after patch: {line}");
        }
        assert!(patched_text.contains("exp_date: 1700000000"));
        // The exact bug signature: `pass\n    exp_date: ...` glued with no separator.
        assert!(
            !patched_text.contains("pass1700000000") && !patched_text.contains("pass exp_date"),
            "next line must not be glued to the inserted value"
        );
    }

    #[test]
    fn enabling_then_disabling_yields_two_scalar_edits_without_insertions() {
        // `enabled` is missing on the alias below; the patcher must replace or insert, never duplicate.
        let fixture = concat!(
            "inputs:\n",
            "  - name: provider\n",
            "    url: http://main.example\n",
            "    aliases:\n",
            "      - name: alias-a\n",
            "        url: http://a.example\n",
        );
        let doc: crate::config_loader::source_patch::SourcePatchDocument =
            serde_saphyr::from_str(fixture).expect("parse");

        let before = ConfigInputAliasDto {
            name: "alias-a".into(),
            url: "http://a.example".to_string(),
            enabled: true,
            ..Default::default()
        };
        let mut expected = before.clone();
        expected.enabled = false;

        let mut edits = Vec::new();
        plan_alias_scalar_edits(fixture, &doc, &Arc::from("provider"), 0, &before, &expected, &mut edits)
            .expect("plan");

        assert_eq!(edits.len(), 1, "exactly one replacement edit, no insertion");
        let patched_text = edits_to_text(fixture, edits);
        assert!(patched_text.contains("enabled: false"));
        assert_eq!(patched_text.matches("enabled:").count(), 1, "no duplicate `enabled:` key");
    }

    #[test]
    fn root_panel_credits_inserts_into_block_mapping() {
        let fixture = concat!(
            "inputs:\n",
            "  - name: provider\n",
            "    url: http://main.example\n",
            "    panel_api:\n",
            "      url: http://panel.example\n",
        );
        let doc: crate::config_loader::source_patch::SourcePatchDocument =
            serde_saphyr::from_str(fixture).expect("parse");

        let before = ConfigInputDto {
            name: "provider".into(),
            url: "http://main.example".to_string(),
            panel_api: Some(PanelApiConfigDto { url: "http://panel.example".to_string(), ..Default::default() }),
            ..Default::default()
        };
        let mut expected = before.clone();
        expected.panel_api.as_mut().expect("panel").credits = Some("42".to_string());

        let mut edits = Vec::new();
        plan_scalar_field_edits(fixture, &doc, &Arc::from("provider"), &before, &expected, &mut edits).expect("plan");

        assert!(find_edit(&edits, "credits").is_some(), "credits edit planned");
        let patched_text = edits_to_text(fixture, edits);
        let reparsed: crate::config_loader::source_patch::SourcePatchDocument =
            serde_saphyr::from_str(&patched_text).expect("reparse");
        let panel = reparsed.inputs[0].value.panel_api.as_ref().expect("panel");
        assert_eq!(panel.value.credits.as_ref().expect("credits").value, "42");
    }

    #[test]
    fn flow_style_insertion_adds_a_mapping_entry() {
        let fixture = "inputs:\n  - { name: provider, url: http://main.example }\n";
        let doc: crate::config_loader::source_patch::SourcePatchDocument =
            serde_saphyr::from_str(fixture).expect("parse");
        let input = &doc.inputs[0].value;
        let anchor = span_byte_range(&input.name).expect("name span");

        let edit = build_field_insertion_edit(fixture, &anchor, "enabled", "false").expect("build edit");
        let patched_text = edits_to_text(fixture, vec![edit]);
        let reparsed: crate::config_loader::source_patch::SourcePatchDocument =
            serde_saphyr::from_str(&patched_text).expect("reparse");

        assert!(!reparsed.inputs[0].value.enabled.as_ref().expect("enabled").value);
        assert!(patched_text.contains("name: provider, enabled: false"));
    }

    // -----------------------------------------------------------------------
    // End-to-end: the central transaction against a real on-disk source.yml.
    // -----------------------------------------------------------------------

    mod e2e {
        use super::*;
        use arc_swap::ArcSwap;
        use shared::{
            model::{ConfigPaths, InputType, SourcesConfigDto},
            utils::Internable,
        };
        use std::time::{SystemTime, UNIX_EPOCH};
        use tuliprox_core::{
            model::{Config, ConfigInput, MediaToolCapabilities, SourcesConfig},
            utils::FileLockManager,
        };
        use tuliprox_repository::AliasExpDateSortOrder;

        fn build_app_config(backup_dir: &std::path::Path) -> Arc<AppConfig> {
            let input = ConfigInput {
                id: 1,
                name: "provider".intern(),
                input_type: InputType::Xtream,
                url: "http://main.example".to_string(),
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                enabled: true,
                priority: 0,
                max_connections: 0,
                aliases: None,
                ..ConfigInput::default()
            };
            let sources = SourcesConfig { inputs: vec![Arc::new(input)], ..SourcesConfig::default() };
            Arc::new(AppConfig {
                config: Arc::new(ArcSwap::from_pointee(Config {
                    backup_dir: Some(backup_dir.to_string_lossy().into_owned()),
                    ..Config::default()
                })),
                sources: Arc::new(ArcSwap::from_pointee(sources)),
                hdhomerun: Arc::new(arc_swap::ArcSwapOption::default()),
                plugins: Arc::default(),
                api_proxy: Arc::new(arc_swap::ArcSwapOption::default()),
                file_locks: Arc::new(FileLockManager::default()),
                paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                    home_path: String::new(),
                    config_path: String::new(),
                    storage_path: String::new(),
                    config_file_path: String::new(),
                    sources_file_path: String::new(),
                    mapping_file_path: None,
                    mapping_files_used: None,
                    template_file_path: None,
                    template_files_used: None,
                    failover_file_path: None,
                    api_proxy_file_path: String::new(),
                    custom_stream_response_path: None,
                })),
                custom_stream_response: Arc::new(arc_swap::ArcSwapOption::default()),
                access_token_secret: [0; 32],
                encrypt_secret: [0; 16],
                media_tools: Arc::new(MediaToolCapabilities::new()),
                failover: Arc::default(),
            })
        }

        async fn execute_fixture_patches(
            app_config: &Arc<AppConfig>,
            source_path: &std::path::Path,
            patches: &[SourcesYmlPatch],
        ) -> Result<bool, TuliproxError> {
            let text =
                tokio::fs::read_to_string(source_path).await.map_err(|err| TuliproxError::Io(err.to_string()))?;
            let dto: SourcesConfigDto =
                serde_saphyr::from_str(&text).map_err(|err| TuliproxError::ConfigInput(err.to_string()))?;
            let mut sources = (*app_config.sources.load_full()).clone();
            sources.inputs = dto.inputs.iter().map(|input| Arc::new(ConfigInput::from(input))).collect();
            app_config.sources.store(Arc::new(sources));
            app_config.file_locks.mark_internal_write_content(source_path, text.as_bytes()).await;
            execute_source_yml_patches(app_config, source_path, patches).await
        }

        const FIXTURE: &str = "\
inputs:
  - name: provider
    enabled: true
    url: http://main.example
    username: user
    password: pass
    aliases:
      - name: alias-a
        url: http://a.example
        username: a-user
        password: a-pass
sources: []
";

        fn unique_path(name: &str) -> std::path::PathBuf {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            std::env::temp_dir().join(format!("tuliprox-source-yml-patch-{nanos}-{name}"))
        }

        fn panel_alias_fixture(style: &str, indent: usize, newline: &str) -> String {
            let mut text = String::from(concat!(
                "inputs:\n- name: provider\n  type: xtream\n  url: provider://example\n",
                "  username: root\n  password: pass\n",
                "  options:\n    update_quality:\n      live: 85\n      vod: 85\n      series: 85\n",
                "    resolve_background: false\n  cache_duration: 20h\n",
                "  # aliases:\n  # - {name: commented-old, url: provider://example}\n",
                "  max_connections: 1\n  exp_date: 1788779143\n  aliases:\n",
            ));
            for (name, expiry) in [("expired", 1), ("current", 4_102_444_800_u64)] {
                let item = if style == "flow" || (style == "mixed" && name == "current") {
                    format!("- {{name: {name}, url: provider://example, username: {name}, password: pass, max_connections: 1, exp_date: {expiry}}}\n")
                } else {
                    format!("- name: {name}\n  url: provider://example\n  username: {name}\n  password: pass\n  max_connections: 1\n  exp_date: {expiry}\n")
                };
                for line in item.lines() {
                    writeln!(text, "{}{line}", " ".repeat(indent)).expect("fixture line");
                }
            }
            text.push_str(concat!(
                "  # keep panel settings\n  panel_api:\n    enabled: true\n",
                "    url: http://panel.example\n    api_key: synthetic-key\n",
                "    alias_pool:\n      size:\n        min: 1\n        max: auto\n      remove_expired: true\n",
                "- name: next-input\n  url: http://next.example\nsources: []\n",
            ));
            text.replace('\n', newline)
        }

        #[tokio::test]
        async fn panel_alias_layout_root_update_round_trips_block_flow_and_mixed_lists() {
            for style in ["block", "flow", "mixed"] {
                for indent in [2, 4] {
                    for newline in ["\n", "\r\n"] {
                        let directory = tempfile::tempdir().expect("temporary fixture");
                        let source_path = directory.path().join("source.yml");
                        let backup_dir = directory.path().join("backup");
                        std::fs::create_dir(&backup_dir).expect("backup directory");
                        let fixture = panel_alias_fixture(style, indent, newline);
                        std::fs::write(&source_path, &fixture).expect("write fixture");
                        let before: SourcesConfigDto = serde_saphyr::from_str(&fixture).expect("original DTO");
                        let app_cfg = build_app_config(&backup_dir);
                        let patches = [
                            SourcesYmlPatch::SortAliases {
                                input_name: "provider".into(),
                                order: AliasExpDateSortOrder::NewestFirst,
                            },
                            SourcesYmlPatch::UpdateRootCredentials {
                                input_name: "provider".into(),
                                username: "renewed-root".into(),
                                password: "renewed-pass".into(),
                                exp_date: Some(4_102_444_900),
                            },
                            SourcesYmlPatch::UpdateAliasCredentials {
                                input_name: "provider".into(),
                                alias_name: "current".into(),
                                username: "renewed-alias".into(),
                                password: "pass # with: {}, commas".into(),
                                exp_date: Some(4_102_444_900),
                            },
                            SourcesYmlPatch::AddAlias {
                                input_name: "provider".into(),
                                alias_name: "added".into(),
                                base_url: "provider://example".into(),
                                username: "added-user".into(),
                                password: "added-pass".into(),
                                exp_date: Some(4_102_445_000),
                            },
                            SourcesYmlPatch::RemoveExpiredAliases { input_name: "provider".into() },
                            SourcesYmlPatch::SortAliases {
                                input_name: "provider".into(),
                                order: AliasExpDateSortOrder::NewestFirst,
                            },
                        ];
                        assert!(execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
                        let patched_text = std::fs::read_to_string(&source_path).expect("read patched_text");
                        let mut parsed: SourcesConfigDto = serde_saphyr::from_str(&patched_text).expect("reparse");
                        let input = &parsed.inputs[0];
                        assert_eq!(input.username.as_deref(), Some("renewed-root"));
                        assert_eq!(input.exp_date, Some(4_102_444_900));
                        assert_eq!(input.options, before.inputs[0].options);
                        assert_eq!(input.panel_api, before.inputs[0].panel_api);
                        assert_eq!(parsed.inputs[1], before.inputs[1]);
                        let aliases = input.aliases.as_ref().expect("aliases");
                        assert_eq!(
                            aliases.iter().map(|alias| alias.name.as_ref()).collect::<Vec<_>>(),
                            ["added", "current"]
                        );
                        assert_eq!(aliases[1].password.as_deref(), Some("pass # with: {}, commas"));
                        assert!(patched_text.contains("# - {name: commented-old, url: provider://example}"));
                        let panel_start = fixture.find("  # keep panel settings").expect("panel");
                        assert!(patched_text.ends_with(&fixture[panel_start..]));
                        let current_prefix = if style == "block" { "- name: current" } else { "- {name: current" };
                        assert!(patched_text.contains(current_prefix));
                        // The initial sort makes the older entry the last item. In
                        // mixed lists its block style is the insertion convention.
                        let added_prefix = if style == "flow" { "- {name: added" } else { "- name: added" };
                        assert!(patched_text.contains(added_prefix));
                        assert!(std::fs::read_dir(&backup_dir).expect("backups").any(|entry| {
                            let path = entry.expect("backup entry").path();
                            std::fs::read(path).is_ok_and(|bytes| bytes == fixture.as_bytes())
                        }));
                        parsed.inputs[0]
                            .prepare(0, false, &HashSet::from(["example".to_string()]), None)
                            .expect("prepare updated config");
                        assert_eq!(parsed.inputs[0].cache_duration_seconds, 72_000);
                    }
                }
            }
        }

        #[tokio::test]
        async fn panel_alias_layout_repeated_add_is_noop_and_conflict_preserves_root() {
            for style in ["block", "flow"] {
                let directory = tempfile::tempdir().expect("temporary fixture");
                let source_path = directory.path().join("source.yml");
                let backup_dir = directory.path().join("backup");
                std::fs::create_dir(&backup_dir).expect("backup directory");
                let original = panel_alias_fixture(style, 2, "\n");
                std::fs::write(&source_path, &original).expect("fixture");
                let app_cfg = build_app_config(&backup_dir);
                let add = SourcesYmlPatch::AddAlias {
                    input_name: "provider".into(),
                    alias_name: "current".into(),
                    base_url: "provider://example".into(),
                    username: "current".into(),
                    password: "pass".into(),
                    exp_date: Some(4_102_444_800),
                };
                assert!(!execute_fixture_patches(&app_cfg, &source_path, &[add]).await.expect("identical addition"));
                let patches = [
                    SourcesYmlPatch::UpdateRootCredentials {
                        input_name: "provider".into(),
                        username: "new-root".into(),
                        password: "new-pass".into(),
                        exp_date: Some(4_102_444_900),
                    },
                    SourcesYmlPatch::AddAlias {
                        input_name: "provider".into(),
                        alias_name: "current".into(),
                        base_url: "provider://example".into(),
                        username: "current".into(),
                        password: "conflicting-password".into(),
                        exp_date: Some(4_102_444_800),
                    },
                ];
                let error = execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect_err("conflict");
                assert!(error.to_string().contains("already exists"));
                assert!(!error.to_string().contains("conflicting-password"));
                assert_eq!(std::fs::read(&source_path).expect("unchanged file"), original.as_bytes());
                assert_eq!(std::fs::read_dir(&backup_dir).expect("backups").count(), 0);
            }
        }

        #[tokio::test]
        async fn panel_alias_layout_replenishes_empty_list_with_block_default() {
            for style in ["block", "flow"] {
                let directory = tempfile::tempdir().expect("temporary fixture");
                let source_path = directory.path().join("source.yml");
                let backup_dir = directory.path().join("backup");
                std::fs::create_dir(&backup_dir).expect("backup directory");
                let original = panel_alias_fixture(style, 2, "\n").replace("4102444800", "2");
                std::fs::write(&source_path, &original).expect("fixture");
                let app_cfg = build_app_config(&backup_dir);
                let patches = [
                    SourcesYmlPatch::RemoveExpiredAliases { input_name: "provider".into() },
                    SourcesYmlPatch::UpdateRootCredentials {
                        input_name: "provider".into(),
                        username: "new-root".into(),
                        password: "new-pass".into(),
                        exp_date: Some(4_102_444_900),
                    },
                    SourcesYmlPatch::AddAlias {
                        input_name: "provider".into(),
                        alias_name: "added".into(),
                        base_url: "provider://example".into(),
                        username: "added-user".into(),
                        password: "added-pass".into(),
                        exp_date: Some(4_102_445_000),
                    },
                ];
                assert!(execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect("replenish"));
                let result = std::fs::read_to_string(&source_path).expect("read");
                let parsed: SourcesConfigDto = serde_saphyr::from_str(&result).expect("reparse");
                assert_eq!(parsed.inputs[0].username.as_deref(), Some("new-root"));
                assert_eq!(parsed.inputs[0].aliases.as_ref().expect("aliases").len(), 1);
                assert!(result.contains("  aliases:\n    - name: added\n"));
                assert!(result.ends_with(&original[original.find("  # keep panel settings").expect("panel")..]));
            }
        }

        #[tokio::test]
        async fn end_to_end_patch_inserts_exp_date_and_writes_backup() {
            let dir = unique_path("insert");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let source_path = dir.join("source.yml");
            let backup_dir = dir.join("backup");
            std::fs::create_dir_all(&backup_dir).expect("mkdir backup");
            tokio::fs::write(&source_path, FIXTURE).await.expect("write fixture");

            let app_cfg = build_app_config(&backup_dir);
            let name: Arc<str> = Arc::from("provider");

            let patches = [SourcesYmlPatch::SetFetchedExpiry {
                input_name: name.clone(),
                account_name: name.clone(),
                exp_date: 1_700_000_000,
                disable: false,
            }];

            let written = execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect("patch");
            assert!(written, "patch should report a write happened");

            let patched_text = tokio::fs::read_to_string(&source_path).await.expect("read patched_text");
            assert!(patched_text.contains("exp_date: 1700000000"), "patched_text file must contain inserted exp_date");
            for line in FIXTURE.lines() {
                assert!(patched_text.contains(line), "line lost after patch: {line}");
            }

            let backup_entries: Vec<_> = std::fs::read_dir(&backup_dir).expect("readdir").flatten().collect();
            assert!(!backup_entries.is_empty(), "backup directory must contain a backup of the original");
            let backup_path = backup_entries[0].path();
            let backup_text = tokio::fs::read_to_string(&backup_path).await.expect("read backup");
            assert_eq!(backup_text, FIXTURE, "backup must equal the original fixture byte-for-byte");

            let parsed: SourcesConfigDto = serde_saphyr::from_str(&patched_text).expect("reparse");
            let updated = parsed.inputs.iter().find(|i| i.name.as_ref() == "provider").expect("input");
            assert_eq!(updated.exp_date, Some(1_700_000_000));

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn scalar_patch_batch_updates_root_and_alias_in_one_planning_step() {
            let dir = unique_path("scalar-batch");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let source_path = dir.join("source.yml");
            let backup_dir = dir.join("backup");
            std::fs::create_dir_all(&backup_dir).expect("mkdir backup");
            tokio::fs::write(&source_path, FIXTURE).await.expect("write fixture");

            let app_cfg = build_app_config(&backup_dir);
            let patches = [
                SourcesYmlPatch::SetFetchedExpiry {
                    input_name: Arc::from("provider"),
                    account_name: Arc::from("provider"),
                    exp_date: 1_700_000_000,
                    disable: false,
                },
                SourcesYmlPatch::SetFetchedExpiry {
                    input_name: Arc::from("provider"),
                    account_name: Arc::from("alias-a"),
                    exp_date: 1_800_000_000,
                    disable: false,
                },
            ];

            assert!(execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
            let patched_text = tokio::fs::read_to_string(&source_path).await.expect("read patched_text");
            let parsed: SourcesConfigDto = serde_saphyr::from_str(&patched_text).expect("reparse");
            assert_eq!(parsed.inputs[0].exp_date, Some(1_700_000_000));
            assert_eq!(parsed.inputs[0].aliases.as_ref().expect("aliases")[0].exp_date, Some(1_800_000_000));

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn flow_style_expiry_disables_root_and_alias_accounts() {
            let dir = unique_path("flow-expiry");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let source_path = dir.join("source.yml");
            let backup_dir = dir.join("backup");
            std::fs::create_dir_all(&backup_dir).expect("mkdir backup");
            let fixture = concat!(
                "inputs:\n",
                "  - {name: flow-root, type: xtream, url: http://root.example, username: root-user, password: root-pass, exp_date: 100}\n",
                "  - name: provider\n",
                "    type: xtream\n",
                "    url: http://main.example\n",
                "    username: user\n",
                "    password: pass\n",
                "    aliases:\n",
                "      - {name: flow-alias, url: http://alias.example, username: alias-user, password: alias-pass}\n",
                "sources: []\n",
            );
            tokio::fs::write(&source_path, fixture).await.expect("write fixture");
            let app_cfg = build_app_config(&backup_dir);
            let commands = [
                SourcesYmlPatch::SetFetchedExpiry {
                    input_name: Arc::from("flow-root"),
                    account_name: Arc::from("flow-root"),
                    exp_date: 100,
                    disable: true,
                },
                SourcesYmlPatch::SetFetchedExpiry {
                    input_name: Arc::from("provider"),
                    account_name: Arc::from("flow-alias"),
                    exp_date: 200,
                    disable: true,
                },
            ];

            assert!(execute_fixture_patches(&app_cfg, &source_path, &commands).await.expect("patch"));
            let patched_text = tokio::fs::read_to_string(&source_path).await.expect("read");
            let parsed: SourcesConfigDto = serde_saphyr::from_str(&patched_text).expect("reparse");
            let root = parsed.inputs.iter().find(|input| input.name.as_ref() == "flow-root").expect("root");
            let provider = parsed.inputs.iter().find(|input| input.name.as_ref() == "provider").expect("provider");
            let alias = provider
                .aliases
                .as_ref()
                .and_then(|aliases| aliases.iter().find(|alias| alias.name.as_ref() == "flow-alias"))
                .expect("alias");

            assert!(root.enabled);
            assert!(root.account_disabled);
            assert_eq!(root.exp_date, Some(100));
            assert!(!alias.enabled);
            assert_eq!(alias.exp_date, Some(200));
            assert!(patched_text.contains("name: flow-root, account_disabled: true"));
            assert!(patched_text.contains("name: flow-alias, enabled: false"));
            assert!(patched_text.contains("password: alias-pass, exp_date: 200"));

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn duplicate_alias_names_make_patch_fail_without_corrupting_file() {
            let dir = unique_path("dup");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let source_path = dir.join("source.yml");
            let backup_dir = dir.join("backup");
            std::fs::create_dir_all(&backup_dir).expect("mkdir backup");

            let fixture = "\
inputs:
  - name: provider
    url: http://main.example
    aliases:
      - name: twin
        url: http://first.example
      - name: twin
        url: http://second.example
sources: []
";
            tokio::fs::write(&source_path, fixture).await.expect("write fixture");

            let app_cfg = build_app_config(&backup_dir);
            let name: Arc<str> = Arc::from("provider");

            let patches = [SourcesYmlPatch::SetFetchedExpiry {
                input_name: name,
                account_name: Arc::from("twin"),
                exp_date: 1_700_000_000,
                disable: false,
            }];

            let err = execute_fixture_patches(&app_cfg, &source_path, &patches)
                .await
                .expect_err("duplicate alias must be rejected");
            let msg = format!("{err:?}");
            assert!(msg.contains("twin"), "error must name the duplicated alias, got: {msg}");

            let after = tokio::fs::read_to_string(&source_path).await.expect("read after");
            assert_eq!(after, fixture, "failed patch must not modify the file");

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn unchanged_payload_is_a_no_op() {
            let dir = unique_path("noop");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let source_path = dir.join("source.yml");
            let backup_dir = dir.join("backup");
            std::fs::create_dir_all(&backup_dir).expect("mkdir backup");
            tokio::fs::write(&source_path, FIXTURE).await.expect("write fixture");

            let app_cfg = build_app_config(&backup_dir);
            let name: Arc<str> = Arc::from("provider");

            let patches =
                [SourcesYmlPatch::SortAliases { input_name: name, order: AliasExpDateSortOrder::NewestFirst }];

            let written = execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect("noop");
            assert!(!written, "no-op patch must not report a write");

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn add_and_sort_alias_is_one_lossless_transaction() {
            let dir = unique_path("add-sort");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let source_path = dir.join("source.yml");
            let backup_dir = dir.join("backup");
            std::fs::create_dir_all(&backup_dir).expect("mkdir backup");
            let fixture = concat!(
                "inputs:\r\n",
                "  - name: provider\r\n",
                "    url: http://main.example\r\n",
                "    aliases:\r\n",
                "      # old account\r\n",
                "      - name: old\r\n",
                "        url: http://old.example\r\n",
                "        exp_date: 100\r\n",
                "sources: []\r\n",
            );
            tokio::fs::write(&source_path, fixture).await.expect("write fixture");
            let app_cfg = build_app_config(&backup_dir);
            let patches = [
                SourcesYmlPatch::AddAlias {
                    input_name: Arc::from("provider"),
                    alias_name: Arc::from("new"),
                    base_url: "http://new.example".to_string(),
                    username: "new-user".to_string(),
                    password: "new-pass".to_string(),
                    exp_date: Some(300),
                },
                SourcesYmlPatch::SortAliases {
                    input_name: Arc::from("provider"),
                    order: AliasExpDateSortOrder::NewestFirst,
                },
            ];

            assert!(execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
            let patched_text = tokio::fs::read_to_string(&source_path).await.expect("read");
            assert!(patched_text.find("name: new").expect("new") < patched_text.find("name: old").expect("old"));
            assert_eq!(patched_text.matches("# old account").count(), 1);
            assert!(patched_text.contains("\r\n"));

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn adding_first_alias_preserves_panel_api_and_following_input() {
            let dir = unique_path("first-alias");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let source_path = dir.join("source.yml");
            let backup_dir = dir.join("backup");
            std::fs::create_dir_all(&backup_dir).expect("mkdir backup");
            let fixture = concat!(
                "inputs:\n",
                "  - name: provider\n",
                "    type: xtream\n",
                "    url: http://main.example\n",
                "    username: user\n",
                "    password: pass\n",
                "    panel_api:\n",
                "      enabled: true\n",
                "      url: http://panel.example\n",
                "      api_key: test-key\n",
                "      alias_pool:\n",
                "        size:\n",
                "          min: 1\n",
                "          max: auto\n",
                "        remove_expired: true\n",
                "  - name: next\n",
                "    url: http://next.example\n",
                "sources: []\n",
            );
            tokio::fs::write(&source_path, fixture).await.expect("write fixture");
            let app_cfg = build_app_config(&backup_dir);
            let patches = [SourcesYmlPatch::AddAlias {
                input_name: Arc::from("provider"),
                alias_name: Arc::from("provider-new"),
                base_url: "http://new.example".to_string(),
                username: "new-user".to_string(),
                password: "new-pass".to_string(),
                exp_date: Some(300),
            }];

            assert!(execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
            let patched_text = tokio::fs::read_to_string(&source_path).await.expect("read");
            let parsed: SourcesConfigDto = serde_saphyr::from_str(&patched_text).expect("reparse");

            assert_eq!(parsed.inputs.len(), 2);
            assert!(parsed.inputs[0].panel_api.is_some());
            let aliases = parsed.inputs[0].aliases.as_ref().expect("aliases");
            assert_eq!(aliases.len(), 1);
            assert_eq!(aliases[0].name.as_ref(), "provider-new");
            assert_eq!(parsed.inputs[1].name.as_ref(), "next");
            assert!(patched_text.contains("    aliases:\n      - name: provider-new"));
            assert!(patched_text.contains("        remove_expired: true"));

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn scalar_update_and_sort_aliases_are_both_persisted() {
            let dir = unique_path("update-sort");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let source_path = dir.join("source.yml");
            let backup_dir = dir.join("backup");
            std::fs::create_dir_all(&backup_dir).expect("mkdir backup");
            let fixture = concat!(
                "inputs:\n",
                "  - name: provider\n",
                "    url: http://main.example\n",
                "    aliases:\n",
                "      - name: first\n",
                "        url: http://first.example\n",
                "        exp_date: 100\n",
                "      - name: second\n",
                "        url: http://second.example\n",
                "        exp_date: 200\n",
                "sources: []\n",
            );
            tokio::fs::write(&source_path, fixture).await.expect("write fixture");
            let app_cfg = build_app_config(&backup_dir);
            let patches = [
                SourcesYmlPatch::UpdatePanelAccountExpiry {
                    input_name: Arc::from("provider"),
                    account_name: Arc::from("first"),
                    exp_date: 300,
                },
                SourcesYmlPatch::SortAliases {
                    input_name: Arc::from("provider"),
                    order: AliasExpDateSortOrder::NewestFirst,
                },
            ];

            assert!(execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
            let patched_text = tokio::fs::read_to_string(&source_path).await.expect("read");
            assert!(
                patched_text.find("name: first").expect("first") < patched_text.find("name: second").expect("second")
            );
            assert!(patched_text.contains("exp_date: 300"));
            assert!(patched_text.contains("max_connections: 1"));

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn removing_all_aliases_is_semantically_valid() {
            let dir = unique_path("remove-all");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let source_path = dir.join("source.yml");
            let backup_dir = dir.join("backup");
            std::fs::create_dir_all(&backup_dir).expect("mkdir backup");
            let fixture = concat!(
                "inputs:\n",
                "  - name: provider\n",
                "    url: http://main.example\n",
                "    aliases:\n",
                "      - name: expired\n",
                "        url: http://expired.example\n",
                "        exp_date: 1\n",
                "sources: []\n",
            );
            tokio::fs::write(&source_path, fixture).await.expect("write fixture");
            let app_cfg = build_app_config(&backup_dir);
            let patches = [SourcesYmlPatch::RemoveExpiredAliases { input_name: Arc::from("provider") }];

            assert!(execute_fixture_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
            let patched_text = tokio::fs::read_to_string(&source_path).await.expect("read");
            assert!(!patched_text.contains("name: expired"));
            let parsed: SourcesConfigDto = serde_saphyr::from_str(&patched_text).expect("reparse");
            assert!(parsed.inputs[0].aliases.as_ref().is_none_or(Vec::is_empty));

            let _ = std::fs::remove_dir_all(&dir);
        }

        // -------------------------------------------------------------------
        // Explorer group order
        // -------------------------------------------------------------------

        mod group_order {
            use super::*;
            use shared::model::{ItemField, SortOrder, SortTarget, TargetGroupCluster};

            const FIXTURE: &str = "\
inputs:
  - name: provider
    url: http://main.example
sources:
  - inputs: [provider]
    targets:
      # keep this target first
      - name: main
        enabled: true
        output:
          - type: m3u
            filename: main.m3u

      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
";

            /// A managed live group rule as it appears in the file, indented for the `kids` target.
            fn managed_rule(patterns: &[&str]) -> String {
                let mut out = String::from(
                    "            - target: group\n              field: group\n              order: none\n              filter: 'Type = live'\n              sequence:\n",
                );
                for pattern in patterns {
                    let _ = writeln!(out, "                - '{pattern}'");
                }
                out
            }

            const FOREIGN_CHANNEL_RULE: &str =
                "            - target: channel\n              field: title\n              order: asc\n              filter: 'Type = live'\n";

            struct Fixture {
                _dir: tempfile::TempDir,
                source_path: std::path::PathBuf,
                app_cfg: Arc<AppConfig>,
            }

            impl Fixture {
                fn new(text: &str) -> Self {
                    let dir = tempfile::tempdir().expect("temp dir");
                    let source_path = dir.path().join("source.yml");
                    let backup_dir = dir.path().join("backup");
                    std::fs::create_dir(&backup_dir).expect("backup dir");
                    std::fs::write(&source_path, text).expect("fixture written");
                    let app_cfg = build_app_config(&backup_dir);
                    Self { _dir: dir, source_path, app_cfg }
                }

                async fn apply(&self, patch: SourcesYmlPatch) -> bool {
                    execute_source_yml_patches(&self.app_cfg, &self.source_path, &[patch]).await.expect("patch")
                }

                fn text(&self) -> String { std::fs::read_to_string(&self.source_path).expect("patched source.yml") }

                fn dto(&self) -> SourcesConfigDto { serde_saphyr::from_str(&self.text()).expect("reparse") }

                fn target_sort(&self, name: &str) -> Option<shared::model::ConfigSortDto> {
                    self.dto()
                        .sources
                        .iter()
                        .flat_map(|source| source.targets.iter())
                        .find(|target| target.name == name)
                        .and_then(|target| target.sort.clone())
                }
            }

            fn order_test_patch(groups: &[&str]) -> SourcesYmlPatch {
                SourcesYmlPatch::SetTargetGroupOrder {
                    target_name: Arc::from("kids"),
                    cluster: TargetGroupCluster::Live,
                    groups: groups.iter().map(ToString::to_string).collect(),
                }
            }

            fn decode_all(sort: &shared::model::ConfigSortDto) -> Vec<String> {
                sort.rules[0]
                    .sequence
                    .as_ref()
                    .expect("sequence")
                    .iter()
                    .map(|pattern| decode_group_sequence_entry(pattern).expect("managed literal"))
                    .collect()
            }

            #[tokio::test]
            async fn creates_a_sort_block_and_keeps_the_rest_of_the_file() {
                let fixture = Fixture::new(FIXTURE);
                assert!(fixture.apply(order_test_patch(&["Kids", "Cartoons"])).await);
                let text = fixture.text();
                for line in FIXTURE.lines() {
                    assert!(text.contains(line), "line lost after patch: {line}");
                }
                assert!(text.contains("        sort:\n          rules:\n            - target: group\n"), "{text}");
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 1);
                let rule = &sort.rules[0];
                assert_eq!(rule.target, SortTarget::Group);
                assert_eq!(rule.field, ItemField::Group);
                assert_eq!(rule.order, SortOrder::None);
                assert!(!rule.natural);
                assert_eq!(rule.filter, "Type = live");
                assert_eq!(decode_all(&sort), ["Kids", "Cartoons"]);
                assert!(fixture.target_sort("main").is_none(), "the other target keeps its configuration");
            }

            #[tokio::test]
            async fn replaces_an_existing_managed_rule_in_place() {
                let fixture_text = format!("{FIXTURE}        sort:\n          rules:\n{}", managed_rule(&["^Old$"]));
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&["New"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 1);
                assert_eq!(decode_all(&sort), ["New"]);
                assert!(
                    fixture.text().contains("              order: none\n"),
                    "the rest of the rule is not rewritten"
                );
            }

            #[tokio::test]
            async fn keeps_foreign_rules_after_the_managed_one() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n{}{FOREIGN_CHANNEL_RULE}",
                    managed_rule(&["^Old$"])
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&["New"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 2);
                assert_eq!(decode_all(&sort), ["New"]);
                assert_eq!(sort.rules[1].target, SortTarget::Channel);
                assert_eq!(sort.rules[1].order, SortOrder::Asc);
                assert_eq!(sort.rules[1].field, ItemField::Title);
                assert!(fixture.text().contains(FOREIGN_CHANNEL_RULE), "the foreign rule text is untouched");
            }

            #[tokio::test]
            async fn moves_a_managed_rule_back_to_the_front() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n{FOREIGN_CHANNEL_RULE}{}",
                    managed_rule(&["^Old$"])
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&["New"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 2);
                assert_eq!(decode_all(&sort), ["New"]);
                assert_eq!(sort.rules[1].target, SortTarget::Channel);
                assert!(!fixture.text().contains("^Old$"), "the displaced managed rule is removed");
            }

            #[tokio::test]
            async fn removes_the_sort_block_of_an_empty_order() {
                let fixture_text = format!("{FIXTURE}        sort:\n          rules:\n{}", managed_rule(&["^Old$"]));
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&[])).await);
                assert!(fixture.target_sort("kids").is_none());
                assert!(!fixture.text().contains("sort:"), "{}", fixture.text());
                for line in FIXTURE.lines() {
                    assert!(fixture.text().contains(line), "line lost after patch: {line}");
                }
            }

            #[tokio::test]
            async fn keeps_a_sort_block_that_holds_another_key() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          match_as_ascii: true\n          rules:\n{}",
                    managed_rule(&["^Old$"])
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&[])).await);
                let sort = fixture.target_sort("kids").expect("sort block kept");
                assert!(sort.match_as_ascii);
                assert_eq!(sort.rules.len(), 0, "the rule is removed from a kept sort block");
                let text = fixture.text();
                assert!(text.contains("match_as_ascii: true"));
                assert!(!text.contains("sequence"), "{text}");
                assert!(!text.contains("rules:"), "{text}");
            }

            #[tokio::test]
            async fn group_names_with_regex_metacharacters_round_trip() {
                let names = ["News (HD)", "A.B*C", "x$y", "back\\slash", "plus+dot.", "[brackets]"];
                let fixture = Fixture::new(FIXTURE);
                assert!(fixture.apply(order_test_patch(&names)).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(decode_all(&sort), names);
                assert!(fixture.text().contains("- ^x\\$y$"), "{}", fixture.text());
            }

            #[tokio::test]
            async fn keeps_single_line_flow_user_rules_in_block_rules_sequence() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - {{target: channel, field: title, order: asc, filter: 'Type = live'}}\n"
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&["New"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 2);
                assert_eq!(decode_all(&sort), ["New"]);
                assert_eq!(sort.rules[1].target, SortTarget::Channel);
                assert!(fixture.text().contains("target: channel, field: title, order: asc"));

                // Clearing removes only the managed rule and keeps the single-line flow user rule.
                assert!(fixture.apply(order_test_patch(&[])).await);
                let sort = fixture.target_sort("kids").expect("sort block kept for user rule");
                assert_eq!(sort.rules.len(), 1);
                assert_eq!(sort.rules[0].target, SortTarget::Channel);
                assert!(fixture.text().contains("- {target: channel, field: title, order: asc"));
            }

            #[tokio::test]
            async fn replaces_single_line_flow_managed_rule_with_block_rule() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - {{target: group, field: group, order: none, filter: 'Type = live', sequence: ['^Old$']}}\n            - {{target: channel, field: title, order: asc, filter: 'Type = live'}}\n"
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&["New"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 2);
                assert_eq!(decode_all(&sort), ["New"]);
                assert_eq!(sort.rules[1].target, SortTarget::Channel);
                assert!(!fixture.text().contains("^Old$"));
            }

            #[tokio::test]
            async fn rejects_all_unsupported_sort_layouts_with_409_error_and_leaves_file_unchanged() {
                let cases = [
                    ("flow sort", format!("{FIXTURE}        sort: {{}}\n")),
                    (
                        "flow sort with rules",
                        format!("{FIXTURE}        sort: {{rules: [{{target: channel, field: title, order: asc, filter: 'Type = live'}}]}}\n"),
                    ),
                    ("flow rules empty", format!("{FIXTURE}        sort:\n          rules: []\n")),
                    (
                        "flow rules non-empty",
                        format!("{FIXTURE}        sort:\n          rules: [{{target: channel, field: title, order: asc, filter: 'Type = live'}}]\n"),
                    ),
                    (
                        "multiline flow rule item",
                        format!("{FIXTURE}        sort:\n          rules:\n            - {{\n                target: channel,\n                field: title,\n                order: asc,\n                filter: 'Type = live'\n              }}\n"),
                    ),
                    (
                        "inline comment on sort",
                        format!("{FIXTURE}        sort: # note\n          rules:\n{FOREIGN_CHANNEL_RULE}"),
                    ),
                    (
                        "inline comment on rules",
                        format!("{FIXTURE}        sort:\n          rules: # note\n{FOREIGN_CHANNEL_RULE}"),
                    ),
                    (
                        "anchor on sort",
                        format!("{FIXTURE}        sort: &s\n          rules:\n{FOREIGN_CHANNEL_RULE}"),
                    ),
                    (
                        "anchor on rules",
                        format!("{FIXTURE}        sort:\n          rules: &r\n{FOREIGN_CHANNEL_RULE}"),
                    ),
                    ("null sort", format!("{FIXTURE}        sort: null\n")),
                    ("empty sort", format!("{FIXTURE}        sort:\n")),
                    (
                        "inline flow target",
                        "inputs:\n  - name: provider\n    url: http://main.example\nsources:\n  - inputs: [provider]\n    targets:\n      - {name: kids, output: [{type: m3u, filename: kids.m3u}]}\n".to_string(),
                    ),
                ];

                for (label, text) in cases {
                    let fixture = Fixture::new(&text);
                    let guard = fixture.app_cfg.file_locks.write_lock(&fixture.source_path).await;
                    let err = execute_target_group_order_patch_locked(
                        &fixture.app_cfg,
                        &fixture.source_path,
                        order_test_patch(&["Kids"]),
                        &guard,
                    )
                    .await
                    .expect_err(label);
                    drop(guard);
                    match err {
                        TargetGroupOrderPatchError::UnsupportedLayout(msg) => {
                            assert_eq!(msg, UNSUPPORTED_SORT_LAYOUT_MSG, "{label}");
                        }
                        other => panic!("{label}: expected UnsupportedLayout, got {other:?}"),
                    }
                    assert_eq!(fixture.text(), text, "{label}: file must remain unchanged");
                }
            }

            #[tokio::test]
            async fn unparseable_source_yml_returns_parse_error_instead_of_masking() {
                let cases = [
                    (
                        "null rules",
                        format!("{FIXTURE}        sort:\n          match_as_ascii: true\n          rules: null\n"),
                    ),
                    (
                        "empty rules",
                        format!("{FIXTURE}        sort:\n          match_as_ascii: true\n          rules:\n"),
                    ),
                    (
                        "broken input with unsupported target sort",
                        "inputs:\n  - name: provider\n    url: http://main.example\n    unknown_key: true\nsources:\n  - inputs: [provider]\n    targets:\n      - name: kids\n        output:\n          - type: m3u\n            filename: kids.m3u\n        sort: null\n".to_string(),
                    ),
                ];

                for (label, text) in cases {
                    let fixture = Fixture::new(&text);
                    let guard = fixture.app_cfg.file_locks.write_lock(&fixture.source_path).await;
                    let err = execute_target_group_order_patch_locked(
                        &fixture.app_cfg,
                        &fixture.source_path,
                        order_test_patch(&["Kids"]),
                        &guard,
                    )
                    .await
                    .expect_err(label);
                    drop(guard);
                    match err {
                        TargetGroupOrderPatchError::ValidationFailed(msg) => {
                            assert!(msg.contains("failed to parse source.yml:"), "{label}: {msg}");
                        }
                        other => panic!("{label}: expected ValidationFailed, got {other:?}"),
                    }
                    assert_eq!(fixture.text(), text, "{label}: file must remain unchanged");
                }
            }

            #[tokio::test]
            async fn prepare_failure_aborts_before_write_and_keeps_internal_revision_on_success() {
                let fixture = Fixture::new(FIXTURE);
                fixture
                    .app_cfg
                    .file_locks
                    .record_loaded_revision(&fixture.source_path, blake3::hash(FIXTURE.as_bytes()))
                    .await;
                assert!(fixture.apply(order_test_patch(&["Kids"])).await);
                assert!(
                    fixture.app_cfg.file_locks.is_internal_write_revision(&fixture.source_path).await,
                    "successful save must mark the written content as an internal revision"
                );

                let invalid_yaml = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - target: channel\n              field: title\n              order: asc\n              filter: 'Group ~ \"[unclosed\"'\n"
                );
                let bad_fixture = Fixture::new(&invalid_yaml);
                let backup_dir = bad_fixture.app_cfg.config.load().get_backup_dir().to_string();
                let guard = bad_fixture.app_cfg.file_locks.write_lock(&bad_fixture.source_path).await;
                let err = execute_target_group_order_patch_locked(
                    &bad_fixture.app_cfg,
                    &bad_fixture.source_path,
                    order_test_patch(&["Kids"]),
                    &guard,
                )
                .await
                .expect_err("invalid user regex must abort before write");
                drop(guard);
                assert!(matches!(err, TargetGroupOrderPatchError::ValidationFailed(_)), "{err:?}");
                assert_eq!(bad_fixture.text(), invalid_yaml);
                assert_eq!(std::fs::read_dir(backup_dir).expect("backup dir").count(), 0);
            }

            #[tokio::test]
            async fn leaves_a_rule_with_a_pattern_sequence_alone() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - target: group\n              field: group\n              order: none\n              filter: 'Type = live'\n              sequence:\n                - '^News.*$'\n"
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&["Kids"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 2, "a user pattern rule is not the Explorer rule");
                assert_eq!(decode_all(&sort), ["Kids"]);
                assert_eq!(sort.rules[1].sequence.as_deref(), Some(&["^News.*$".to_string()][..]));
            }

            #[tokio::test]
            async fn clearing_the_order_keeps_the_user_rules() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n{}{FOREIGN_CHANNEL_RULE}",
                    managed_rule(&["^Old$"])
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&[])).await);
                let sort = fixture.target_sort("kids").expect("sort block kept for the user rule");
                assert_eq!(sort.rules.len(), 1);
                assert_eq!(sort.rules[0].target, SortTarget::Channel);
                assert!(fixture.text().contains(FOREIGN_CHANNEL_RULE), "the user rule text is untouched");
                assert!(!fixture.text().contains("sequence"), "{}", fixture.text());
            }

            #[tokio::test]
            async fn recognizes_a_managed_rule_written_with_another_filter_spelling() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - target: group\n              field: group\n              order: none\n              filter: 'Type=live'\n              sequence:\n                - '^Old$'\n"
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&[])).await);
                assert!(fixture.target_sort("kids").is_none(), "{}", fixture.text());
            }

            #[tokio::test]
            async fn keeps_comments_inside_and_around_the_sort_block() {
                let fixture_text = format!(
                    "{FIXTURE}        # the explorer order\n        sort:\n          rules:\n{}          # keep the news rule below\n",
                    managed_rule(&["^Old$"])
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&["New"])).await);
                let text = fixture.text();
                assert!(text.contains("# the explorer order"));
                assert!(text.contains("# keep the news rule below"));
            }

            #[tokio::test]
            async fn normalizes_the_filter_spelling_of_a_recognized_rule() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - target: group\n              field: group\n              order: none\n              filter: 'Type = movie'\n              sequence:\n                - '^Old$'\n"
                );
                let fixture = Fixture::new(&fixture_text);
                let patch = SourcesYmlPatch::SetTargetGroupOrder {
                    target_name: Arc::from("kids"),
                    cluster: TargetGroupCluster::Vod,
                    groups: vec!["Movies".to_string()],
                };
                assert!(fixture.apply(patch).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 1);
                assert_eq!(sort.rules[0].filter, "Type = vod");
                assert_eq!(decode_all(&sort), ["Movies"]);
                assert!(
                    fixture.text().contains("filter: Type = vod") || fixture.text().contains("filter: 'Type = vod'"),
                    "{}",
                    fixture.text()
                );
            }

            #[test]
            fn decodes_only_anchored_escaped_literals() {
                assert_eq!(decode_group_sequence_entry("^News$").as_deref(), Some("News"));
                assert_eq!(decode_group_sequence_entry("^A\\.B$").as_deref(), Some("A.B"));
                assert_eq!(decode_group_sequence_entry("^Sports \\x21HD\\x21$").as_deref(), Some("Sports !HD!"));
                assert_eq!(decode_group_sequence_entry("^a\\\\x21$").as_deref(), Some("a\\x21"));
                assert_eq!(decode_group_sequence_entry("^a\\\\\\x21$").as_deref(), Some("a\\!"));
                assert_eq!(decode_group_sequence_entry("^Sports !HD!$"), None);
                assert_eq!(decode_group_sequence_entry("^\\x41$"), None);
                assert_eq!(decode_group_sequence_entry("^\\x2$"), None);
                assert_eq!(decode_group_sequence_entry("^\\x$"), None);
                assert_eq!(decode_group_sequence_entry("News"), None);
                assert_eq!(decode_group_sequence_entry("^News"), None);
                assert_eq!(decode_group_sequence_entry("^News.*$"), None);
                assert_eq!(decode_group_sequence_entry("^$"), None);
            }

            #[tokio::test]
            async fn noop_does_not_touch_runtime_sources() {
                let fixture = Fixture::new(FIXTURE);
                let mut sentinel_target = crate::model::ConfigTarget::from(&shared::model::ConfigTargetDto {
                    name: "kids".to_string(),
                    ..Default::default()
                });
                sentinel_target.sort = Some(ConfigSort { match_as_ascii: true, rules: Vec::new(), strategy: None });
                let sentinel = SourcesConfig {
                    sources: vec![crate::model::ConfigSource {
                        inputs: vec!["provider".intern()],
                        targets: vec![Arc::new(sentinel_target)],
                    }],
                    ..SourcesConfig::default()
                };
                fixture.app_cfg.sources.store(Arc::new(sentinel));
                assert!(!fixture.apply(order_test_patch(&[])).await, "empty order on no-sort target is a no-op");
                let after = fixture.app_cfg.sources.load();
                assert!(
                    after
                        .get_target_by_name("kids")
                        .expect("target")
                        .sort
                        .as_ref()
                        .expect("sentinel sort")
                        .match_as_ascii,
                    "a no-op must not touch the runtime config"
                );
            }

            #[tokio::test]
            async fn a_repeated_save_is_a_no_op() {
                let fixture = Fixture::new(FIXTURE);
                assert!(fixture.apply(order_test_patch(&["Kids"])).await);
                let first = fixture.text();
                assert!(!fixture.apply(order_test_patch(&["Kids"])).await, "no second write");
                assert_eq!(fixture.text(), first);
            }

            #[tokio::test]
            async fn empty_sequence_rule_is_not_managed() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - target: group\n              field: group\n              order: none\n              filter: 'Type = live'\n              sequence: []\n"
                );
                let fixture = Fixture::new(&fixture_text);
                let sort_dto = fixture.target_sort("kids").expect("sort");
                assert!(
                    !is_managed_dto_rule(&sort_dto.rules[0], TargetGroupCluster::Live),
                    "a rule with an empty sequence must not be managed"
                );
                assert!(!fixture.apply(order_test_patch(&[])).await, "clearing must not remove a sequence: [] rule");
                assert_eq!(fixture.text(), fixture_text);

                assert!(fixture.apply(order_test_patch(&["Kids"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 2, "the sequence: [] rule is kept as a user rule");
                assert_eq!(decode_all(&sort), ["Kids"]);
                assert_eq!(sort.rules[1].sequence.as_deref(), Some(&[][..]));
            }

            #[tokio::test]
            async fn rejects_sequence_update_when_comment_lies_between_managed_entries() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - target: group\n              field: group\n              order: none\n              filter: 'Type = live'\n              sequence:\n                - '^First$'\n                # keep this comment\n                - '^Second$'\n"
                );
                let fixture = Fixture::new(&fixture_text);
                let guard = fixture.app_cfg.file_locks.write_lock(&fixture.source_path).await;
                let err = execute_target_group_order_patch_locked(
                    &fixture.app_cfg,
                    &fixture.source_path,
                    order_test_patch(&["Second", "First"]),
                    &guard,
                )
                .await
                .expect_err("comment inside replaced sequence range must be rejected");
                drop(guard);
                match err {
                    TargetGroupOrderPatchError::UnsupportedLayout(msg) => {
                        assert_eq!(msg, UNSUPPORTED_SORT_LAYOUT_MSG);
                    }
                    other => panic!("expected UnsupportedLayout, got {other:?}"),
                }
                assert_eq!(fixture.text(), fixture_text, "file must remain unchanged");
            }

            async fn assert_unsupported_layout_unchanged(fixture_text: &str, groups: &[&str]) {
                let fixture = Fixture::new(fixture_text);
                let guard = fixture.app_cfg.file_locks.write_lock(&fixture.source_path).await;
                let err = execute_target_group_order_patch_locked(
                    &fixture.app_cfg,
                    &fixture.source_path,
                    order_test_patch(groups),
                    &guard,
                )
                .await
                .expect_err("a comment inside the replaced rule lines must be rejected");
                drop(guard);
                match err {
                    TargetGroupOrderPatchError::UnsupportedLayout(msg) => {
                        assert_eq!(msg, UNSUPPORTED_SORT_LAYOUT_MSG);
                    }
                    other => panic!("expected UnsupportedLayout, got {other:?}"),
                }
                assert_eq!(fixture.text(), fixture_text, "file must remain unchanged");
            }

            #[tokio::test]
            async fn rejects_sequence_update_when_entry_has_trailing_comment() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - target: group\n              field: group\n              order: none\n              filter: 'Type = live'\n              sequence:\n                - '^First$' # keep this comment\n                - '^Second$'\n"
                );
                assert_unsupported_layout_unchanged(&fixture_text, &["Second", "First"]).await;
            }

            #[tokio::test]
            async fn rejects_flow_sequence_rewrite_when_rule_holds_a_comment() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - target: group\n              # keep this comment\n              field: group\n              order: none\n              filter: 'Type = live'\n              sequence: ['^First$', '^Second$']\n"
                );
                assert_unsupported_layout_unchanged(&fixture_text, &["Second", "First"]).await;
            }

            #[tokio::test]
            async fn rejects_flow_rule_rewrite_when_line_has_trailing_comment() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - {{target: group, field: group, order: none, filter: 'Type = live', sequence: ['^First$']}} # keep\n"
                );
                assert_unsupported_layout_unchanged(&fixture_text, &["Second", "First"]).await;
            }

            #[tokio::test]
            async fn keeps_comment_above_a_rewritten_flow_rule() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            # group note\n            - {{target: group, field: group, order: none, filter: 'Type = live', sequence: ['^First$']}}\n"
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&["Second", "First"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(decode_all(&sort), ["Second", "First"]);
                assert!(fixture.text().contains("            # group note\n            - target: group\n"));
            }

            #[tokio::test]
            async fn updates_sequence_whose_quoted_entry_contains_a_hash() {
                let fixture_text = format!(
                    "{FIXTURE}        sort:\n          rules:\n            - target: group\n              field: group\n              order: none\n              filter: 'Type = live'\n              sequence:\n                - '^A \\#1$'\n                - '^Second$'\n"
                );
                let fixture = Fixture::new(&fixture_text);
                assert!(fixture.apply(order_test_patch(&["Second", "A #1"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(decode_all(&sort), ["Second", "A #1"]);
            }

            #[tokio::test]
            async fn write_failure_maps_revision_change_only_on_readable_different_hash() {
                let dir = tempfile::tempdir().expect("temp dir");
                let missing_path = dir.path().join("missing-source.yml");
                let err =
                    classify_write_failure(&missing_path, blake3::hash(b"original"), TuliproxError::Io("disk full"))
                        .await;
                match err {
                    TargetGroupOrderPatchError::Other(inner) => {
                        assert_eq!(inner.to_string(), TuliproxError::Io("disk full").to_string());
                    }
                    other => panic!("expected Other(write error), got {other:?}"),
                }

                let existing_path = dir.path().join("source.yml");
                std::fs::write(&existing_path, b"original").expect("write original");
                let same_hash_err = classify_write_failure(
                    &existing_path,
                    blake3::hash(b"original"),
                    TuliproxError::Io("permission denied"),
                )
                .await;
                assert!(
                    matches!(same_hash_err, TargetGroupOrderPatchError::Other(_)),
                    "same hash must preserve write error, got {same_hash_err:?}"
                );

                std::fs::write(&existing_path, b"modified").expect("write modified");
                let changed_err =
                    classify_write_failure(&existing_path, blake3::hash(b"original"), TuliproxError::Io("ignored"))
                        .await;
                assert!(
                    matches!(changed_err, TargetGroupOrderPatchError::RevisionChanged),
                    "readable modified file must map to RevisionChanged, got {changed_err:?}"
                );
            }

            #[tokio::test]
            async fn saves_group_order_with_flow_sort_rule_after_plain_apostrophe_filename() {
                let yaml = "\
inputs:
  - name: provider
    url: http://main.example
sources:
  - inputs: [provider]
    targets:
      - name: family
        output:
          - type: m3u
            filename: family.m3u
      - name: family_apostrophe
        output:
          - type: m3u
            filename: family's.m3u
      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
        sort:
          rules:
            - {target: channel, field: title, order: asc, filter: 'Type = live'}
";
                let fixture = Fixture::new(yaml);
                assert!(fixture.apply(order_test_patch(&["Kids"])).await);
                let sort = fixture.target_sort("kids").expect("sort block");
                assert_eq!(sort.rules.len(), 2);
                assert_eq!(decode_all(&sort), ["Kids"]);
                assert_eq!(sort.rules[1].target, SortTarget::Channel);
                assert!(fixture.text().contains("filename: family's.m3u"));
                assert!(fixture.text().contains("- {target: channel, field: title, order: asc"));
            }
        }
    }
}
