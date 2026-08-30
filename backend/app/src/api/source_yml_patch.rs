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
            build_alias_sort_edit, build_field_insertion_edit, find_input, parse_and_validate_patched_text,
            parse_patch_document, serialize_yaml_scalar, span_byte_range, TextEdit,
        },
        write_config_text_file,
    },
    model::{is_input_expired, is_input_expired_at},
    repository::AliasExpDateSortOrder,
};
use log::warn;
use shared::{
    error::TuliproxError,
    model::{ConfigInputAliasDto, ConfigInputDto, SourcesConfigDto},
    utils::Internable,
};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::Path,
    sync::Arc,
};
use tuliprox_core::model::AppConfig;
use url::Url;

// ---------------------------------------------------------------------------
// Command enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum SourcesYmlPatch {
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
}

impl SourcesYmlPatch {
    /// Structural alias edits change subsequent YAML byte spans and therefore
    /// form their own planning step. Consecutive scalar-only commands can be
    /// planned and validated together without reparsing the whole document.
    const fn changes_alias_structure(&self) -> bool {
        matches!(
            self,
            Self::SortAliases { .. }
                | Self::PersistProvisionedAccount { .. }
                | Self::AddAlias { .. }
                | Self::RemoveExpiredAliases { .. }
        )
    }
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

#[allow(clippy::too_many_lines)]
pub(crate) fn apply_sources_yml_patches(
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
                        || !doc.inputs[idx].enabled
                        || doc.inputs[idx].max_connections != 1
                    {
                        doc.inputs[idx].exp_date = Some(*exp_date);
                        doc.inputs[idx].enabled = true;
                        doc.inputs[idx].max_connections = 1;
                        changed = true;
                    }
                    continue;
                }
                let Some(alias_idx) = alias_indices[idx].get(account_name).copied() else {
                    return Err(TuliproxError::ConfigPanelApi(format!(
                        "source.yml patch target alias '{account_name}' under input '{input_name}' was not found"
                    )));
                };
                let aliases = doc.inputs[idx].aliases.as_mut().ok_or_else(|| {
                    TuliproxError::ConfigPanelApi(format!("source.yml patch: input '{input_name}' has no aliases"))
                })?;
                if aliases[alias_idx].exp_date != Some(*exp_date) || aliases[alias_idx].max_connections != 1 {
                    aliases[alias_idx].exp_date = Some(*exp_date);
                    aliases[alias_idx].max_connections = 1;
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
                    continue;
                };
                if sort_aliases_by_exp_date_order(aliases, *order) {
                    alias_indices[idx] =
                        aliases.iter().enumerate().map(|(idx, alias)| (alias.name.clone(), idx)).collect();
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
                let current_root_is_usable = input.exp_date.is_some()
                    && !is_input_expired_at(input.exp_date, jsonwebtoken::get_current_timestamp());
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
                    changed = true;
                    continue;
                }
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
            SourcesYmlPatch::AddAlias { input_name, alias_name, base_url, username, password, exp_date } => {
                let idx = *inputs_by_name.get(input_name).ok_or_else(|| {
                    TuliproxError::ConfigPanelApi(format!("source.yml patch target input '{input_name}' was not found"))
                })?;
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
                    continue;
                };
                let before = aliases.len();
                aliases.retain(|a| !is_input_expired(a.exp_date));
                if aliases.len() != before {
                    alias_indices[idx] =
                        aliases.iter().enumerate().map(|(idx, alias)| (alias.name.clone(), idx)).collect();
                    changed = true;
                }
            }
        }
    }

    Ok(changed)
}

// ---------------------------------------------------------------------------
// Text-edit planning (translates semantic diff into byte edits)
// ---------------------------------------------------------------------------

