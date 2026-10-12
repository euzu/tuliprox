//! One entry of a target's `watch` list.
//!
//! An entry is either a bare regex string, which watches the channels of
//! matching groups (the only form that existed before), or a map with
//! `pattern` and `on`, which also chooses whether groups appearing or
//! vanishing are reported.
//!
//! The contexts are a `u8` bitset built by `create_bitset!`, like `RoleSet`
//! and `PermissionSet`: no allocation per entry, and "does this entry apply
//! to X" is a `test` instruction in the per-group loop.

use crate::{
    create_bitset,
    error::TuliproxError,
    model::{Prepare, REGEX_CACHE},
};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

create_bitset!(u8, WatchContext, Channels, GroupAdded, GroupRemoved);

/// The config spelling of a [`WatchContext`].
///
/// The macro-generated enum serialises its Rust variant names; the YAML
/// spelling is snake case.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireContext {
    Channels,
    GroupAdded,
    GroupRemoved,
}

const CONTEXTS: [(WatchContext, WireContext); 3] = [
    (WatchContext::Channels, WireContext::Channels),
    (WatchContext::GroupAdded, WireContext::GroupAdded),
    (WatchContext::GroupRemoved, WireContext::GroupRemoved),
];

impl From<WireContext> for WatchContext {
    fn from(context: WireContext) -> Self {
        match context {
            WireContext::Channels => Self::Channels,
            WireContext::GroupAdded => Self::GroupAdded,
            WireContext::GroupRemoved => Self::GroupRemoved,
        }
    }
}

/// The `on` list, read straight into the bitset without an intermediate `Vec`.
struct OnList(WatchContextSet);

