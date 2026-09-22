//! Authorization of proxied HTTP(S) resource requests.
//!
//! Every resource request decides which input supplied the concrete URL, looks up that input's
//! policy, and then runs the fetch through the client that belongs to the policy. There is no
//! permissive default: an origin that cannot be resolved is a rejection, and a missing client is a
//! rejection as well.

use crate::model::AppConfig;
use axum::http::StatusCode;
use log::{debug, error, warn};
use shared::{model::resolve_resource_value, utils::sanitize_sensitive_info};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tuliprox_core::{
    model::{public_only_policy, PolicyDigest, ResourcePolicy, ResourcePolicyError, ResourceRedirectMode},
    utils::{decode_resource_token, has_resource_token_prefix},
};

/// Whether a resource fetch may be served from (and stored in) the resource cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceCacheMode {
    Disabled,
    Enabled,
}

/// The authorization a resource request ended up with.
#[derive(Clone, Debug)]
pub struct ResolvedResourceAuthorization {
    /// Canonical main input name. `None` only for data without any origin.
    pub input_name: Option<Arc<str>>,
    /// Normalized policy. An empty policy is public-only.
    pub policy: Arc<ResourcePolicy>,
    pub policy_digest: PolicyDigest,
}

#[derive(Clone, Debug)]
pub struct ResolvedResource {
    pub url: Arc<str>,
    pub authorization: ResolvedResourceAuthorization,
}

impl ResolvedResourceAuthorization {
    /// Authorization for data without an origin: public destinations only.
    pub fn public_only() -> Self {
        let policy = public_only_policy();
        Self { input_name: None, policy_digest: policy.digest(), policy }
    }

    /// Authorization for an input that is known to be configured.
    pub fn for_input(input_name: Arc<str>, policy: Option<&Arc<ResourcePolicy>>) -> Self {
        let policy = policy.cloned().unwrap_or_else(public_only_policy);
        Self { input_name: Some(input_name), policy_digest: policy.digest(), policy }
    }
}

/// Everything the shared resource path needs besides the URL itself.
#[derive(Clone)]
pub struct ResourceFetchOptions {
    pub authorization: ResolvedResourceAuthorization,
    pub redirect_mode: ResourceRedirectMode,
    pub cache_mode: ResourceCacheMode,
}

impl ResourceFetchOptions {
    /// EPG delivery: an upstream redirect is returned to the client and nothing is cached.
    pub fn epg(authorization: ResolvedResourceAuthorization) -> Self {
        Self { authorization, redirect_mode: ResourceRedirectMode::NoRedirect, cache_mode: ResourceCacheMode::Disabled }
    }

    /// Cached resource routes: bounded redirects and cache participation.
    pub fn cached(authorization: ResolvedResourceAuthorization) -> Self {
        Self { authorization, redirect_mode: ResourceRedirectMode::Bounded, cache_mode: ResourceCacheMode::Enabled }
    }
}

/// Resolves the originating input of a resource value to its current policy.
///
/// Three outcomes are kept apart on purpose:
/// - no origin at all (legacy records, legacy links) becomes public-only;
/// - an origin naming an unknown or disabled input is rejected;
/// - a valid origin with an empty policy becomes public-only.
pub fn resolve_resource_authorization(
    app_config: &AppConfig,
    input_name: Option<&Arc<str>>,
) -> Result<ResolvedResourceAuthorization, ResourcePolicyError> {
    let Some(input_name) = input_name else {
        return Ok(ResolvedResourceAuthorization::public_only());
    };

    let sources = app_config.sources.load();
    // The lookup only contains enabled inputs and their enabled aliases, so an unknown name and a
    // disabled input are both rejected here.
    let Some(main_name) = sources.group_lookup.get(input_name) else {
        return Err(ResourcePolicyError::UnknownOrigin(input_name.to_string()));
    };
    let Some(input) = sources.get_input_by_name(main_name) else {
        return Err(ResourcePolicyError::UnknownOrigin(input_name.to_string()));
    };

    Ok(ResolvedResourceAuthorization::for_input(Arc::clone(main_name), input.resource_policy.as_ref()))
}

