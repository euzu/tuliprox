use crate::model::{InputRefreshPolicy, InputType};
use serde::{Deserialize, Serialize};

/// One request-local acquisition before the common target pipeline.
/// Provider actions keep the existing policy domain; Rescan is local discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "policy", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputUpdateAction {
    Provider(InputRefreshPolicy),
    Rescan,
}

/// Stable input identity and its manual action. Target selection stays on the update request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputUpdateRequest {
    pub input_id: u16,
    pub action: InputUpdateAction,
}

/// The single capability contract shared by cards and backend request validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputUpdateCapabilities {
    Manual { policies: &'static [InputRefreshPolicy] },
    Rescan,
    BulkOnly,
    ImportUnavailable,
    Disabled,
}

impl InputUpdateCapabilities {
    #[must_use]
    pub const fn for_input_type(input_type: InputType, enabled: bool) -> Self {
        if !enabled {
            return Self::Disabled;
        }
        match input_type {
            InputType::Xtream | InputType::Stalker | InputType::M3u => Self::Manual {
                policies: &[InputRefreshPolicy::NORMAL, InputRefreshPolicy::REFRESH, InputRefreshPolicy::FORCE],
            },
            InputType::Plex => Self::Manual { policies: &[InputRefreshPolicy::NORMAL, InputRefreshPolicy::REFRESH] },
            InputType::Library => Self::Rescan,
            InputType::Jellyfin | InputType::Emby => Self::ImportUnavailable,
            InputType::M3uBatch | InputType::XtreamBatch | InputType::StalkerBatch | InputType::Staged => {
                Self::BulkOnly
            }
        }
    }

    #[must_use]
    pub const fn policies(self) -> &'static [InputRefreshPolicy] {
        match self {
            Self::Manual { policies } => policies,
            Self::Rescan | Self::BulkOnly | Self::ImportUnavailable | Self::Disabled => &[],
        }
    }

    #[must_use]
    pub fn supports(self, policy: InputRefreshPolicy) -> bool { self.policies().contains(&policy) }

    #[must_use]
    pub fn supports_action(self, action: InputUpdateAction) -> bool {
        match action {
            InputUpdateAction::Provider(policy) => self.supports(policy),
            InputUpdateAction::Rescan => self == Self::Rescan,
        }
    }

    #[must_use]
    pub const fn rebuilds_targets(self) -> bool { matches!(self, Self::Manual { .. } | Self::Rescan) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_update_request_capabilities_cover_all_types_and_never_invent_force() {
        let cases = [
            (InputType::Xtream, 3),
            (InputType::Stalker, 3),
            (InputType::M3u, 3),
            (InputType::Plex, 2),
            (InputType::Library, 0),
            (InputType::Jellyfin, 0),
            (InputType::Emby, 0),
            (InputType::M3uBatch, 0),
            (InputType::XtreamBatch, 0),
            (InputType::StalkerBatch, 0),
            (InputType::Staged, 0),
        ];
        for (input_type, count) in cases {
            let capabilities = InputUpdateCapabilities::for_input_type(input_type, true);
            assert_eq!(capabilities.policies().len(), count, "{input_type}");
            assert_eq!(capabilities.supports(InputRefreshPolicy::FORCE), count == 3);
            assert_eq!(capabilities.supports_action(InputUpdateAction::Rescan), input_type == InputType::Library);
            assert_eq!(InputUpdateCapabilities::for_input_type(input_type, false), InputUpdateCapabilities::Disabled);
        }
    }
}
