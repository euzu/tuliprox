use shared::{
    model::EpgNamePrefix,
    utils::{deunicode_string, CONSTANTS},
};
use tuliprox_core::model::EpgSmartMatchConfig;

/// Splits a string at the first delimiter if the prefix matches a known country code.
///
/// Returns a tuple containing the country code prefix (if found) and the remainder of the string, both trimmed. If no valid prefix is found, returns `None` and the original input.
///
/// # Examples
///
/// ```text
/// let delimiters = vec!['.', '-', '_'];
/// let (prefix, rest) = split_by_first_match("US.HBO", &delimiters);
/// assert_eq!(prefix, Some("US"));
/// assert_eq!(rest, "HBO");
///
/// let (prefix, rest) = split_by_first_match("HBO", &delimiters);
/// assert_eq!(prefix, None);
/// assert_eq!(rest, "HBO");
/// ```
fn split_by_first_match<'a>(input: &'a str, delimiters: &[char]) -> (Option<&'a str>, &'a str) {
    let content = input.trim_start_matches(|c: char| !c.is_alphanumeric());

    for delim in delimiters {
        if let Some(index) = content.find(*delim) {
            let (left, right) = content.split_at(index);
            let right = &right[delim.len_utf8()..].trim();
            if !right.is_empty() {
                let prefix = left.trim();
                if CONSTANTS.country_codes.contains(&prefix) {
                    return (Some(prefix), right.trim());
                }
            }
        }
    }
    (None, input)
}

pub(super) fn name_prefix<'a>(name: &'a str, smart_config: &EpgSmartMatchConfig) -> (&'a str, Option<&'a str>) {
    if smart_config.name_prefix != EpgNamePrefix::Ignore {
        let (prefix, suffix) = split_by_first_match(name, &smart_config.name_prefix_separator);
        if prefix.is_some() {
            return (suffix, prefix);
        }
    }
    (name, None)
}

fn combine(join: &str, left: &str, right: &str) -> String {
    let mut combined = String::with_capacity(left.len() + join.len() + right.len());
    combined.push_str(left);
    combined.push_str(join);
    combined.push_str(right);
    combined
}

pub(super) fn strip_markers(input: &str, markers: &[String]) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while cursor < input.len() {
        let left_is_boundary = cursor == 0 || !input.as_bytes()[cursor - 1].is_ascii_alphanumeric();
        let matched_end = left_is_boundary
            .then(|| {
                markers.iter().filter(|marker| !marker.is_empty()).find_map(|marker| {
                    let end = cursor + marker.len();
                    let candidate = input.get(cursor..end)?;
                    (candidate.eq_ignore_ascii_case(marker)
                        && input.as_bytes().get(end).is_none_or(|byte| !byte.is_ascii_alphanumeric()))
                    .then_some(end)
                })
            })
            .flatten();
        if let Some(end) = matched_end {
            cursor = end;
        } else {
            let next = input[cursor..].chars().next().expect("cursor is within the string");
            output.push(next);
            cursor += next.len_utf8();
        }
    }
    output
}

/// # Panics
pub fn normalize_channel_name(name: &str, normalize_config: &EpgSmartMatchConfig) -> String {
    let normalized = deunicode_string(name.trim()).to_lowercase();
    let (channel_name, suffix) = name_prefix(&normalized, normalize_config);
    let stripped_name = strip_markers(channel_name, &normalize_config.strip);
    let reconstructed = match suffix {
        None => stripped_name,
        Some(sfx) => match &normalize_config.name_prefix {
            EpgNamePrefix::Ignore => stripped_name,
            EpgNamePrefix::Suffix(separator) => combine(separator, &stripped_name, sfx),
            EpgNamePrefix::Prefix(separator) => combine(separator, sfx, &stripped_name),
        },
    };
    normalize_config.normalize_regex.replace_all(&reconstructed, "").into_owned()
}