/// Resolves provenance before the URL reaches any transport client. A valid locator is
/// authoritative and can never fall back to the containing record's input.
pub fn resolve_resource(
    app_config: &AppConfig,
    value: &str,
    legacy_input_name: Option<&Arc<str>>,
) -> Result<ResolvedResource, ResourcePolicyError> {
    let locator = resolve_resource_value(value).map_err(|_| ResourcePolicyError::UnsupportedScheme)?;
    let (url, input_name) = locator.map_or_else(
        || (Arc::from(value), legacy_input_name.cloned()),
        |locator| (locator.url, Some(locator.input_name)),
    );
    let authorization = resolve_resource_authorization(app_config, input_name.as_ref())?;
    Ok(ResolvedResource { url, authorization })
}

/// Decodes a resource link with the decoder that belongs to the route.
///
/// New links are authenticated tokens and carry their origin; legacy links were issued before
/// origin tracking and stay public-only. The two encodings are never tried against each other, so
/// a malformed token cannot downgrade to a weaker interpretation.
pub fn decode_resource_link<F>(secret: &[u8; 16], encoded: &str, legacy_decode: F) -> Result<String, StatusCode>
where
    F: FnOnce(&[u8; 16], &str) -> Result<String, ()>,
{
    if has_resource_token_prefix(encoded) {
        return decode_resource_token(secret, encoded).map(|token| token.resource).map_err(|_| StatusCode::BAD_REQUEST);
    }
    legacy_decode(secret, encoded).map_err(|()| StatusCode::BAD_REQUEST)
}

/// Maps a policy rejection to the status the client sees.
///
/// A rejected upstream is answered like the reverse proxy would answer a broken upstream, which
/// also keeps the internal allowlist out of the response. An unresolvable origin stays a client
/// error because the link or record itself is no longer usable.
pub const fn rejection_status(error: &ResourcePolicyError) -> StatusCode {
    match error {
        ResourcePolicyError::UnknownOrigin(_) => StatusCode::BAD_REQUEST,
        ResourcePolicyError::BlockedAddress
        | ResourcePolicyError::HostNotTrusted
        | ResourcePolicyError::NetworkNotTrusted
        | ResourcePolicyError::TooManyRedirects
        | ResourcePolicyError::UnsupportedScheme
        | ResourcePolicyError::MissingHost => StatusCode::BAD_GATEWAY,
    }
}

/// At most one warning per input and minute: a hostile playlist can generate unlimited rejections,
/// and the operator only needs the first occurrences to notice.
const REJECTION_LOG_WINDOW: Duration = Duration::from_mins(1);
const REJECTION_LOG_MAX_ENTRIES: usize = 512;

static REJECTION_LOG: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Logs a rejected resource request with the origin input name.
///
/// The destination URL is never part of the warning: it is untrusted input and the operator needs
/// the origin to fix the configuration, not the failing URL.
pub fn log_resource_rejection(input_name: Option<&str>, error: &ResourcePolicyError, resource_url: &str) {
    debug!(
        "Rejected resource request from input '{}': {error} ({})",
        input_name.unwrap_or("<no origin>"),
        sanitize_sensitive_info(resource_url)
    );

    let key = input_name.unwrap_or("<no origin>").to_string();
    let now = Instant::now();
    {
        let Ok(mut log_state) = REJECTION_LOG.lock() else {
            warn!("Rejected resource request from input '{key}': {error}");
            return;
        };
        let visible = match log_state.get(&key) {
            Some(last) => now.duration_since(*last) >= REJECTION_LOG_WINDOW,
            None => true,
        };
        if !visible {
            return;
        }
        if log_state.len() >= REJECTION_LOG_MAX_ENTRIES {
            log_state.retain(|_, last| now.duration_since(*last) < REJECTION_LOG_WINDOW);
        }
        log_state.insert(key.clone(), now);
    }

    warn!("Rejected resource request from input '{key}': {error}");
}

