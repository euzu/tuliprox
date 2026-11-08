use super::PluginRef;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedChannelStage {
    BeforeFilter,
    #[default]
    AfterFilter,
}

impl ManagedChannelStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeFilter => "before_filter",
            Self::AfterFilter => "after_filter",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedChannelBinding {
    pub plugin: PluginRef,
    #[serde(default)]
    pub stage: ManagedChannelStage,
}

pub fn validate_managed_channel_refs<'a>(plugins: impl IntoIterator<Item = &'a PluginRef>) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for plugin in plugins {
        if !seen.insert(plugin.id()) {
            return Err(format!("duplicate managed channel binding for {}", plugin.id()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_bindings_default_after_filter_and_round_trip_without_losing_input_bindings() {
        let target: crate::model::ConfigTargetDto =
            serde_json::from_value(serde_json::json!({"name":"TV","managed_channels":[{"plugin":"plugin:bridge"}]}))
                .unwrap();
        assert_eq!(target.managed_channels[0].stage, ManagedChannelStage::AfterFilter);
        let input: crate::model::ConfigInputDto =
            serde_json::from_value(serde_json::json!({"name":"provider","managed_channels":["plugin:bridge"]}))
                .unwrap();
        let restored: crate::model::ConfigInputDto =
            serde_json::from_str(&serde_json::to_string(&input).unwrap()).unwrap();
        assert_eq!(restored.managed_channels, input.managed_channels);
        let restored: crate::model::ConfigTargetDto =
            serde_json::from_str(&serde_json::to_string(&target).unwrap()).unwrap();
        assert_eq!(restored.managed_channels, target.managed_channels);
    }
    #[test]
    fn duplicate_or_invalid_stage_bindings_are_rejected() {
        let plugin = PluginRef::new("bridge").unwrap();
        assert!(validate_managed_channel_refs([&plugin, &plugin]).is_err());
        assert!(serde_json::from_value::<ManagedChannelBinding>(
            serde_json::json!({"plugin":"plugin:bridge","stage":"after_map"})
        )
        .is_err());
    }
}
