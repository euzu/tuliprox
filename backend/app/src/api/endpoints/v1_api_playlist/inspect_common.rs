//! Building blocks shared by the read-only playlist/EPG inspection endpoints (`items`, `epg_inspect`):
//! paging window, closed-set query parameters, case-insensitive search and the JSON error response.

use axum::{http::StatusCode, response::IntoResponse};
use serde_json::json;

pub(super) const DEFAULT_PAGE_LIMIT: u32 = 50;
pub(super) const MAX_PAGE_LIMIT: u32 = 500;

/// A parameter whose accepted values are a closed, documented set.
///
/// Implemented by [`query_enum!`]; the schema of the MCP tools lists the same names.
pub(in crate::api::endpoints) trait QueryEnum: Copy + Sized + 'static {
    const ALL: &'static [Self];
    const NAMES: &'static [&'static str];
    fn name(self) -> &'static str;
}

/// Declares a closed-set query enum: the variants, their wire names, `ALL`, `NAMES` and `name()`.
macro_rules! query_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        $vis enum $name {
            $($variant),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            /// Wire names of every variant, in declaration order.
            pub const NAMES: &'static [&'static str] = &[$($text),+];

            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl $crate::api::endpoints::v1_api_playlist::inspect_common::QueryEnum for $name {
            const ALL: &'static [Self] = Self::ALL;
            const NAMES: &'static [&'static str] = Self::NAMES;

            fn name(self) -> &'static str { Self::name(self) }
        }
    };
}
pub(super) use query_enum;

/// Error of an inspection endpoint, rendered as `{"error": "..."}`.
#[derive(Debug)]
pub(super) struct InspectError {
    status: StatusCode,
    message: String,
}

impl InspectError {
    pub(super) fn bad_request(message: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: message.into() }
    }

    pub(super) fn not_found(message: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: message.into() }
    }

    pub(super) fn internal(message: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: message.into() }
    }

    #[cfg(test)]
    pub(super) fn message(&self) -> &str { &self.message }
}

impl IntoResponse for InspectError {
    fn into_response(self) -> axum::response::Response {
        (self.status, axum::Json(json!({ "error": self.message }))).into_response()
    }
}

/// The page asked for, after applying defaults and the hard maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PageWindow {
    pub offset: usize,
    pub limit: usize,
    /// The requested limit exceeded the maximum and was reduced to it.
    pub limit_clamped: bool,
}

impl PageWindow {
    /// `limit` of `None` or `0` means the default.
    pub(super) fn resolve(offset: Option<u32>, limit: Option<u32>, default_limit: u32, max_limit: u32) -> Self {
        let requested = limit.filter(|limit| *limit > 0).unwrap_or(default_limit);
        Self {
            offset: offset.unwrap_or(0) as usize,
            limit: requested.min(max_limit) as usize,
            limit_clamped: requested > max_limit,
        }
    }

    /// Whether the matched record at zero-based position `index` belongs to the page.
    pub(super) fn contains(&self, index: usize) -> bool {
        index.checked_sub(self.offset).is_some_and(|position| position < self.limit)
    }

    /// Whether matched records exist beyond the `returned` ones of this page.
    pub(super) fn has_more(&self, total_matched: usize, returned: usize) -> bool {
        self.offset.saturating_add(returned) < total_matched
    }
}

const MAX_ECHOED_VALUE_CHARS: usize = 64;

/// Parses one value of a closed-set parameter; the error lists the allowed values.
pub(super) fn parse_enum<E: QueryEnum>(param: &str, value: &str) -> Result<E, InspectError> {
    let value = value.trim();
    E::ALL.iter().copied().find(|candidate| candidate.name() == value).ok_or_else(|| {
        let shown: String = value.chars().take(MAX_ECHOED_VALUE_CHARS).collect();
        InspectError::bad_request(format!("Unknown {param} '{shown}'; allowed values: {}", E::NAMES.join(", ")))
    })
}

/// Parses a repeatable closed-set parameter, dropping duplicates and keeping the first-seen order.
pub(super) fn parse_enum_list<E: QueryEnum + PartialEq>(
    param: &str,
    values: Option<&[String]>,
) -> Result<Vec<E>, InspectError> {
    let mut parsed: Vec<E> = Vec::new();
    for value in values.unwrap_or_default() {
        let item = parse_enum::<E>(param, value)?;
        if !parsed.contains(&item) {
            parsed.push(item);
        }
    }
    Ok(parsed)
}

/// Case-insensitive substring matcher with an allocation-free path for ASCII needles.
#[derive(Debug, Clone)]
pub(super) struct CaseInsensitiveNeedle {
    needle: String,
    ascii: bool,
}

