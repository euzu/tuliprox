use super::resolve_request_url_for_logging;
use crate::{
    api::model::{AppState, ProviderConfig},
    model::{AppConfig, ConfigInput, InputUserInfo},
    repository::{load_first_input_m3u_stream_url_by_keys, load_input_m3u_stream_url},
    utils::debug_if_enabled,
};
use shared::{
    defaults::HLS_EXT,
    model::{InputType, PlaylistItemType, StreamChannel},
    utils::{
        extract_extension_from_url, get_credentials_from_url, is_account_query_key, m3u_flussonic_live_file_lookup_key,
        m3u_stream_url_identity, sanitize_sensitive_info,
    },
};
use std::sync::Arc;
use url::Url;

pub fn get_stream_alternative_url(
    stream_url: &str,
    input: &ConfigInput,
    alias_input: &Arc<ProviderConfig>,
) -> Option<String> {
    if input.input_type.is_m3u() && input.get_matched_config_by_url(stream_url).is_none() {
        return get_stream_alternative_url_m3u(stream_url, input, alias_input);
    }

    let (source_base_url, source_username, source_password, matched_via_external_signature) =
        if let Some(matched) = input.get_matched_config_by_url(stream_url) {
            (matched.0.to_string(), matched.1.cloned(), matched.2.cloned(), false)
        } else {
            let (base_url, username, password) = find_input_account_by_signature(stream_url, input)?;
            (base_url, username, password, true)
        };
    if matched_via_external_signature && !input.input_type.is_m3u() {
        return None;
    }
    let alt_input_user_info = alias_input.get_user_info()?;

    let modified = stream_url.replacen(&source_base_url, &alt_input_user_info.base_url, 1);
    let mut url = Url::parse(&modified).ok()?;

    if let (Some(old_username), Some(old_password)) = (source_username, source_password) {
        let auth_updated = rewrite_url_auth_fields(
            &mut url,
            &old_username,
            &old_password,
            &alt_input_user_info.username,
            &alt_input_user_info.password,
        );
        if !auth_updated {
            return None;
        }
    }

    Some(url.to_string())
}

pub(super) fn get_stream_alternative_url_m3u(
    stream_url: &str,
    input: &ConfigInput,
    alias_input: &Arc<ProviderConfig>,
) -> Option<String> {
    if let Some(source_config_url) = find_input_account_by_query_signature(stream_url, input) {
        return rewrite_account_query_fields(stream_url, source_config_url, &alias_input.url);
    }

    if let Some((source_base_url, source_username, source_password)) =
        find_input_account_by_signature(stream_url, input)
    {
        let Some(alt_input_user_info) = alias_input.get_user_info() else {
            return Some(stream_url.to_string());
        };
        let modified = stream_url.replacen(&source_base_url, &alt_input_user_info.base_url, 1);
        let mut url = Url::parse(&modified).ok()?;

        if let (Some(old_username), Some(old_password)) = (source_username, source_password) {
            let auth_updated = rewrite_url_auth_fields(
                &mut url,
                &old_username,
                &old_password,
                &alt_input_user_info.username,
                &alt_input_user_info.password,
            );
            if !auth_updated {
                return None;
            }
        }

        return Some(url.to_string());
    }
    let Some(alt_input_user_info) = alias_input.get_user_info() else {
        let Ok(url) = Url::parse(stream_url) else {
            return None;
        };
        if providerless_m3u_url_has_explicit_credentials(&url) {
            return None;
        }
        return Some(stream_url.to_string());
    };
    if stream_url_has_account_signature(stream_url, &alt_input_user_info) {
        return None;
    }
    Some(stream_url.to_string())
}

pub(super) fn find_input_account_by_query_signature<'a>(stream_url: &str, input: &'a ConfigInput) -> Option<&'a str> {
    if stream_url_account_query_matches(stream_url, &input.url) {
        return Some(&input.url);
    }

    input.aliases.as_ref().and_then(|aliases| {
        aliases
            .iter()
            .find(|alias| stream_url_account_query_matches(stream_url, &alias.url))
            .map(|alias| alias.url.as_str())
    })
}

pub(super) fn stream_url_account_query_matches(stream_url: &str, config_url: &str) -> bool {
    let (Ok(stream_url), Ok(config_url)) = (Url::parse(stream_url), Url::parse(config_url)) else {
        return false;
    };

    config_url.query_pairs().any(|(config_key, config_value)| {
        is_account_query_key(&config_key)
            && stream_url.query_pairs().any(|(stream_key, stream_value)| {
                stream_key.eq_ignore_ascii_case(&config_key) && stream_value == config_value
            })
    })
}

