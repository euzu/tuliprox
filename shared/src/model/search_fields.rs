use regex::Regex;
use std::collections::HashMap;
use strum_macros::{AsRefStr, EnumIter, EnumString};

/// Searchable fields of a stream history record. Single source of truth for
/// the REST API parser and the Web UI field dropdown.
#[derive(Debug, Copy, Clone, Eq, PartialEq, EnumIter, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum StreamHistorySearchField {
    EventTsUtc,
    EventType,
    Title,
    Group,
    ApiUsername,
    ProviderName,
    ProviderId,
    BytesSent,
    FirstByteLatencyMs,
    UserAgent,
    ItemType,
    Container,
    DisconnectReason,
    SourceAddr,
    Country,
    Cluster,
}

/// Value type a filterable field accepts.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SearchFieldKind {
    Text,
    Numeric,
}

/// A compiled filter operand: exact text (case-insensitive), regex
/// (`~pattern` syntax) or exact numeric match.
#[derive(Debug)]
pub enum CompiledFieldValue {
    Exact(String),
    Regex(Regex),
    NumericExact(u64),
}

impl CompiledFieldValue {
    pub fn matches_text(&self, value: &str) -> bool {
        match self {
            Self::Exact(expected) => value.eq_ignore_ascii_case(expected),
            Self::Regex(re) => re.is_match(value),
            Self::NumericExact(_) => false,
        }
    }

    pub fn matches_numeric(&self, value: u64) -> bool {
        match self {
            Self::NumericExact(expected) => value == *expected,
            Self::Exact(_) | Self::Regex(_) => false,
        }
    }
}

/// Field-name → operand filter compiled from raw query parameters.
/// Field resolution is supplied by the caller, so the same operator syntax
/// works for any record type.
#[derive(Debug, Default)]
pub struct FieldFilter {
    fields: Vec<(String, CompiledFieldValue)>,
}

impl FieldFilter {
    /// `kind_of` returns the value kind for a known field name, `None` for
    /// unknown fields (which are rejected).
    pub fn compile(
        raw: &HashMap<String, String>,
        kind_of: impl Fn(&str) -> Option<SearchFieldKind>,
    ) -> Result<Self, String> {
        let mut fields = Vec::with_capacity(raw.len());
        for (key, value) in raw {
            let kind = kind_of(key).ok_or_else(|| format!("Unknown filter field: '{key}'"))?;
            let compiled = match kind {
                SearchFieldKind::Numeric => {
                    let parsed = value
                        .parse::<u64>()
                        .map_err(|_| format!("Filter '{key}' expects a numeric value, got '{value}'"))?;
                    CompiledFieldValue::NumericExact(parsed)
                }
                SearchFieldKind::Text => {
                    if let Some(pattern) = value.strip_prefix('~') {
                        let re =
                            Regex::new(pattern).map_err(|err| format!("Invalid regex for filter '{key}': {err}"))?;
                        CompiledFieldValue::Regex(re)
                    } else {
                        CompiledFieldValue::Exact(value.clone())
                    }
                }
            };
            fields.push((key.clone(), compiled));
        }
        Ok(Self { fields })
    }

    pub fn is_empty(&self) -> bool { self.fields.is_empty() }

    /// True when every filter entry matches; `resolve` maps a field name and
    /// its compiled operand onto the concrete record.
    pub fn matches(&self, resolve: impl Fn(&str, &CompiledFieldValue) -> bool) -> bool {
        self.fields.iter().all(|(key, value)| resolve(key, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn search_field_parses_snake_case_ids() {
        assert_eq!(
            StreamHistorySearchField::from_str("event_ts_utc").expect("known field parses"),
            StreamHistorySearchField::EventTsUtc
        );
        assert_eq!(
            StreamHistorySearchField::from_str("first_byte_latency_ms").expect("known field parses"),
            StreamHistorySearchField::FirstByteLatencyMs
        );
        assert!(StreamHistorySearchField::from_str("bogus").is_err());
    }

    #[test]
    fn search_field_round_trips_through_as_ref() {
        use strum::IntoEnumIterator;
        for field in StreamHistorySearchField::iter() {
            let id: &str = field.as_ref();
            assert_eq!(StreamHistorySearchField::from_str(id).expect("id round-trips"), field);
        }
    }

    #[test]
    fn field_filter_rejects_unknown_fields() {
        let mut raw = HashMap::new();
        raw.insert("unknown".to_string(), "value".to_string());
        assert!(FieldFilter::compile(&raw, |_| None).is_err());
    }

    #[test]
    fn field_filter_compiles_text_regex_and_numeric_operands() {
        let mut raw = HashMap::new();
        raw.insert("title".to_string(), "~^News".to_string());
        raw.insert("session_id".to_string(), "42".to_string());
        raw.insert("group".to_string(), "Sports".to_string());
        let filter = FieldFilter::compile(&raw, |key| match key {
            "session_id" => Some(SearchFieldKind::Numeric),
            "title" | "group" => Some(SearchFieldKind::Text),
            _ => None,
        })
        .expect("valid filter compiles");

        let matched = filter.matches(|key, value| match key {
            "title" => value.matches_text("News at 10"),
            "group" => value.matches_text("sports"),
            "session_id" => value.matches_numeric(42),
            _ => false,
        });
        assert!(matched);

        let mismatched = filter.matches(|key, value| match key {
            "title" => value.matches_text("Movies"),
            "group" => value.matches_text("sports"),
            "session_id" => value.matches_numeric(42),
            _ => false,
        });
        assert!(!mismatched);
    }
}