impl CaseInsensitiveNeedle {
    /// `None` for an empty (or blank-only) search text: no filter.
    pub(super) fn new(text: Option<&str>) -> Option<Self> {
        let text = text.map(str::trim).filter(|text| !text.is_empty())?;
        Some(Self { needle: text.to_lowercase(), ascii: text.is_ascii() })
    }

    pub(super) fn is_in(&self, haystack: &str) -> bool {
        if self.ascii {
            let needle = self.needle.as_bytes();
            needle.len() <= haystack.len()
                && haystack.as_bytes().windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
        } else {
            haystack.to_lowercase().contains(&self.needle)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_enum, parse_enum_list, CaseInsensitiveNeedle, PageWindow};

    query_enum! {
        enum Sample {
            First => "first",
            SecondValue => "second_value",
        }
    }

    #[test]
    fn query_enum_exposes_names_in_declaration_order() {
        assert_eq!(Sample::NAMES, &["first", "second_value"]);
        assert_eq!(Sample::ALL.len(), Sample::NAMES.len());
        for (variant, name) in Sample::ALL.iter().zip(Sample::NAMES) {
            assert_eq!(variant.name(), *name);
        }
    }

    #[test]
    fn parse_enum_accepts_known_and_lists_allowed_values_otherwise() {
        assert_eq!(parse_enum::<Sample>("kind", " first ").unwrap(), Sample::First);
        let err = parse_enum::<Sample>("kind", "nope").unwrap_err();
        assert!(err.message.contains("Unknown kind 'nope'"), "{}", err.message);
        assert!(err.message.contains("first, second_value"), "{}", err.message);
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_enum_list_dedups_and_fails_on_unknown() {
        let values = vec!["second_value".to_string(), "first".to_string(), "second_value".to_string()];
        assert_eq!(parse_enum_list::<Sample>("kind", Some(&values)).unwrap(), vec![Sample::SecondValue, Sample::First]);
        assert_eq!(parse_enum_list::<Sample>("kind", None).unwrap(), Vec::<Sample>::new());
        assert!(parse_enum_list::<Sample>("kind", Some(&["x".to_string()])).is_err());
    }

    #[test]
    fn page_window_defaults_and_clamps() {
        let default = PageWindow::resolve(None, None, 50, 500);
        assert_eq!(default, PageWindow { offset: 0, limit: 50, limit_clamped: false });
        let zero = PageWindow::resolve(Some(3), Some(0), 50, 500);
        assert_eq!(zero, PageWindow { offset: 3, limit: 50, limit_clamped: false });
        let exact = PageWindow::resolve(None, Some(500), 50, 500);
        assert_eq!((exact.limit, exact.limit_clamped), (500, false));
        let clamped = PageWindow::resolve(None, Some(501), 50, 500);
        assert_eq!((clamped.limit, clamped.limit_clamped), (500, true));
        let huge = PageWindow::resolve(None, Some(u32::MAX), 50, 500);
        assert_eq!((huge.limit, huge.limit_clamped), (500, true));
    }

    #[test]
    fn page_window_contains_is_offset_inclusive_and_end_exclusive() {
        let window = PageWindow::resolve(Some(2), Some(3), 50, 500);
        let inside: Vec<usize> = (0..8).filter(|index| window.contains(*index)).collect();
        assert_eq!(inside, vec![2, 3, 4]);
    }

    #[test]
    fn page_window_has_more() {
        let window = PageWindow::resolve(Some(2), Some(3), 50, 500);
        assert!(window.has_more(6, 3), "one record remains");
        assert!(!window.has_more(5, 3), "exactly the end");
        assert!(!window.has_more(1, 0), "offset beyond total");
        assert!(!window.has_more(0, 0));
        let far = PageWindow::resolve(Some(u32::MAX), Some(10), 50, 500);
        assert!(!far.has_more(3, 0));
    }

    #[test]
    fn needle_matches_ascii_case_insensitively() {
        let needle = CaseInsensitiveNeedle::new(Some("  News ")).unwrap();
        assert!(needle.is_in("Daily NEWS 24"));
        assert!(needle.is_in("breaking news"));
        assert!(!needle.is_in("sports"));
        assert!(!needle.is_in("ne"));
        assert!(CaseInsensitiveNeedle::new(Some("   ")).is_none());
        assert!(CaseInsensitiveNeedle::new(None).is_none());
    }

    #[test]
    fn needle_matches_non_ascii_case_insensitively() {
        let needle = CaseInsensitiveNeedle::new(Some("Über")).unwrap();
        assert!(needle.is_in("DER ÜBERFALL"));
        assert!(needle.is_in("über"));
        assert!(!needle.is_in("uber"));
    }
}