pub(super) fn rewrite_account_query_fields(
    stream_url: &str,
    source_config_url: &str,
    target_config_url: &str,
) -> Option<String> {
    let mut stream_url = Url::parse(stream_url).ok()?;
    let source_config_url = Url::parse(source_config_url).ok()?;
    let target_config_url = Url::parse(target_config_url).ok()?;
    let source_pairs: Vec<_> = source_config_url
        .query_pairs()
        .filter(|(key, _)| is_account_query_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let target_pairs: Vec<_> = target_config_url
        .query_pairs()
        .filter(|(key, _)| is_account_query_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let unambiguous_cross_key_target =
        if source_pairs.len() == 1 && target_pairs.len() == 1 { target_pairs.first() } else { None };
    let present_source_pairs: Vec<_> = source_pairs
        .iter()
        .filter(|(source_key, source_value)| {
            stream_url.query_pairs().any(|(stream_key, stream_value)| {
                stream_key.eq_ignore_ascii_case(source_key) && stream_value == source_value.as_str()
            })
        })
        .collect();
    let replacements: Vec<_> = present_source_pairs
        .iter()
        .filter_map(|(source_key, source_value)| {
            target_pairs
                .iter()
                .find(|(target_key, _)| target_key.eq_ignore_ascii_case(source_key))
                .or(unambiguous_cross_key_target)
                .map(|(target_key, target_value)| {
                    (source_key.clone(), source_value.clone(), target_key.clone(), target_value.clone())
                })
        })
        .collect();

    if replacements.len() != present_source_pairs.len() || replacements.is_empty() {
        return None;
    }

    let mut replaced = false;
    let stream_pairs: Vec<(String, String)> = stream_url
        .query_pairs()
        .map(|(key, value)| {
            if let Some((_, _, target_key, target_value)) =
                replacements.iter().find(|(source_key, source_value, _, _)| {
                    key.eq_ignore_ascii_case(source_key) && value == source_value.as_str()
                })
            {
                replaced = true;
                (target_key.clone(), target_value.clone())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect();

    if !replaced {
        return None;
    }
    stream_url
        .query_pairs_mut()
        .clear()
        .extend_pairs(stream_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())));
    Some(stream_url.to_string())
}

pub(super) fn providerless_m3u_url_has_explicit_credentials(url: &Url) -> bool {
    !url.username().is_empty()
        || url.password().is_some()
        || url
            .query_pairs()
            .any(|(key, _)| key.eq_ignore_ascii_case("username") || key.eq_ignore_ascii_case("password"))
}

/// Look for an account signature in the stream URL that matches the input
/// itself or one of its configured aliases. Returns the matching entry's
/// `(base_url, username, password)` so the caller can rewrite only the
/// account-specific parts of the URL while preserving the original host/path.
///
/// This helper is used for safe credential rewrites when Tuliprox switches
/// from one account to another. It is not the general trust gate for M3U
/// foreign hosts: plain external URLs from a stored M3U playlist item may be
/// accepted without a matching signature, while unrelated credential-bearing
/// URLs still fail closed unless they provably match the input or one of its
/// aliases.
pub(super) fn find_input_account_by_signature(
    stream_url: &str,
    input: &ConfigInput,
) -> Option<(String, Option<String>, Option<String>)> {
    // Try the input's main account first.
    if let Some(user_info) = input.get_user_info() {
        if stream_url_account_matches(stream_url, &user_info) {
            return Some((input.url.clone(), Some(user_info.username), Some(user_info.password)));
        }
    }
    // Then try each alias, if any. The input_type is inherited from the
    // parent input for all aliases — see ConfigInputAlias definition.
    if let Some(aliases) = input.aliases.as_ref() {
        for alias in aliases {
            if let Some(user_info) =
                InputUserInfo::new(input.input_type, alias.username.as_deref(), alias.password.as_deref(), &alias.url)
            {
                if stream_url_account_matches(stream_url, &user_info) {
                    return Some((alias.url.clone(), Some(user_info.username), Some(user_info.password)));
                }
            }
        }
    }
    None
}

pub(super) fn rewrite_url_auth_fields(
    url: &mut Url,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
) -> bool {
    if rewrite_query_auth_fields(url, new_username, new_password) {
        return true;
    }

    if url.username() == old_username && url.password() == Some(old_password) {
        return url.set_username(new_username).is_ok() && url.set_password(Some(new_password)).is_ok();
    }

    rewrite_path_auth_fields(url, old_username, old_password, new_username, new_password)
}

pub(super) fn rewrite_query_auth_fields(url: &mut Url, new_username: &str, new_password: &str) -> bool {
    let mut has_username = false;
    let mut has_password = false;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| {
            if key.eq_ignore_ascii_case("username") {
                has_username = true;
                (key.into_owned(), new_username.to_string())
            } else if key.eq_ignore_ascii_case("password") {
                has_password = true;
                (key.into_owned(), new_password.to_string())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect();

    if !(has_username && has_password) {
        return false;
    }

    url.query_pairs_mut().clear().extend_pairs(pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())));
    true
}

pub(super) fn collect_path_segments(url: &Url) -> Option<Vec<String>> {
    url.path_segments().map(|segments| segments.map(ToOwned::to_owned).collect::<Vec<_>>())
}

pub(super) fn find_path_auth_segment_index(segments: &[String], username: &str, password: &str) -> Option<usize> {
    segments.windows(2).position(|pair| {
        pair.first().is_some_and(|segment| segment == username)
            && pair.get(1).is_some_and(|segment| segment == password)
    })
}

pub(super) fn rewrite_path_auth_fields(
    url: &mut Url,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
) -> bool {
    let Some(mut segments) = collect_path_segments(url) else {
        return false;
    };

    let credential_index = find_path_auth_segment_index(&segments, old_username, old_password);
    let Some(credential_index) = credential_index else {
        return false;
    };

    segments[credential_index] = new_username.to_string();
    segments[credential_index + 1] = new_password.to_string();

    let Ok(mut path_segments) = url.path_segments_mut() else {
        return false;
    };
    path_segments.clear().extend(segments.iter().map(String::as_str));
    true
}

pub(super) fn stream_url_matches_provider(stream_url: &str, provider_cfg: &ProviderConfig) -> bool {
    let account_query_matches =
        provider_cfg.input_type.is_m3u() && stream_url_account_query_matches(stream_url, &provider_cfg.url);
    let Some(user_info) = provider_cfg.get_user_info() else {
        return account_query_matches;
    };
    if stream_url_base_matches(stream_url, &user_info.base_url) {
        // Same-host fast path: both base URL and account identity must match.
        return stream_url_account_matches(stream_url, &user_info);
    }
    if !provider_cfg.input_type.is_m3u() {
        return false;
    }
    // For M3U inputs, the stored playlist entry itself is the trust anchor.
    // Open external URLs are therefore allowed, but external URLs that carry
    // explicit account markers must still match the selected provider account.
    if stream_url_has_account_signature(stream_url, &user_info) {
        return stream_url_account_matches(stream_url, &user_info);
    }
    true
}

pub(super) fn stream_url_base_matches(stream_url: &str, base_url: &str) -> bool {
    stream_url
        .strip_prefix(base_url)
        .is_some_and(|remaining| remaining.is_empty() || remaining.starts_with(['/', '?', '#']))
}

pub(super) fn stream_url_account_matches(stream_url: &str, user_info: &crate::model::InputUserInfo) -> bool {
    let Ok(url) = Url::parse(stream_url) else {
        return false;
    };

    let (url_username, url_password) = get_credentials_from_url(&url);
    if let (Some(url_username), Some(url_password)) = (url_username.as_deref(), url_password.as_deref()) {
        return url_username == user_info.username && url_password == user_info.password;
    }

    let mut has_query_username = false;
    let mut has_query_password = false;
    for (key, value) in url.query_pairs() {
        if key.eq_ignore_ascii_case("username") {
            has_query_username = value == user_info.username;
        } else if key.eq_ignore_ascii_case("password") {
            has_query_password = value == user_info.password;
        }
    }
    if has_query_username || has_query_password {
        return has_query_username && has_query_password;
    }

    let Some(segments) = collect_path_segments(&url) else {
        return false;
    };

    find_path_auth_segment_index(&segments, &user_info.username, &user_info.password).is_some()
}

pub(super) fn stream_url_has_account_signature(stream_url: &str, user_info: &crate::model::InputUserInfo) -> bool {
    let Ok(url) = Url::parse(stream_url) else {
        return false;
    };

    let (url_username, url_password) = get_credentials_from_url(&url);
    if url_username.is_some() && url_password.is_some() {
        return true;
    }

    let mut has_query_username = false;
    let mut has_query_password = false;
    for (key, _) in url.query_pairs() {
        if key.eq_ignore_ascii_case("username") {
            has_query_username = true;
        } else if key.eq_ignore_ascii_case("password") {
            has_query_password = true;
        }
    }
    if has_query_username || has_query_password {
        return has_query_username && has_query_password;
    }

    // Path-based credentials: some Xtream endpoints embed the account in the URL
    // path (e.g. /live/<user>/<pass>/...). Only flag a signature when the
    // consecutive segments actually match the configured user/pass — arbitrary
    // open paths must not be treated as account signatures.
    if let Some(segments) = collect_path_segments(&url) {
        if find_path_auth_segment_index(&segments, &user_info.username, &user_info.password).is_some() {
            return true;
        }
    }

    false
}

/// Maps a Flussonic archive URL of another account onto the alias account.
///
/// The URL index only holds live URLs, so an archive URL derived from the main account's
/// live URL never matches. The archive lives next to the channel's live file, so the alias
/// URL of that sibling supplies the alias credentials and the archive file name is kept.
pub(super) async fn resolve_m3u_alias_flussonic_archive_url(
    app_config: &Arc<AppConfig>,
    provider_name: &Arc<str>,
    stream_url: &str,
) -> Option<String> {
    let requested = Url::parse(stream_url).ok()?;
    let archive_file = requested.path_segments()?.next_back()?;
    crate::iptv::m3u::parse_flussonic_archive_file(archive_file)?;
    // The live file the archive was derived from is probed first: same stem, then same transport.
    let archive_lower = archive_file.to_ascii_lowercase();
    let archive_is_hls = archive_lower.ends_with(HLS_EXT);
    let archive_stem = archive_lower.split_once('-').map(|(stem, _)| stem);
    let mut live_files = crate::iptv::m3u::FLUSSONIC_LIVE_FILES;
    live_files.sort_by_key(|live_file| {
        let same_stem =
            archive_stem.is_some_and(|stem| live_file.split_once('.').is_some_and(|(name, _)| name == stem));
        let same_transport = live_file.ends_with(HLS_EXT) == archive_is_hls;
        (!same_stem, !same_transport)
    });
    // Each sibling is probed by its exact identity, then by its case-folded Flussonic key.
    let keys: Vec<String> = live_files
        .iter()
        .filter_map(|live_file| {
            let mut sibling = requested.clone();
            sibling.path_segments_mut().ok()?.pop().push(live_file);
            Some([m3u_stream_url_identity(sibling.as_str()), m3u_flussonic_live_file_lookup_key(sibling.as_str())])
        })
        .flatten()
        .flatten()
        .collect();
    let alias_live_url = match load_first_input_m3u_stream_url_by_keys(app_config, provider_name, keys).await {
        Ok(url) => url?,
        Err(err) => {
            debug_if_enabled!(
                "Failed to resolve M3U archive sibling URL for provider {}: {}",
                sanitize_sensitive_info(provider_name),
                sanitize_sensitive_info(&err.to_string())
            );
            return None;
        }
    };
    let mut alias_url = Url::parse(&alias_live_url).ok()?;
    alias_url.path_segments_mut().ok()?.pop().push(archive_file);
    Some(alias_url.into())
}

pub(crate) async fn select_provider_stream_url(
    stream_url: &str,
    input: &ConfigInput,
    provider_cfg: &Arc<ProviderConfig>,
    accept_requested_stream_url: bool,
    app_config: &Arc<AppConfig>,
) -> Option<(Arc<str>, String)> {
    if accept_requested_stream_url {
        return Some((provider_cfg.name.clone(), stream_url.to_string()));
    }

    if provider_cfg.input_type.is_m3u() {
        match load_input_m3u_stream_url(app_config, &provider_cfg.name, stream_url).await {
            Ok(Some(provider_stream_url)) => {
                debug_if_enabled!(
                    "M3U alias URL lookup: provider={} requested_url={} index_match=true resolved_url={}",
                    sanitize_sensitive_info(&provider_cfg.name),
                    sanitize_sensitive_info(resolve_request_url_for_logging(input, stream_url).as_ref()),
                    sanitize_sensitive_info(resolve_request_url_for_logging(input, &provider_stream_url).as_ref())
                );
                return Some((provider_cfg.name.clone(), provider_stream_url.to_string()));
            }
            Ok(None) => {
                if let Some(provider_stream_url) =
                    resolve_m3u_alias_flussonic_archive_url(app_config, &provider_cfg.name, stream_url).await
                {
                    debug_if_enabled!(
                        "M3U alias URL lookup: provider={} requested_url={} index_match=archive_sibling resolved_url={}",
                        sanitize_sensitive_info(&provider_cfg.name),
                        sanitize_sensitive_info(resolve_request_url_for_logging(input, stream_url).as_ref()),
                        sanitize_sensitive_info(resolve_request_url_for_logging(input, &provider_stream_url).as_ref())
                    );
                    return Some((provider_cfg.name.clone(), provider_stream_url));
                }
                debug_if_enabled!(
                    "M3U alias URL lookup: provider={} requested_url={} index_match=false",
                    sanitize_sensitive_info(&provider_cfg.name),
                    sanitize_sensitive_info(resolve_request_url_for_logging(input, stream_url).as_ref())
                );
            }
            Err(err) => {
                debug_if_enabled!(
                    "Failed to resolve M3U stream URL for provider {}: {}",
                    sanitize_sensitive_info(&provider_cfg.name),
                    sanitize_sensitive_info(&err.to_string())
                );
            }
        }
    }

    if stream_url_matches_provider(stream_url, provider_cfg) {
        Some((provider_cfg.name.clone(), stream_url.to_string()))
    } else {
        get_stream_alternative_url(stream_url, input, provider_cfg).map(|url| (provider_cfg.name.clone(), url))
    }
}

/// Stored Xtream VOD entries belong to their named input even when their URL uses a CDN host.
pub(super) fn resolve_xtream_vod_provider_url(
    stream_url: &str,
    input: &ConfigInput,
    provider_cfg: &ProviderConfig,
    channel: &StreamChannel,
) -> Option<String> {
    if input.input_type != InputType::Xtream
        || provider_cfg.input_type != InputType::Xtream
        || channel.item_type != PlaylistItemType::Video
        || channel.input_name != input.name
        || channel.url.as_ref() != stream_url
        || channel.provider_id == 0
    {
        return None;
    }
    let url = Url::parse(stream_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let is_main = provider_cfg.name == input.name;
    if !is_main
        && !input.aliases.as_ref().is_some_and(|aliases| aliases.iter().any(|alias| alias.name == provider_cfg.name))
    {
        return None;
    }
    let selected = provider_cfg.get_user_info()?;
    let origin = input.get_user_info()?;
    if is_main
        && selected.base_url == origin.base_url
        && selected.username == origin.username
        && selected.password == origin.password
    {
        return Some(stream_url.to_string());
    }

    // A direct source may carry account-bound tokens. A different account obtains its own redirect.
    let extension = channel
        .technical
        .as_ref()
        .map(|technical| technical.container.as_str())
        .filter(|container| !container.is_empty())
        .map(|container| match container {
            "mpegts" => "ts",
            "hls" => "m3u8",
            "dash" => "mpd",
            other => other,
        })
        .or_else(|| extract_extension_from_url(&channel.url).and_then(|extension| extension.strip_prefix('.')));
    let base_url = selected.base_url.trim_end_matches('/');
    let mut url = format!("{base_url}/movie/{}/{}/{}", selected.username, selected.password, channel.provider_id);
    if let Some(extension) = extension {
        url.push('.');
        url.push_str(extension);
    }
    Some(url)
}

pub(super) fn get_redirect_alternative_url(
    app_state: &Arc<AppState>,
    redirect_url: &Arc<str>,
    input: &ConfigInput,
) -> Arc<str> {
    if let Some((base_url, username, password)) = input.get_matched_config_by_url(redirect_url) {
        if let Some(provider_cfg) = app_state.active_provider.get_next_provider(&input.name) {
            let mut new_url = redirect_url.replacen(base_url, provider_cfg.url.as_str(), 1);
            if let (Some(old_username), Some(old_password)) = (username, password) {
                if let (Some(new_username), Some(new_password)) =
                    (provider_cfg.username.as_ref(), provider_cfg.password.as_ref())
                {
                    new_url = new_url.replacen(old_username, new_username, 1);
                    new_url = new_url.replacen(old_password, new_password, 1);
                    return new_url.into();
                }
                // one has credentials the other not, something not right
                return redirect_url.clone();
            }
            return new_url.into();
        }
    }
    redirect_url.clone()
}