/// Reports a request whose policy is not served by any current client.
///
/// This happens when the configuration changed between resolving the origin and using it. The
/// request is rejected instead of falling back to a broader client.
pub fn log_missing_resource_client(input_name: Option<&str>, digest: &PolicyDigest) {
    error!(
        "No resource client for policy {} of input '{}'; rejecting instead of falling back",
        digest.as_str(),
        input_name.unwrap_or("<no origin>")
    );
}

#[cfg(test)]
mod tests {
    use super::{rejection_status, ResourceCacheMode, ResourceFetchOptions};
    use axum::http::StatusCode;
    use tuliprox_core::model::{public_only_policy, ResourcePolicyError};

    #[test]
    fn epg_and_cached_options_differ_in_redirect_and_cache_mode() {
        let authorization = super::ResolvedResourceAuthorization::public_only();
        let epg = ResourceFetchOptions::epg(authorization.clone());
        let cached = ResourceFetchOptions::cached(authorization);

        assert_eq!(epg.cache_mode, ResourceCacheMode::Disabled);
        assert_eq!(cached.cache_mode, ResourceCacheMode::Enabled);
        assert_ne!(epg.redirect_mode, cached.redirect_mode);
    }

    #[test]
    fn rejection_status_separates_origin_errors_from_destination_errors() {
        assert_eq!(rejection_status(&ResourcePolicyError::UnknownOrigin("gone".to_string())), StatusCode::BAD_REQUEST);
        assert_eq!(rejection_status(&ResourcePolicyError::HostNotTrusted), StatusCode::BAD_GATEWAY);
        assert_eq!(rejection_status(&ResourcePolicyError::BlockedAddress), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn originless_authorization_is_public_only() {
        let authorization = super::ResolvedResourceAuthorization::public_only();
        assert_eq!(authorization.input_name, None);
        assert_eq!(authorization.policy_digest, public_only_policy().digest());
        assert!(authorization.policy.is_empty());
    }

    #[test]
    fn decode_resource_link_prefers_the_authenticated_token() {
        let secret = [5u8; 16];
        let encoded = tuliprox_core::utils::encode_resource_token(
            &secret,
            &tuliprox_core::utils::resource_token("resource://v1/example"),
        )
        .expect("encode");

        let decoded = super::decode_resource_link(&secret, &encoded, |_, _| Err(())).expect("decode");

        assert_eq!(decoded, "resource://v1/example");
    }

    #[test]
    fn decode_resource_link_never_falls_back_to_the_legacy_decoder() {
        let secret = [5u8; 16];
        // A malformed token must not be handed to the route's legacy decoder.
        let malformed = format!("{}not-a-token", tuliprox_core::utils::RESOURCE_TOKEN_PREFIX);

        assert_eq!(
            super::decode_resource_link(&secret, &malformed, |_, _| panic!("legacy decoder")),
            Err(StatusCode::BAD_REQUEST)
        );
    }

    #[test]
    fn decode_resource_link_uses_the_route_decoder_for_legacy_values() {
        let secret = [5u8; 16];

        let legacy =
            super::decode_resource_link(&secret, "legacy", |_, _| Ok("https://cdn.example.com/a.png".to_string()))
                .expect("legacy decode");

        assert_eq!(legacy, "https://cdn.example.com/a.png");
        assert_eq!(super::decode_resource_link(&secret, "broken", |_, _| Err(())).err(), Some(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn input_authorization_without_policy_stays_public_only_but_keeps_the_name() {
        let authorization = super::ResolvedResourceAuthorization::for_input("local".into(), None);
        assert_eq!(authorization.input_name.as_deref(), Some("local"));
        assert_eq!(authorization.policy_digest, public_only_policy().digest());
    }
}
