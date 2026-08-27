use crate::utils::sanitize_sensitive_info;
use std::fmt::{Display, Formatter};

#[macro_export]
macro_rules! get_errors_notify_message {
    ($errors:expr, $size:expr) => {
        if $errors.is_empty() {
            None
        } else {
            let text = $errors
                .iter()
                .filter(|err| err.is_notify())
                .map(|err| err.message())
                .collect::<Vec<&str>>()
                .join("\r\n");

            let max_size = $size;
            if max_size > 0 && text.chars().count() > max_size {
                let truncated: String = text.chars().take(max_size).collect();
                Some(format!("{}...", truncated))
            } else if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
    };
}

pub use get_errors_notify_message;

// #[macro_export]
// macro_rules! handle_tuliprox_error_result {
//     ($result: expr) => {
//         if let Err(err) = $result {
//             return Err($crate::error::TuliproxError::Config(err.to_string()));
//         }
//     };
// }
// pub use handle_tuliprox_error_result;

/// Declares every error category once: the [`ErrorKind`] variant, its display
/// label, and the matching constructor on [`TuliproxError`].
macro_rules! error_kinds {
    ($($variant:ident => $label:literal),+ $(,)?) => {
        /// The category of a [`TuliproxError`].
        ///
        /// Fieldless and `Copy`, so a category can be compared, matched, stored
        /// and returned without touching the message.
        #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
        pub enum ErrorKind {
            $($variant,)+
        }

        impl ErrorKind {
            /// The prefix this category is displayed with.
            #[must_use]
            pub const fn label(self) -> &'static str {
                match self {
                    $(Self::$variant => $label,)+
                }
            }
        }

        // Constructors named after the categories, so the 755 existing
        // `TuliproxError::Config(msg)` call sites keep compiling verbatim: a
        // tuple-variant constructor and an associated function are invoked with
        // identical syntax. The non-snake-case names are the deliberate price of
        // splitting the type without touching every call site at once.
        #[allow(non_snake_case)]
        impl TuliproxError {
            $(
                #[doc = concat!("Build a `", $label, "`.")]
                #[inline]
                #[must_use]
                pub fn $variant(message: impl Into<Box<str>>) -> Self {
                    Self::new(ErrorKind::$variant, message)
                }
            )+
        }
    };
}

error_kinds! {
    Errors => "errors",
    ApiXtream => "api xtream error",
    Config => "config error",
    ConfigApp => "config app error",
    ConfigCache => "config cache error",
    ConfigBase => "config base error",
    ConfigApiProxy => "config API proxy error",
    ConfigEpg => "config EPG error",
    ConfigHdhomerun => "config HDHomeRun error",
    ConfigInput => "config input error",
    ConfigIpCheck => "config IP check error",
    ConfigLibrary => "config library error",
    ConfigMetadataUpdate => "config metadata update error",
    ConfigPanelApi => "config panel API error",
    ConfigProxy => "config proxy error",
    ConfigProxyType => "config proxy type error",
    ConfigProxyUserStatus => "config proxy user status error",
    ConfigQosAggregation => "config QoS aggregation error",
    ConfigRateLimit => "config rate limit error",
    ConfigReverseProxy => "config reverse proxy error",
    ConfigSort => "config sort error",
    ConfigSource => "config source error",
    ConfigStream => "config stream error",
    ConfigStreamHistory => "config stream history error",
    ConfigVideoDownload => "config video download error",
    ConfigTarget => "config target error",
    ConfigWebUi => "config web UI error",
    RepositoryEpg => "repository EPG error",
    RepositoryXtream => "repository XTream error",
    RepositoryM3u => "repository M3U error",
    RepositoryStalker => "repository Stalker error",
    RepositoryLibrary => "repository Library error",
    RepositoryStorage => "repository Storage error",
    RepositoryNetwork => "repository Network error",
    RepositoryPlaylist => "repository Playlist error",
    RepositoryTrakt => "repository Trakt error",
    FilterParse => "filter parse error",
    Mapper => "mapper error",
    RegexCompile => "regex compile error",
    UrlParse => "url parse error",
    Io => "io error",
    Crypto => "crypto error",
    Server => "server error",
    Probe => "probe error",
    Task => "task error",
    Parse => "parse error",
    ProxyUser => "proxy user error",
    Repository => "repository error",
    Download => "download error",
    ProviderConnection => "provider connection error",
}

impl ErrorKind {
    /// Whether an error of this category should reach the user as a
    /// notification rather than only the log.
    #[must_use]
    pub const fn is_notify(self) -> bool {
        matches!(
            self,
            Self::RepositoryEpg
                | Self::RepositoryXtream
                | Self::RepositoryM3u
                | Self::RepositoryStalker
                | Self::RepositoryLibrary
                | Self::RepositoryStorage
                | Self::RepositoryNetwork
                | Self::RepositoryPlaylist
                | Self::RepositoryTrakt
                | Self::Repository
                | Self::Download
                | Self::ProviderConnection
        )
    }
}

