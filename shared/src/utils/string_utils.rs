use crate::utils::CONSTANTS;
use deunicode::deunicode_with_tofu_cow;
use std::{borrow::Cow, sync::Arc};

/// Cleans a playlist title by removing common IPTV prefixes (e.g., "[US]", "┃DE┃").
pub fn clean_playlist_title(title: &str) -> String { CONSTANTS.re_clean_title.replace(title, "").trim().to_string() }

pub trait Capitalize {
    fn capitalize(&self) -> String;
}

// Implement the Capitalize trait for &str
impl<T: AsRef<str>> Capitalize for T {
    fn capitalize(&self) -> String {
        let s = self.as_ref();
        let mut chars = s.chars();
        let first = chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
        let rest = chars.as_str().to_lowercase();
        first + &rest
    }
}

pub fn get_trimmed_string(value: Option<&str>) -> Option<String> {
    if let Some(v) = value {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

pub fn generate_random_string(length: usize) -> String {
    let mut rng = fastrand::Rng::new();
    let random_string: String = (0..length).map(|_| rng.alphanumeric()).collect();
    random_string
}

// compare 2 small vecs without HashSet
pub fn small_vecs_equal_unordered<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    for item in a {
        if !b.iter().any(|x| x == item) {
            return false;
        }
    }
    true
}

pub fn get_non_empty_str<'a>(first: &'a str, second: &'a str, third: &'a str) -> &'a str {
    if !first.is_empty() {
        first
    } else if !second.is_empty() {
        second
    } else {
        third
    }
}

pub fn is_blank_optional_str(s: Option<&str>) -> bool {
    s.as_ref().is_none_or(|s| s.chars().all(|c| c.is_whitespace()))
}

pub fn is_blank_optional_string(s: &Option<String>) -> bool {
    s.as_ref().is_none_or(|s| s.chars().all(|c| c.is_whitespace()))
}

pub fn is_non_blank_optional_string(s: &Option<String>) -> bool { !is_blank_optional_string(s) }

pub fn is_blank_optional_arc_str(s: &Option<::std::sync::Arc<str>>) -> bool {
    s.as_ref().is_none_or(|s| s.chars().all(|c| c.is_whitespace()))
}

pub fn trim_slash(s: &str) -> Cow<'_, str> {
    let trimmed = s.trim_matches('/');
    if trimmed.len() == s.len() {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(trimmed.to_string())
    }
}

pub fn trim_last_slash(s: &str) -> Cow<'_, str> {
    if s.ends_with('/') {
        if let Some(stripped) = s.strip_suffix('/') {
            return Cow::Owned(stripped.to_string());
        }
    }
    Cow::Borrowed(s)
}

pub trait Substring {
    fn substring(&self, from: usize, to: usize) -> String;
}

impl Substring for String {
    fn substring(&self, from: usize, to: usize) -> String { self.chars().skip(from).take(to - from).collect() }
}

pub fn truncate_string(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        s.chars().take(max_len).collect()
    }
}

pub fn mask_credentials(s: &str) -> String {
    match s.chars().next() {
        Some(first) => format!("{first}..."),
        None => "...".to_string(),
    }
}

pub fn humanize_snake_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = true;

    for c in s.chars() {
        if c == '_' {
            result.push(' ');
            capitalize_next = true;
        } else if capitalize_next {
            for up in c.to_uppercase() {
                result.push(up);
            }
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }

    result
}

pub fn deunicode_string(s: &str) -> Cow<'_, str> { deunicode_with_tofu_cow(s, "[?]") }

pub fn longest<'a>(a: &'a Arc<str>, b: &'a Arc<str>) -> &'a Arc<str> {
    if a.len() >= b.len() {
        a
    } else {
        b
    }
}

fn trim_leading_zeros(digits: &[u8]) -> &[u8] {
    let start = digits.iter().position(|b| *b != b'0').unwrap_or(digits.len().saturating_sub(1));
    &digits[start..]
}

/// Compare strings with embedded ascii integers numerically ("Chan 2" < "Chan 10").
pub fn natural_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let l = left.as_bytes();
    let r = right.as_bytes();
    let (mut i, mut j) = (0usize, 0usize);
    while i < l.len() && j < r.len() {
        if l[i].is_ascii_digit() && r[j].is_ascii_digit() {
            let li = i;
            while i < l.len() && l[i].is_ascii_digit() {
                i += 1;
            }
            let rj = j;
            while j < r.len() && r[j].is_ascii_digit() {
                j += 1;
            }
            let ls = trim_leading_zeros(&l[li..i]);
            let rs = trim_leading_zeros(&r[rj..j]);
            let ord = ls.len().cmp(&rs.len()).then_with(|| ls.cmp(rs));
            if ord != Ordering::Equal {
                return ord;
            }
            // equal numeric value: fewer leading zeros first for determinism
            let ord = (i - li).cmp(&(j - rj));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            let ord = l[i].cmp(&r[j]);
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        }
    }
    (l.len() - i).cmp(&(r.len() - j))
}

