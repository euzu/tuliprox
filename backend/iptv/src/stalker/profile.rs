use crate::stalker::session::StalkerSession;
use serde::Deserialize;
use shared::{
    model::stalker::{
        StalkerAuthMode, StalkerBootstrapRecipe, StalkerEndpointPreference, StalkerMagPreset, StalkerPlaybackMode,
        StalkerPortalCapabilitiesDto, StalkerStreamKind,
    },
    utils::deserialize_as_option_string,
};
use std::{fmt, time::Duration};
use tuliprox_core::model::{StalkerInputConfig, StalkerSizeCaps};

/// Information the `get_profile` action returns about the underlying portal account. We
/// deserialize it loosely (all fields optional) because the field set varies by portal
/// firmware. The fields we actually rely on are `max_connections`, `expiration` and
/// `status` — those drive connection admission and credential rotation.
#[derive(Default, Clone, Deserialize)]
pub struct StalkerRawProviderProfile {
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub login: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub password: Option<String>,
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub expiration: Option<String>,
    #[serde(default)]
    pub max_connections: Option<u16>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub storage_usage: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub storage_total: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub phone: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub email: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub account_number: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub tariff_plan: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    pub portal_url: Option<String>,
}

impl fmt::Debug for StalkerRawProviderProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StalkerRawProviderProfile")
            .field("id", &self.id.as_ref().map(|_| "[redacted]"))
            .field("name", &self.name.as_ref().map(|_| "[redacted]"))
            .field("login", &self.login.as_ref().map(|_| "[redacted]"))
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .field("status", &self.status)
            .field("expiration", &self.expiration)
            .field("max_connections", &self.max_connections)
            .field("storage_usage", &self.storage_usage)
            .field("storage_total", &self.storage_total)
            .field("phone", &self.phone.as_ref().map(|_| "[redacted]"))
            .field("email", &self.email.as_ref().map(|_| "[redacted]"))
            .field("account_number", &self.account_number.as_ref().map(|_| "[redacted]"))
            .field("tariff_plan", &self.tariff_plan)
            .field("portal_url", &self.portal_url.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Resolved, runtime view of a Stalker input. Built by the client at handshake time from
/// the backend `StalkerInputConfig` + the raw profile returned by the portal. The
/// plan and the runtime fields never diverge in shape — only the `portal_capabilities`
/// field is portal-specific and is refreshed on every handshake.
#[derive(Clone)]
pub struct StalkerProviderProfile {
    pub auth_mode: StalkerAuthMode,
    pub mag_preset: StalkerMagPreset,
    pub endpoint_preference: StalkerEndpointPreference,
    pub bootstrap_recipe: StalkerBootstrapRecipe,
    pub bootstrap_recipe_chain: Vec<StalkerBootstrapRecipe>,
    pub account_login: Option<String>,
    pub account_password: Option<String>,
    pub account_id: Option<String>,
    pub status: Option<i32>,
    pub expiration: Option<String>,
    pub max_connections: Option<u16>,
    pub storage_usage: Option<String>,
    pub storage_total: Option<String>,
    pub portal_capabilities: StalkerPortalCapabilitiesDto,
    pub action_size_caps: StalkerSizeCaps,
}

impl fmt::Debug for StalkerProviderProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StalkerProviderProfile")
            .field("auth_mode", &self.auth_mode)
            .field("mag_preset", &self.mag_preset)
            .field("endpoint_preference", &self.endpoint_preference)
            .field("bootstrap_recipe", &self.bootstrap_recipe)
            .field("bootstrap_recipe_chain", &self.bootstrap_recipe_chain)
            .field("account_login", &self.account_login.as_ref().map(|_| "[redacted]"))
            .field("account_password", &self.account_password.as_ref().map(|_| "[redacted]"))
            .field("account_id", &self.account_id.as_ref().map(|_| "[redacted]"))
            .field("status", &self.status)
            .field("expiration", &self.expiration)
            .field("max_connections", &self.max_connections)
            .field("storage_usage", &self.storage_usage)
            .field("storage_total", &self.storage_total)
            .field("portal_capabilities", &self.portal_capabilities)
            .field("action_size_caps", &self.action_size_caps)
            .finish()
    }
}

impl StalkerProviderProfile {
    #[allow(clippy::too_many_arguments)]
    pub fn from_config(
        config: &StalkerInputConfig,
        raw: StalkerRawProviderProfile,
        resolved_recipe: StalkerBootstrapRecipe,
        recipe_chain: Vec<StalkerBootstrapRecipe>,
        capabilities: StalkerPortalCapabilitiesDto,
        size_caps: StalkerSizeCaps,
        account_login: Option<String>,
        account_password: Option<String>,
    ) -> Self {
        Self {
            auth_mode: config.auth_mode,
            mag_preset: config.mag_preset,
            endpoint_preference: config.endpoint_preference,
            bootstrap_recipe: resolved_recipe,
            bootstrap_recipe_chain: recipe_chain,
            account_login: raw.login.or(account_login),
            account_password: raw.password.or(account_password),
            account_id: raw.id.or(raw.account_number),
            status: raw.status,
            expiration: raw.expiration,
            max_connections: raw.max_connections,
            storage_usage: raw.storage_usage,
            storage_total: raw.storage_total,
            portal_capabilities: capabilities,
            action_size_caps: size_caps,
        }
    }