/// A categorised error with a message.
///
/// This used to be a 50-variant enum in which every variant carried exactly one
/// `String` and nothing else -- so it was never really an enum of errors, but a
/// category tag beside a message. Encoding it as an enum cost a 50-arm
/// `message()` whose only job was to return the payload, plus a second 50-name
/// list in `is_notify()` that had to be kept in sync by hand. Splitting the two
/// makes the category `Copy` and comparable, moves classification onto
/// [`ErrorKind`], and leaves one field access where the match used to be.
#[derive(Debug)]
pub struct TuliproxError {
    kind: ErrorKind,
    message: Box<str>,
}

impl TuliproxError {
    #[inline]
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<Box<str>>) -> Self { Self { kind, message: message.into() } }

    #[inline]
    #[must_use]
    pub const fn kind(&self) -> ErrorKind { self.kind }

    /// The message, without the category prefix that [`Display`] adds.
    #[inline]
    #[must_use]
    pub fn message(&self) -> &str { &self.message }

    #[inline]
    #[must_use]
    pub const fn is_notify(&self) -> bool { self.kind.is_notify() }
}

impl Display for TuliproxError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result { write!(f, "{}: {}", self.kind.label(), self.message) }
}

impl std::error::Error for TuliproxError {}

pub fn to_io_error<E>(err: E) -> std::io::Error
where
    E: std::error::Error,
{
    std::io::Error::other(sanitize_sensitive_info(&err.to_string()))
}

pub fn str_to_io_error(err: &str) -> std::io::Error { std::io::Error::other(sanitize_sensitive_info(err)) }

pub fn string_to_io_error(err: impl AsRef<str>) -> std::io::Error {
    std::io::Error::other(sanitize_sensitive_info(err.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::{ErrorKind, TuliproxError};

    #[test]
    fn display_keeps_the_label_colon_message_shape_the_enum_produced() {
        assert_eq!(TuliproxError::Config("bad").to_string(), "config error: bad");
        assert_eq!(TuliproxError::Errors("a\nb").to_string(), "errors: a\nb");
        assert_eq!(TuliproxError::ConfigWebUi("nope").to_string(), "config web UI error: nope");
        assert_eq!(TuliproxError::RepositoryM3u("gone").to_string(), "repository M3U error: gone");
    }

    #[test]
    fn message_returns_the_payload_without_the_label() {
        let err = TuliproxError::ConfigEpg(String::from("boom"));
        assert_eq!(err.message(), "boom");
        assert_eq!(err.kind(), ErrorKind::ConfigEpg);
    }

    #[test]
    fn constructors_accept_both_str_and_string() {
        // The old String payload forced `.to_string()` at call sites that only
        // had a &str; `impl Into<Box<str>>` accepts either.
        assert_eq!(TuliproxError::Io("x").message(), "x");
        assert_eq!(TuliproxError::Io(String::from("y")).message(), "y");
        assert_eq!(TuliproxError::Io(format!("z{}", 1)).message(), "z1");
    }

    #[test]
    fn only_the_repository_download_and_provider_categories_notify() {
        for kind in [
            ErrorKind::Repository,
            ErrorKind::RepositoryEpg,
            ErrorKind::RepositoryXtream,
            ErrorKind::RepositoryM3u,
            ErrorKind::RepositoryStalker,
            ErrorKind::RepositoryLibrary,
            ErrorKind::RepositoryStorage,
            ErrorKind::RepositoryNetwork,
            ErrorKind::RepositoryPlaylist,
            ErrorKind::RepositoryTrakt,
            ErrorKind::Download,
            ErrorKind::ProviderConnection,
        ] {
            assert!(kind.is_notify(), "{kind:?} should notify");
        }
        for kind in [ErrorKind::Config, ErrorKind::Errors, ErrorKind::Io, ErrorKind::Server, ErrorKind::ApiXtream] {
            assert!(!kind.is_notify(), "{kind:?} should not notify");
        }
    }

    #[test]
    fn kind_is_copy_and_the_error_is_not_larger_than_the_enum_it_replaced() {
        // The 50-variant enum was a String (24 bytes) plus a discriminant.
        // kind + Box<str> is one word smaller.
        let kind = ErrorKind::Config;
        let copied = kind;
        assert_eq!(kind, copied);
        assert!(
            size_of::<TuliproxError>() <= size_of::<String>() + size_of::<usize>(),
            "unexpected size: {}",
            size_of::<TuliproxError>()
        );
    }
}
