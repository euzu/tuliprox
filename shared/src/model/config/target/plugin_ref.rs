use crate::model::is_valid_plugin_id;
use std::{
    fmt::{Display, Formatter},
    str::FromStr,
};

/// The value of a per-target plugin strategy (`sort.strategy`,
/// `options.deduplicate.keep`, `chno`, `group_merge`): the string
/// `plugin:<id>`.
///
/// Only the syntax is checked here. Plugins load after the config, so whether
/// `<id>` names an enabled plugin that runs the hook is validated against the
/// registry right after plugin load (see `ConfigTarget::plugin_refs`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginRef(String);

impl PluginRef {
    const PREFIX: &'static str = "plugin:";

    /// The plugin id, without the `plugin:` prefix.
    #[must_use]
    pub fn id(&self) -> &str { &self.0 }

    /// A reference to the plugin `id`, which must be a valid plugin id.
    ///
    /// # Errors
    /// `id` is not a valid plugin id (`^[a-z][a-z0-9-]{2,40}$`).
    pub fn new(id: &str) -> Result<Self, String> {
        if is_valid_plugin_id(id) {
            Ok(Self(id.to_string()))
        } else {
            Err(format!("`{id}` is not a valid plugin id (expected ^[a-z][a-z0-9-]{{2,40}}$)"))
        }
    }
}

impl FromStr for PluginRef {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        let value = value.trim();
        value
            .strip_prefix(Self::PREFIX)
            .ok_or_else(|| format!("`{value}` is not a plugin reference (expected `plugin:<id>`)"))
            .and_then(|id| Self::new(id.trim()))
    }
}

impl Display for PluginRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result { write!(f, "{}{}", Self::PREFIX, self.0) }
}

impl serde::Serialize for PluginRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> { serializer.collect_str(self) }
}

impl<'de> serde::Deserialize<'de> for PluginRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_plugin_spelling_and_round_trips() {
        let plugin: PluginRef = "plugin:my-sorter".parse().unwrap();
        assert_eq!(plugin.id(), "my-sorter");
        assert_eq!(plugin.to_string(), "plugin:my-sorter");
        assert_eq!(serde_json::to_string(&plugin).unwrap(), "\"plugin:my-sorter\"");
        assert_eq!(serde_json::from_str::<PluginRef>("\"plugin:my-sorter\"").unwrap(), plugin);
    }

    #[test]
    fn rejects_other_spellings_and_invalid_ids() {
        for bad in ["my-sorter", "plugin:", "plugin:A", "plugin:ab", "plugin:has space", "builtin:best_quality"] {
            assert!(bad.parse::<PluginRef>().is_err(), "{bad}");
        }
    }
}
