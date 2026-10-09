use super::{
    BinaryOperator, CompiledRegex, Filter, ItemField, NumericOperator, PlaylistItemType, PresenceOperator,
    StringOperator, UnaryOperator,
};
use crate::foundation::value_provider::ValueProvider;
use log::{log_enabled, trace, Level};
use std::borrow::Cow;

fn get_caption<'a>(provider: &'a ValueProvider<'_>, rewc: &CompiledRegex) -> (bool, Cow<'a, str>) {
    if let Some(value) = provider.get_filter_value(ItemField::Title) {
        if rewc.re.is_match(&value) {
            return (true, value);
        }
    }

    if let Some(value) = provider.get_filter_value(ItemField::Name) {
        if rewc.re.is_match(&value) {
            return (true, value);
        }
    }
    (false, Cow::Borrowed(""))
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.as_bytes().windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn starts_with_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len() && haystack.as_bytes()[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

fn string_operator_matches(actual: &str, op: StringOperator, value: &str) -> bool {
    match op {
        StringOperator::Eq => actual.eq_ignore_ascii_case(value),
        StringOperator::NotEq => !actual.eq_ignore_ascii_case(value),
        StringOperator::Contains => contains_ignore_ascii_case(actual, value),
        StringOperator::StartsWith => starts_with_ignore_ascii_case(actual, value),
    }
}

/// Caption matches against the title and falls back to the name, mirroring `get_caption`.
fn string_caption_matches(provider: &ValueProvider<'_>, op: StringOperator, value: &str) -> bool {
    let title = provider.get_filter_value(ItemField::Title).unwrap_or(Cow::Borrowed(""));
    let name = provider.get_filter_value(ItemField::Name).unwrap_or(Cow::Borrowed(""));
    match op {
        // NotEq is the negation of Eq-on-either, not "either differs"
        StringOperator::NotEq => !(title.eq_ignore_ascii_case(value) || name.eq_ignore_ascii_case(value)),
        StringOperator::Eq | StringOperator::Contains | StringOperator::StartsWith => {
            string_operator_matches(&title, op, value) || string_operator_matches(&name, op, value)
        }
    }
}

impl Filter {
    pub fn filter(&self, provider: &ValueProvider) -> bool {
        match self {
            Self::FieldComparison(field, rewc) => {
                let (is_match, value) = if field == &ItemField::Caption {
                    get_caption(provider, rewc)
                } else if let Some(value) = provider.get_filter_value(*field) {
                    (rewc.re.is_match(&value), value)
                } else {
                    (false, Cow::Borrowed(""))
                };
                if log_enabled!(Level::Trace) {
                    if is_match {
                        trace!("Match found: {rewc:?} {} => {field}='{value}'", rewc.restr);
                    } else {
                        trace!("Match failed: {self}: {rewc:?} {} => {field}='{value}'", rewc.restr);
                    }
                }
                is_match
            }
            Self::TypeComparison(field, item_type) => {
                let actual = provider.pli.header.item_type;
                let is_match = match item_type {
                    PlaylistItemType::Live => actual.is_live(),
                    PlaylistItemType::Video => actual.is_video(),
                    PlaylistItemType::Series => {
                        actual.is_series()
                            || matches!(actual, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo)
                    }
                    _ => actual == *item_type,
                };
                if log_enabled!(Level::Trace) {
                    if is_match {
                        trace!("Match found: {field:?} {}", actual.as_str());
                    } else {
                        trace!("Match failed: {self}: {field:?} {}", actual.as_str());
                    }
                }
                is_match
            }
            Self::StringComparison(field, op, value) => {
                // Missing values (e.g. absent epgid) are matched as empty strings.
                let is_match = if *field == ItemField::Caption {
                    string_caption_matches(provider, *op, value)
                } else {
                    let actual = provider.get_filter_value(*field).unwrap_or(Cow::Borrowed(""));
                    string_operator_matches(&actual, *op, value)
                };
                if log_enabled!(Level::Trace) {
                    let actual = provider.get_filter_value(*field).unwrap_or(Cow::Borrowed(""));
                    if is_match {
                        trace!("Match found: {field} {op} \"{value}\" => '{actual}'");
                    } else {
                        trace!("Match failed: {field} {op} \"{value}\" => '{actual}'");
                    }
                }
                is_match
            }
            Self::PresenceComparison(field, op) => {
                let is_empty = provider.get_filter_value(*field).is_none_or(|value| value.is_empty());
                match op {
                    PresenceOperator::IsEmpty => is_empty,
                    PresenceOperator::IsNotEmpty => !is_empty,
                }
            }
            Self::NumericComparison(field, op, value) => {
                let actual = match field {
                    ItemField::Chno => provider.pli.header.chno,
                    ItemField::Quality => u32::from(provider.quality_rank()),
                    _ => return false,
                };
                match op {
                    NumericOperator::Eq => actual == *value,
                    NumericOperator::NotEq => actual != *value,
                    NumericOperator::Greater => actual > *value,
                    NumericOperator::GreaterOrEqual => actual >= *value,
                    NumericOperator::Less => actual < *value,
                    NumericOperator::LessOrEqual => actual <= *value,
                }
            }
            Self::SetComparison(field, values) => {
                if *field == ItemField::Caption {
                    let title = provider.get_filter_value(ItemField::Title).unwrap_or(Cow::Borrowed(""));
                    let name = provider.get_filter_value(ItemField::Name).unwrap_or(Cow::Borrowed(""));
                    values.iter().any(|value| title.eq_ignore_ascii_case(value) || name.eq_ignore_ascii_case(value))
                } else {
                    let actual = provider.get_filter_value(*field).unwrap_or(Cow::Borrowed(""));
                    values.iter().any(|value| actual.eq_ignore_ascii_case(value))
                }
            }
            Self::Group(expr) => expr.filter(provider),
            Self::UnaryExpression(op, expr) => match op {
                UnaryOperator::Not => !expr.filter(provider),
            },
            Self::BinaryExpression(left, op, right) => match op {
                BinaryOperator::And => left.filter(provider) && right.filter(provider),
                BinaryOperator::Or => left.filter(provider) || right.filter(provider),
            },
        }
    }
}
