//! One question every provider failure can answer.
//!
//! The three provider families report failure in two incompatible ways: Stalker has a
//! typed [`StalkerError`] with fifteen variants, while M3U and Xtream hand back
//! `Vec<TuliproxError>` drawn from a workspace-wide enum of forty-odd categories. The
//! dispatcher had no way to ask either of them the questions it actually cares about —
//! *is this worth retrying? is the provider down, or is the config wrong?* — so it did
//! not ask. Every error was counted, logged and treated identically.
//!
//! Rather than introduce a third error type for everything to convert through,
//! [`ProviderErrorKind`] is a classification both existing types map onto. Errors keep
//! their identity; what is unified is the judgement.

use crate::stalker::error::{StalkerError, StalkerErrorKind};
use shared::error::{ErrorKind, TuliproxError};

/// What kind of failure a provider hit, independent of which error type carried it.
///
/// Ordered by how much attention it deserves: [`Self::Config`] is the most severe,
/// because it will not fix itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProviderErrorKind {
    /// The provider or the network failed in a way that may not repeat. The only kind
    /// worth retrying verbatim.
    Transient,
    /// The provider answered, but not in a shape we can use. Retrying gets the same
    /// answer; the fix is a parser change or a different endpoint.
    Protocol,
    /// A limit we imposed was reached — a body cap, a page limit. Retrying hits it again;
    /// the fix is to raise the limit or fetch less.
    Capacity,
    /// The provider rejected our identity. The fix is a new session, not a retry.
    Auth,
    /// The input is misconfigured. Nothing at runtime will fix it.
    Config,
}

impl ProviderErrorKind {
    /// Whether repeating the same call could plausibly succeed.
    ///
    /// [`Self::Auth`] is deliberately excluded: it is recoverable, but by re-handshaking,
    /// which is a different call. Reporting it as retryable is how a client ends up
    /// hammering a portal with a token that portal has already refused.
    #[must_use]
    pub const fn is_retryable(self) -> bool { matches!(self, Self::Transient) }

    /// Whether this will still be true on the next scheduled refresh. Config problems
    /// need a human; the rest may not.
    #[must_use]
    pub const fn needs_operator(self) -> bool { matches!(self, Self::Config) }

    /// Classify a workspace error — the form M3U and Xtream report in.
    #[must_use]
    pub fn of_tuliprox(error: &TuliproxError) -> Self { Self::of_error_kind(error.kind()) }

    /// Classify a [`shared::error::ErrorKind`].
    #[must_use]
    pub fn of_error_kind(kind: ErrorKind) -> Self {
        match kind {
            // Anything named `Config*` is a config problem; enumerated rather than
            // matched on the name so a renamed variant is a compile error, not a
            // silent reclassification.
            ErrorKind::Config
            | ErrorKind::ConfigApp
            | ErrorKind::ConfigCache
            | ErrorKind::ConfigBase
            | ErrorKind::ConfigApiProxy
            | ErrorKind::ConfigEpg
            | ErrorKind::ConfigHdhomerun
            | ErrorKind::ConfigInput
            | ErrorKind::ConfigIpCheck
            | ErrorKind::ConfigLibrary
            | ErrorKind::ConfigMetadataUpdate
            | ErrorKind::ConfigPanelApi
            | ErrorKind::ConfigProxy
            | ErrorKind::ConfigProxyType
            | ErrorKind::ConfigProxyUserStatus
            | ErrorKind::ConfigQosAggregation
            | ErrorKind::ConfigRateLimit
            | ErrorKind::ConfigReverseProxy
            | ErrorKind::ConfigSort
            | ErrorKind::ConfigSource
            | ErrorKind::ConfigStream
            | ErrorKind::ConfigStreamHistory
            | ErrorKind::ConfigVideoDownload
            | ErrorKind::ConfigTarget
            | ErrorKind::ConfigWebUi
            | ErrorKind::UrlParse
            | ErrorKind::FilterParse
            | ErrorKind::RegexCompile => Self::Config,

            // The account was refused.
            ErrorKind::ProxyUser => Self::Auth,

            // Reached the provider, could not use the answer.
            ErrorKind::Parse | ErrorKind::Mapper | ErrorKind::ApiXtream | ErrorKind::Probe => Self::Protocol,

            // Storage and network trouble: worth another attempt.
            ErrorKind::Io
            | ErrorKind::Download
            | ErrorKind::ProviderConnection
            | ErrorKind::Server
            | ErrorKind::Task
            | ErrorKind::Crypto
            | ErrorKind::Errors
            | ErrorKind::Repository
            | ErrorKind::RepositoryEpg
            | ErrorKind::RepositoryXtream
            | ErrorKind::RepositoryM3u
            | ErrorKind::RepositoryStalker
            | ErrorKind::RepositoryLibrary
            | ErrorKind::RepositoryStorage
            | ErrorKind::RepositoryNetwork
            | ErrorKind::RepositoryPlaylist
            | ErrorKind::RepositoryTrakt => Self::Transient,
        }
    }