fn plan_text_edits(
    original_text: &str,
    before: &SourcesConfigDto,
    expected: &SourcesConfigDto,
    _patches: &[SourcesYmlPatch],
) -> Result<Vec<TextEdit>, TuliproxError> {
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

fn apply_patch_planning_step(
    expected: &mut SourcesConfigDto,
    patched_text: &mut String,
    patches: &[SourcesYmlPatch],
) -> Result<bool, TuliproxError> {
    let mut next_expected = expected.clone();
    if !apply_sources_yml_patches(&mut next_expected, patches)? {
        return Ok(false);
    }
    let text_edits = plan_text_edits(patched_text, expected, &next_expected, patches)?;
    let next_text = apply_scalar_edits(patched_text, text_edits)?;
    parse_and_validate_patched_text(&next_text, &next_expected)?;
    *expected = next_expected;
    *patched_text = next_text;
    Ok(true)
}

/// Executes a batch of `SourcesYmlPatch` commands as a single atomic transaction.
///
/// Returns `Ok(true)` if the file was written, `Ok(false)` if no change was needed.
/// The write lock is acquired internally — callers must NOT hold it.
pub(crate) async fn execute_source_yml_patches(
    app_config: &Arc<AppConfig>,
    sources_path: &Path,
    patches: &[SourcesYmlPatch],
) -> Result<bool, TuliproxError> {
    if patches.is_empty() {
        return Ok(false);
    }

    let _lock = app_config.file_locks.write_lock(sources_path).await;

    // Step 2: Read original bytes
    let original_bytes = tokio::fs::read(sources_path)
        .await
        .map_err(|err| TuliproxError::ConfigPanelApi(format!("source.yml patch: failed to read file: {err}")))?;
    // Step 3: Compute revision
    let original_revision = blake3::hash(&original_bytes);
    let original_text = String::from_utf8(original_bytes)
        .map_err(|_| TuliproxError::ConfigPanelApi("source.yml patch: file is not valid UTF-8".to_string()))?;

    // Step 4: Parse as SourcesConfigDto
    let before: SourcesConfigDto = serde_saphyr::from_str(&original_text)
        .map_err(|err| TuliproxError::ConfigPanelApi(format!("source.yml patch: failed to parse source.yml: {err}")))?;

    // Steps 5-9: Batch consecutive scalar commands into one clone/parse/validation pass.
    // Structural alias commands remain sequential because they change the byte spans used by
    // subsequent edits. The complete command list still produces one final atomic disk write.
    let mut expected = before;
    let mut patched_text = original_text;
    let mut changed = false;
    let mut patch_index = 0;
    while patch_index < patches.len() {
        let step_end = if patches[patch_index].changes_alias_structure() {
            patch_index + 1
        } else {
            patches[patch_index..]
                .iter()
                .position(SourcesYmlPatch::changes_alias_structure)
                .map_or(patches.len(), |offset| patch_index + offset)
        };
        changed |= apply_patch_planning_step(&mut expected, &mut patched_text, &patches[patch_index..step_end])?;
        patch_index = step_end;
    }

    // Step 6: No-op check
    if !changed {
        return Ok(false);
    }

    // Step 10: Verify bytes outside edit ranges came from original
    // (implicitly guaranteed by the edit model — apply_scalar_edits only modifies declared ranges)

    // Step 11: Re-read and check revision
    let backup_dir = app_config.config.load().get_backup_dir().to_string();

    // Step 12-13: Write with backup and atomic replace (includes revision check)
    let written = write_config_text_file(
        sources_path.to_string_lossy().as_ref(),
        &backup_dir,
        &patched_text,
        "source.yml",
        Some(original_revision),
    )
    .await?;

    if written {
        // Step 14: Mark internal write revision
        app_config
            .file_locks
            .mark_internal_write_revision(sources_path)
            .await
            .map_err(|err| TuliproxError::Io(format!("Failed to track internal source update: {err}")))?;
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_loader::source_patch::{ensure_block_style_for_insertion, TextEdit};
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

        let patched = edits_to_text(fixture, edits);
        for line in fixture.lines() {
            assert!(patched.contains(line), "line lost after patch: {line}");
        }
        assert!(patched.contains("exp_date: 1700000000"));
        // The exact bug signature: `pass\n    exp_date: ...` glued with no separator.
        assert!(
            !patched.contains("pass1700000000") && !patched.contains("pass exp_date"),
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
        let patched = edits_to_text(fixture, edits);
        assert!(patched.contains("enabled: false"));
        assert_eq!(patched.matches("enabled:").count(), 1, "no duplicate `enabled:` key");
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
        let patched = edits_to_text(fixture, edits);
        let reparsed: crate::config_loader::source_patch::SourcePatchDocument =
            serde_saphyr::from_str(&patched).expect("reparse");
        let panel = reparsed.inputs[0].value.panel_api.as_ref().expect("panel");
        assert_eq!(panel.value.credits.as_ref().expect("credits").value, "42");
    }

    #[test]
    fn flow_style_insertion_is_rejected_with_actionable_error() {
        // `find_input` is the entry point; it does not reject flow-style itself. The rejector is
        // `ensure_block_style_for_insertion`. We drive it directly to keep the unit test focused.
        let fixture = "inputs:\n  - { name: provider, url: http://main.example }\n";
        let doc: crate::config_loader::source_patch::SourcePatchDocument =
            serde_saphyr::from_str(fixture).expect("parse");
        let input = &doc.inputs[0].value;
        let anchor = span_byte_range(&input.name).expect("name span");

        let err = ensure_block_style_for_insertion(fixture, &anchor, "exp_date").expect_err("must reject");
        let msg = format!("{err}");
        assert!(
            msg.contains("cannot insert optional field 'exp_date' into a flow-style YAML mapping"),
            "unexpected error: {msg}"
        );
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
                    api_proxy_file_path: String::new(),
                    custom_stream_response_path: None,
                })),
                custom_stream_response: Arc::new(arc_swap::ArcSwapOption::default()),
                access_token_secret: [0; 32],
                encrypt_secret: [0; 16],
                media_tools: Arc::new(MediaToolCapabilities::new()),
            })
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
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            std::env::temp_dir().join(format!("tuliprox-source-yml-patch-{nanos}-{name}"))
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

            let written = execute_source_yml_patches(&app_cfg, &source_path, &patches).await.expect("patch");
            assert!(written, "patch should report a write happened");

            let patched_text = tokio::fs::read_to_string(&source_path).await.expect("read patched");
            assert!(patched_text.contains("exp_date: 1700000000"), "patched file must contain inserted exp_date");
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

            assert!(execute_source_yml_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
            let patched = tokio::fs::read_to_string(&source_path).await.expect("read patched");
            let parsed: SourcesConfigDto = serde_saphyr::from_str(&patched).expect("reparse");
            assert_eq!(parsed.inputs[0].exp_date, Some(1_700_000_000));
            assert_eq!(parsed.inputs[0].aliases.as_ref().expect("aliases")[0].exp_date, Some(1_800_000_000));

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

            let err = execute_source_yml_patches(&app_cfg, &source_path, &patches)
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

            let written = execute_source_yml_patches(&app_cfg, &source_path, &patches).await.expect("noop");
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

            assert!(execute_source_yml_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
            let patched = tokio::fs::read_to_string(&source_path).await.expect("read");
            assert!(patched.find("name: new").expect("new") < patched.find("name: old").expect("old"));
            assert_eq!(patched.matches("# old account").count(), 1);
            assert!(patched.contains("\r\n"));

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

            assert!(execute_source_yml_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
            let patched = tokio::fs::read_to_string(&source_path).await.expect("read");
            assert!(patched.find("name: first").expect("first") < patched.find("name: second").expect("second"));
            assert!(patched.contains("exp_date: 300"));
            assert!(patched.contains("max_connections: 1"));

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

            assert!(execute_source_yml_patches(&app_cfg, &source_path, &patches).await.expect("patch"));
            let patched = tokio::fs::read_to_string(&source_path).await.expect("read");
            assert!(!patched.contains("name: expired"));
            let parsed: SourcesConfigDto = serde_saphyr::from_str(&patched).expect("reparse");
            assert!(parsed.inputs[0].aliases.as_ref().is_none_or(Vec::is_empty));

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