    /// Whether the portal reports an active account. The portal will return `status=0` or
    /// omit the field for blocked/expired accounts; the proxy will then refuse to fetch
    /// any new content from this input.
    pub fn is_active(&self) -> bool {
        matches!(self.status, Some(1) | None)
    }

    /// Soft TTL the client uses to decide whether a fresh `get_profile` is required before
    /// a new download batch. The portal will eventually invalidate the session anyway, so
    /// the TTL is intentionally conservative.
    pub fn profile_freshness_window() -> Duration {
        Duration::from_mins(10)
    }
}

/// A fully-typed `create_link` result: a playable URL plus the chain of command variants
/// that produced it. Reverse-proxy code uses the chain to decide whether to retry with a
/// different `cmd` after a 4xx upstream error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalkerResolvedStream {
    pub stream_url: String,
    pub stream_kind: StalkerStreamKind,
    pub playback_mode: StalkerPlaybackMode,
    pub candidates: Vec<String>,
}

impl StalkerResolvedStream {
    /// The next command candidate to try when the current URL fails with a 4xx upstream
    /// error. Returns the first candidate when the current URL is not part of the chain
    /// (i.e. nothing matched) and `None` when the chain has been exhausted past the last
    /// match.
    pub fn next_candidate_after(&self, current_url: &str) -> Option<&str> {
        let mut iter = self.candidates.iter();
        for candidate in iter.by_ref() {
            if candidate == current_url {
                return iter.next().map(String::as_str);
            }
        }
        // No match — fall back to the first candidate so the caller can retry from the
        // top of the chain instead of silently giving up.
        self.candidates.first().map(String::as_str)
    }
}

/// Pair returned by every successful handshake: the session used for subsequent calls and
/// the profile (capabilities + account info) used to drive playlist parsing.
#[derive(Debug, Clone)]
pub struct StalkerHandshake {
    pub session: StalkerSession,
    pub profile: StalkerProviderProfile,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_chain(urls: &[&str]) -> Vec<String> {
        urls.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn resolved_stream_advances_after_match() {
        let stream = StalkerResolvedStream {
            stream_url: "http://a".to_string(),
            stream_kind: StalkerStreamKind::Live,
            playback_mode: StalkerPlaybackMode::DirectUrl,
            candidates: candidate_chain(&["http://a", "http://b", "http://c"]),
        };
        assert_eq!(stream.next_candidate_after("http://a"), Some("http://b"));
        assert_eq!(stream.next_candidate_after("http://b"), Some("http://c"));
        assert_eq!(stream.next_candidate_after("http://c"), None);
    }

    #[test]
    fn resolved_stream_advances_after_no_match() {
        let stream = StalkerResolvedStream {
            stream_url: "http://a".to_string(),
            stream_kind: StalkerStreamKind::Movie,
            playback_mode: StalkerPlaybackMode::PlayMoviePortal,
            candidates: candidate_chain(&["http://a", "http://b"]),
        };
        assert_eq!(stream.next_candidate_after("http://missing"), Some("http://a"));
    }

    #[test]
    fn profile_active_when_status_is_one() {
        let mut profile = StalkerProviderProfile::from_config(
            &StalkerInputConfig::default(),
            StalkerRawProviderProfile { status: Some(1), ..StalkerRawProviderProfile::default() },
            StalkerBootstrapRecipe::GenericSafe,
            vec![StalkerBootstrapRecipe::GenericSafe],
            StalkerPortalCapabilitiesDto::default(),
            StalkerSizeCaps::default(),
            None,
            None,
        );
        assert!(profile.is_active());
        profile.status = Some(0);
        assert!(!profile.is_active());
    }

    #[test]
    fn profile_active_when_status_missing() {
        let profile = StalkerProviderProfile::from_config(
            &StalkerInputConfig::default(),
            StalkerRawProviderProfile::default(),
            StalkerBootstrapRecipe::GenericSafe,
            vec![StalkerBootstrapRecipe::GenericSafe],
            StalkerPortalCapabilitiesDto::default(),
            StalkerSizeCaps::default(),
            None,
            None,
        );
        assert!(profile.is_active());
    }

    #[test]
    fn raw_profile_accepts_numeric_zero_for_string_like_fields() {
        let raw: StalkerRawProviderProfile = serde_json::from_value(serde_json::json!({
            "status": 1,
            "expiration": 0,
            "storage_usage": 0,
            "storage_total": 0,
            "phone": 0,
            "email": 0,
            "account_number": 0,
            "tariff_plan": 0,
            "portal_url": 0
        }))
        .expect("profile should deserialize");
        assert_eq!(raw.expiration.as_deref(), Some("0"));
        assert_eq!(raw.storage_usage.as_deref(), Some("0"));
        assert_eq!(raw.portal_url.as_deref(), Some("0"));
    }
}