    /// The kind that most deserves attention out of `errors`, or `None` when there are
    /// none. Used to decide what a partly-failed fetch means for the input as a whole.
    #[must_use]
    pub fn worst_of<'a>(errors: impl IntoIterator<Item = &'a TuliproxError>) -> Option<Self> {
        errors.into_iter().map(Self::of_tuliprox).max()
    }
}

impl From<StalkerErrorKind> for ProviderErrorKind {
    fn from(kind: StalkerErrorKind) -> Self {
        match kind {
            StalkerErrorKind::Auth => Self::Auth,
            StalkerErrorKind::Transient => Self::Transient,
            StalkerErrorKind::Protocol => Self::Protocol,
            StalkerErrorKind::Capacity => Self::Capacity,
            StalkerErrorKind::Config => Self::Config,
        }
    }
}

impl From<&StalkerError> for ProviderErrorKind {
    fn from(error: &StalkerError) -> Self { error.kind().into() }
}

/// Convert a Stalker failure into the workspace error type without losing its
/// classification.
///
/// The previous conversion flattened every Stalker failure to `ProviderConnection`, which
/// reads as "the network had a bad moment" — so a misconfigured portal URL and a rejected
/// password were both reported as connection trouble and were both counted as retryable.
#[must_use]
pub fn stalker_error_to_tuliprox(error: &StalkerError) -> TuliproxError {
    let message = format!("Stalker client error: {error}");
    match ProviderErrorKind::from(error) {
        ProviderErrorKind::Config => TuliproxError::ConfigInput(message),
        ProviderErrorKind::Auth => TuliproxError::ProxyUser(message),
        ProviderErrorKind::Protocol => TuliproxError::Parse(message),
        ProviderErrorKind::Capacity | ProviderErrorKind::Transient => TuliproxError::ProviderConnection(message),
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderErrorKind;
    use crate::stalker::{action::StalkerAction, error::StalkerError};
    use shared::error::TuliproxError;

    #[test]
    fn only_transient_failures_are_worth_repeating() {
        assert!(ProviderErrorKind::Transient.is_retryable());
        for kind in [
            ProviderErrorKind::Auth,
            ProviderErrorKind::Protocol,
            ProviderErrorKind::Capacity,
            ProviderErrorKind::Config,
        ] {
            assert!(!kind.is_retryable(), "{kind:?} must not be retried verbatim");
        }
    }

    #[test]
    fn a_config_problem_is_the_one_that_needs_a_human() {
        assert!(ProviderErrorKind::Config.needs_operator());
        assert!(!ProviderErrorKind::Transient.needs_operator());
    }

    #[test]
    fn workspace_errors_classify_by_category() {
        assert_eq!(
            ProviderErrorKind::of_tuliprox(&TuliproxError::ConfigInput("bad url".to_string())),
            ProviderErrorKind::Config
        );
        assert_eq!(
            ProviderErrorKind::of_tuliprox(&TuliproxError::Download("timeout".to_string())),
            ProviderErrorKind::Transient
        );
        assert_eq!(
            ProviderErrorKind::of_tuliprox(&TuliproxError::Parse("bad json".to_string())),
            ProviderErrorKind::Protocol
        );
    }

    #[test]
    fn stalker_and_workspace_errors_reach_the_same_verdict() {
        let stalker = StalkerError::NoEndpoint { portal: "p".into() };
        let workspace = TuliproxError::ConfigInput("no endpoint".to_string());

        assert_eq!(ProviderErrorKind::from(&stalker), ProviderErrorKind::of_tuliprox(&workspace));
    }

    /// The point of ordering the enum: a fetch that hit one blip and one bad config must
    /// be judged on the config problem.
    #[test]
    fn the_worst_failure_in_a_batch_decides() {
        let errors = vec![
            TuliproxError::Download("timeout".to_string()),
            TuliproxError::ConfigInput("bad url".to_string()),
            TuliproxError::Download("timeout".to_string()),
        ];
        assert_eq!(ProviderErrorKind::worst_of(&errors), Some(ProviderErrorKind::Config));
        assert_eq!(ProviderErrorKind::worst_of(&[]), None);
    }

    /// Flattening every Stalker failure to one workspace category was how a rejected
    /// password ended up indistinguishable from a network blip.
    #[test]
    fn converting_a_stalker_error_preserves_what_kind_it_was() {
        let cases = [
            (StalkerError::NoEndpoint { portal: "p".into() }, ProviderErrorKind::Config),
            (StalkerError::TokenRejected { status: 401, url: None }, ProviderErrorKind::Auth),
            (StalkerError::HtmlResponse { snippet: "<html>".into() }, ProviderErrorKind::Protocol),
            (
                StalkerError::BadStatus {
                    status: 502,
                    action: StalkerAction::GetOrderedList,
                    body_snippet: String::new(),
                },
                ProviderErrorKind::Transient,
            ),
        ];

        for (error, expected) in cases {
            let converted = super::stalker_error_to_tuliprox(&error);
            assert_eq!(ProviderErrorKind::of_tuliprox(&converted), expected, "{error}");
        }
    }

    #[test]
    fn a_converted_error_still_says_what_went_wrong() {
        let converted = super::stalker_error_to_tuliprox(&StalkerError::TokenRejected { status: 403, url: None });
        assert!(converted.message().contains("rejected the token"));
    }
}