impl<'de> Deserialize<'de> for OnList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OnVisitor;

        impl<'de> de::Visitor<'de> for OnVisitor {
            type Value = OnList;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a list of `channels`, `group_added`, `group_removed`")
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut set = WatchContextSet::new();
                while let Some(context) = seq.next_element::<WireContext>()? {
                    set.set(context.into());
                }
                Ok(OnList(set))
            }
        }

        deserializer.deserialize_seq(OnVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWatchDto {
    pub pattern: String,
    /// Never empty once `ConfigTargetDto::prepare` accepted the target.
    pub on: WatchContextSet,
}

const CHANNELS_ONLY: WatchContextSet = WatchContextSet(1 << WatchContext::Channels as u8);

impl ConfigWatchDto {
    /// The short form: watch the channels of matching groups.
    #[must_use]
    pub fn channels(pattern: impl Into<String>) -> Self { Self { pattern: pattern.into(), on: CHANNELS_ONLY } }

    #[must_use]
    pub fn new(pattern: impl Into<String>, on: impl IntoIterator<Item = WatchContext>) -> Self {
        let mut set = WatchContextSet::new();
        for context in on {
            set.set(context);
        }
        Self { pattern: pattern.into(), on: set }
    }

    #[must_use]
    pub fn has(&self, context: WatchContext) -> bool { self.on.contains(context) }
}

/// One entry is usable when it reports at least one context and its pattern
/// compiles. Shared by config load and the web UI editor, so both reject the
/// same entries for the same reason.
impl Prepare for ConfigWatchDto {
    type Ctx<'a> = ();

    fn prepare(&mut self, (): Self::Ctx<'_>) -> Result<(), TuliproxError> {
        if self.on.is_empty() {
            return Err(TuliproxError::ConfigTarget(format!("watch entry '{}' has an empty 'on' list", self.pattern)));
        }
        REGEX_CACHE
            .get_or_compile(&self.pattern)
            .map(|_| ())
            .map_err(|err| TuliproxError::ConfigTarget(format!("Invalid watch regular expression: {err}")))
    }
}

impl Serialize for ConfigWatchDto {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct OnRef(WatchContextSet);

        impl Serialize for OnRef {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_seq(
                    CONTEXTS.iter().filter(|(context, _)| self.0.contains(*context)).map(|(_, wire)| *wire),
                )
            }
        }

        #[derive(Serialize)]
        struct FullFormRef<'a> {
            pattern: &'a str,
            on: OnRef,
        }

        if self.on == CHANNELS_ONLY {
            serializer.serialize_str(&self.pattern)
        } else {
            FullFormRef { pattern: &self.pattern, on: OnRef(self.on) }.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for ConfigWatchDto {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A visitor rather than `#[serde(untagged)]`: untagged reports every
        // mistake in the map form as "did not match any variant", which hides
        // the key the operator mistyped.
        struct WatchVisitor;

        impl<'de> de::Visitor<'de> for WatchVisitor {
            type Value = ConfigWatchDto;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a group regex string or a map with `pattern` and `on`")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ConfigWatchDto::channels(value))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(ConfigWatchDto::channels(value))
            }

            // `Vec<String>` used to accept unquoted YAML scalars such as
            // `- 2024`; keep loading them as patterns.
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(ConfigWatchDto::channels(value.to_string()))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(ConfigWatchDto::channels(value.to_string()))
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(ConfigWatchDto::channels(value.to_string()))
            }

            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct FullForm {
                    pattern: String,
                    #[serde(default)]
                    on: Option<OnList>,
                }

                let full = FullForm::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(ConfigWatchDto { pattern: full.pattern, on: full.on.map_or(CHANNELS_ONLY, |on| on.0) })
            }
        }

        deserializer.deserialize_any(WatchVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigWatchDto, WatchContext, WatchContextSet};
    use crate::model::Prepare;

    fn yaml(src: &str) -> Vec<ConfigWatchDto> { serde_saphyr::from_str(src).expect("valid watch yaml") }

    fn set(contexts: &[WatchContext]) -> WatchContextSet {
        let mut set = WatchContextSet::new();
        for context in contexts {
            set.set(*context);
        }
        set
    }

    #[test]
    fn a_plain_string_watches_channels() {
        let entries = yaml("- \"^DE Sport.*\"\n");
        assert_eq!(entries, vec![ConfigWatchDto::channels("^DE Sport.*")]);
    }

    #[test]
    fn the_object_form_reads_pattern_and_on() {
        let entries = yaml("- pattern: \"^Event.*\"\n  on: [group_added, channels]\n");
        assert_eq!(entries[0].pattern, "^Event.*");
        assert_eq!(entries[0].on, set(&[WatchContext::GroupAdded, WatchContext::Channels]));
    }

    #[test]
    fn the_object_form_without_on_watches_channels() {
        let entries = yaml("- pattern: \"^News\"\n");
        assert_eq!(entries, vec![ConfigWatchDto::channels("^News")]);
    }

    #[test]
    fn duplicate_contexts_collapse() {
        let entries = yaml("- pattern: x\n  on: [group_removed, channels, group_removed]\n");
        assert_eq!(entries[0].on, set(&[WatchContext::GroupRemoved, WatchContext::Channels]));
    }

    #[test]
    fn an_empty_on_list_stays_empty_for_prepare_to_reject() {
        let entries = yaml("- pattern: x\n  on: []\n");
        assert!(entries[0].on.is_empty());
    }

    #[test]
    fn an_unknown_key_is_named_in_the_error() {
        let err = serde_saphyr::from_str::<Vec<ConfigWatchDto>>("- group: \"^Sports\"\n").unwrap_err().to_string();
        assert!(err.contains("group"), "error must name the offending key: {err}");
        assert!(!err.contains("did not match any variant"), "untagged-style error leaked: {err}");
    }

    #[test]
    fn an_unknown_context_is_rejected() {
        assert!(serde_saphyr::from_str::<Vec<ConfigWatchDto>>("- pattern: x\n  on: [groups]\n").is_err());
    }

    #[test]
    fn numeric_and_bool_scalars_still_load_as_patterns() {
        let entries = yaml("- 2024\n- true\n");
        assert_eq!(entries, vec![ConfigWatchDto::channels("2024"), ConfigWatchDto::channels("true")]);
    }

    #[test]
    fn round_trip_keeps_short_form_and_writes_contexts_in_canonical_order() {
        let entries = vec![
            ConfigWatchDto::channels("^A"),
            ConfigWatchDto::new("^B", [WatchContext::GroupRemoved, WatchContext::Channels]),
        ];
        let json = serde_json::to_string(&entries).expect("serialize");
        assert_eq!(json, r#"["^A",{"pattern":"^B","on":["channels","group_removed"]}]"#);
        let back: Vec<ConfigWatchDto> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, entries);
    }

    #[test]
    fn yaml_round_trip_keeps_both_forms() {
        let entries = vec![
            ConfigWatchDto::channels("^A"),
            ConfigWatchDto::new("^B", [WatchContext::GroupAdded, WatchContext::GroupRemoved]),
        ];
        let yaml_text = serde_saphyr::to_string(&entries).expect("serialize yaml");
        assert_eq!(yaml(&yaml_text), entries, "{yaml_text}");
    }

    #[test]
    fn prepare_accepts_a_valid_entry() {
        assert!(ConfigWatchDto::new("^Event.*", [WatchContext::GroupAdded]).prepare(()).is_ok());
    }

    #[test]
    fn prepare_rejects_an_empty_on_list() {
        let err = ConfigWatchDto { pattern: "^A".to_string(), on: WatchContextSet::new() }.prepare(()).unwrap_err();
        assert!(err.to_string().contains("watch entry '^A' has an empty 'on' list"), "{err}");
    }

    #[test]
    fn prepare_rejects_an_invalid_regex() {
        let err = ConfigWatchDto::channels("^(Sport").prepare(()).unwrap_err();
        assert!(err.to_string().contains("Invalid watch regular expression"), "{err}");
    }
}