/// Quality tier of a single word-boundary token, best first.
pub fn token_quality(token: &str) -> Option<u8> {
    const TIERS: &[(&[&str], u8)] = &[
        (&["4K", "UHD", "2160P"], 5),
        (&["QHD", "1440P"], 4),
        (&["FHD", "1080P"], 3),
        (&["HD", "720P"], 2),
        (&["SD", "480P", "576P"], 1),
    ];
    TIERS.iter().find_map(|(tokens, rank)| tokens.iter().any(|t| token.eq_ignore_ascii_case(t)).then_some(*rank))
}

pub fn quality_tokens(value: &str) -> impl Iterator<Item = &str> {
    value.split(|c: char| !c.is_alphanumeric()).filter(|token| !token.is_empty())
}

/// Best quality tier found in `value` (5=UHD/4K .. 1=SD); 0 when no known token is present.
pub fn quality_rank(value: &str) -> u8 { quality_tokens(value).filter_map(token_quality).max().unwrap_or(0) }

// ------------------------------------------------------------
// Generic string concatenation macro with optional capacity hint
// Usage:
//   let s = concat_string!("/", user, "/", pass, "/", id);
//   let s = concat_string!(cap = 128; prefix, "/", value);
// The macro writes all arguments using Display into a preallocated String
// to minimize temporary allocations and copies.
// ------------------------------------------------------------
#[macro_export]
macro_rules! concat_string {
    (cap = $cap:expr; $($arg:expr),* $(,)?) => {{
        let mut s = String::with_capacity($cap);
        $( s.push_str($arg); )*
        s
    }};
    ($($s:expr),+ $(,)?) => {{
        let parts = [$($s),+];
        let cap = parts.iter().map(|s| s.len()).sum();

        let mut out = String::with_capacity(cap);
        for s in parts {
            out.push_str(s);
        }
        out
    }};
}

#[cfg(test)]
mod test {
    use super::{clean_playlist_title, generate_random_string, natural_cmp, quality_rank};
    use crate as shared; // allow path-based macro call in tests
    use crate::utils::Capitalize;
    use std::{cmp::Ordering, collections::HashSet};

    #[test]
    fn test_natural_cmp_basics() {
        assert_eq!(natural_cmp("Chan 2", "Chan 10"), Ordering::Less);
        assert_eq!(natural_cmp("Chan 10", "Chan 2"), Ordering::Greater);
        assert_eq!(natural_cmp("Chan 2", "Chan 2"), Ordering::Equal);
        assert_eq!(natural_cmp("Chan 2", "Chan 02"), Ordering::Less);
        assert_eq!(natural_cmp("abc", "abd"), Ordering::Less);
        assert_eq!(natural_cmp("abc", "abc def"), Ordering::Less);
        assert_eq!(natural_cmp("00", "0"), Ordering::Greater);
    }

    #[test]
    fn test_quality_rank_tokens() {
        assert_eq!(quality_rank("News [UHD]"), 5);
        assert_eq!(quality_rank("News 1080p"), 3);
        assert_eq!(quality_rank("News HD"), 2);
        assert_eq!(quality_rank("News"), 0);
        assert_eq!(quality_rank("HDTV News"), 0); // no partial token match
    }

    #[test]
    fn test_generate_random_string() {
        let mut strings = HashSet::new();
        for _i in 0..100 {
            strings.insert(generate_random_string(5));
        }
        assert_eq!(strings.len(), 100);
    }

    #[test]
    fn test_capitalize() {
        assert_eq!("hELLO".capitalize(), "Hello");
    }

    #[test]
    fn test_concat_string_basic() {
        let a = "hello";
        let b = String::from("world");
        let n = 42;
        let s = shared::concat_string!(a, " ", &b, " ", &n.to_string());
        assert_eq!(s, "hello world 42");
    }

    #[test]
    fn test_concat_string_with_cap() {
        let part = "abc";
        let s = shared::concat_string!(cap = 16; part, "/", &123.to_string());
        assert_eq!(s, "abc/123");
    }

    #[test]
    fn test_clean_playlist_title() {
        assert_eq!(clean_playlist_title("┃DE┃ Movie Title"), "Movie Title");
        assert_eq!(clean_playlist_title("[US] Movie Title"), "Movie Title");
        assert_eq!(clean_playlist_title("|EN| Movie Title"), "Movie Title");
        assert_eq!(clean_playlist_title("(FR) Movie Title"), "Movie Title");
        assert_eq!(clean_playlist_title("[US] (FR) Movie Title"), "Movie Title");
        assert_eq!(clean_playlist_title("Movie Title"), "Movie Title");
    }
}
