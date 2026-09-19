use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What kind of failure a provider fetch hit.
///
/// Mirrors `tuliprox_iptv::error::ProviderErrorKind`, which cannot live here:
/// it classifies a `StalkerError` that `shared` does not know about. The
/// conversion sits beside that enum so adding a variant there is a compile
/// error rather than a silent fallthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProviderFailureKind {
    /// May not repeat. The only kind worth retrying verbatim.
    Transient,
    /// The provider answered in a shape we cannot use.
    Protocol,
    /// A limit we imposed was reached - a body cap, a page limit.
    Capacity,
    /// The provider rejected our identity. Needs a new session, not a retry.
    Auth,
    /// The input is misconfigured. Nothing at runtime will fix it.
    Config,
}

impl ProviderFailureKind {
    /// Stable wire name, for a plugin matching on the classification.
    #[must_use]
    pub const fn as_wire_name(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Protocol => "protocol",
            Self::Capacity => "capacity",
            Self::Auth => "auth",
            Self::Config => "config",
        }
    }
}

/// An input's playlist fetch failed.
///
/// `ProviderErrorKind` already answers the questions an operator asks - is
/// this worth retrying, is the provider down or is the config wrong - and
/// already exposes `needs_operator()`, which is precisely "should a human
/// hear about this". Nothing consumed either. Every fetch failure was
/// counted, logged and treated identically.
///
/// # Redaction
///
/// `message` is a provider error string and can carry a URL with credentials
/// in it. The emitter passes it through
/// [`sanitize_sensitive_info`](crate::utils::sanitize_sensitive_info), for
/// the reason [`StreamProbeFailure`](crate::model::StreamProbeFailure)
/// documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFetchFailure {
    /// The input that failed, already sanitized.
    pub input: Arc<str>,
    /// Which provider implementation ran - the input's type.
    pub provider: Arc<str>,
    pub kind: ProviderFailureKind,
    /// How many errors the fetch reported. `kind` is the worst of them.
    pub error_count: usize,
    /// The worst error's text, already sanitized.
    pub message: Option<String>,
    /// Whether repeating the same call could plausibly succeed.
    pub retryable: bool,
    /// Whether this will still be true on the next scheduled refresh.
    pub needs_operator: bool,
    /// Whether some of the playlist came through anyway.
    pub partial: bool,
}

impl ProviderFetchFailure {
    /// Per input, not per error: a provider that is down fails every request
    /// behind it in the same run.
    #[must_use]
    pub fn dedup_key(&self) -> String { format!("provider-fetch:{}", self.input) }
}
