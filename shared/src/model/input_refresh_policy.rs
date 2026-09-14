use crate::model::{InputUpdateAction, InputUpdateRequest};
use serde::{Deserialize, Serialize};

/// Whether a manual playlist update may reuse a valid input cache entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheReadPolicy {
    #[default]
    Respect,
    Bypass,
}

/// Whether a manual playlist update applies the configured quality thresholds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateQualityPolicy {
    #[default]
    Enforce,
    Bypass,
}

/// Request-local cache and quality behavior for one input update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRefreshPolicy {
    pub cache: CacheReadPolicy,
    pub quality: UpdateQualityPolicy,
}

impl InputRefreshPolicy {
    pub const NORMAL: Self = Self { cache: CacheReadPolicy::Respect, quality: UpdateQualityPolicy::Enforce };
    pub const REFRESH: Self = Self { cache: CacheReadPolicy::Bypass, quality: UpdateQualityPolicy::Enforce };
    pub const FORCE: Self = Self { cache: CacheReadPolicy::Bypass, quality: UpdateQualityPolicy::Bypass };

    #[must_use]
    pub const fn bypasses_cache(self) -> bool { matches!(self.cache, CacheReadPolicy::Bypass) }

    #[must_use]
    pub const fn bypasses_quality(self) -> bool { matches!(self.quality, UpdateQualityPolicy::Bypass) }
}

impl Default for InputRefreshPolicy {
    fn default() -> Self { Self::NORMAL }
}

/// Applies a refresh policy to one configured input without changing its persisted configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRefreshOverride {
    pub input_id: u16,
    pub policy: InputRefreshPolicy,
}

/// Current request body for a manual playlist update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistUpdateRequestDto {
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ids: Option<Vec<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_refresh: Option<InputRefreshOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_action: Option<InputUpdateRequest>,
}

impl PlaylistUpdateRequestDto {
    /// Resolves the legacy policy field without permitting two conflicting actions.
    pub fn manual_input_update(&self) -> Result<Option<InputUpdateRequest>, &'static str> {
        match (self.input_action, self.input_refresh) {
            (Some(_), Some(_)) => Err("Specify either input_action or input_refresh, not both"),
            (Some(action), None) => Ok(Some(action)),
            (None, Some(refresh)) => Ok(Some(InputUpdateRequest {
                input_id: refresh.input_id,
                action: InputUpdateAction::Provider(refresh.policy),
            })),
            (None, None) => Ok(None),
        }
    }
}

/// Accepted wire formats for the update endpoint.
///
/// The legacy target-name array remains valid so existing API clients keep
/// their pre-policy behavior.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum PlaylistUpdateRequestPayload {
    Current(PlaylistUpdateRequestDto),
    LegacyTargets(Vec<String>),
}

impl PlaylistUpdateRequestPayload {
    #[must_use]
    pub fn into_request(self) -> PlaylistUpdateRequestDto {
        match self {
            Self::Current(request) => request,
            Self::LegacyTargets(targets) => PlaylistUpdateRequestDto { targets, ..PlaylistUpdateRequestDto::default() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_policies_have_the_required_cache_and_quality_semantics() {
        assert_eq!(InputRefreshPolicy::NORMAL.cache, CacheReadPolicy::Respect);
        assert_eq!(InputRefreshPolicy::NORMAL.quality, UpdateQualityPolicy::Enforce);
        assert_eq!(InputRefreshPolicy::REFRESH.cache, CacheReadPolicy::Bypass);
        assert_eq!(InputRefreshPolicy::REFRESH.quality, UpdateQualityPolicy::Enforce);
        assert_eq!(InputRefreshPolicy::FORCE.cache, CacheReadPolicy::Bypass);
        assert_eq!(InputRefreshPolicy::FORCE.quality, UpdateQualityPolicy::Bypass);
    }

    #[test]
    fn playlist_update_request_with_target_ids_round_trips_without_mutating_policy() -> Result<(), serde_json::Error> {
        let request = PlaylistUpdateRequestDto {
            targets: Vec::new(),
            target_ids: Some(vec![4, 1]),
            input_refresh: Some(InputRefreshOverride { input_id: 17, policy: InputRefreshPolicy::FORCE }),
            input_action: None,
        };

        let encoded = serde_json::to_string(&request)?;
        let decoded = serde_json::from_str::<PlaylistUpdateRequestDto>(&encoded)?;

        assert_eq!(decoded, request);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded)?,
            serde_json::json!({
                "targets": [],
                "target_ids": [4, 1],
                "input_refresh": {
                    "input_id": 17,
                    "policy": {"cache": "bypass", "quality": "bypass"}
                }
            })
        );
        Ok(())
    }

    #[test]
    fn playlist_update_request_distinguishes_missing_and_explicitly_empty_target_ids() -> Result<(), serde_json::Error>
    {
        let missing = serde_json::from_str::<PlaylistUpdateRequestDto>(r#"{"targets":[]}"#)?;
        let explicit_empty = serde_json::from_str::<PlaylistUpdateRequestDto>(r#"{"targets":[],"target_ids":[]}"#)?;

        assert_eq!(missing.target_ids, None);
        assert_eq!(explicit_empty.target_ids, Some(Vec::new()));
        Ok(())
    }

    #[test]
    fn playlist_update_request_named_targets_remain_backward_compatible() -> Result<(), serde_json::Error> {
        let request = serde_json::from_str::<PlaylistUpdateRequestDto>(r#"{"targets":["family"]}"#)?;

        assert_eq!(request.targets, vec!["family"]);
        assert_eq!(request.target_ids, None);
        assert_eq!(request.input_refresh, None);
        Ok(())
    }

    #[test]
    fn playlist_update_request_legacy_target_array_maps_to_normal_behavior() -> Result<(), serde_json::Error> {
        let payload = serde_json::from_str::<PlaylistUpdateRequestPayload>(r#"["family","mobile"]"#)?;

        assert_eq!(
            payload.into_request(),
            PlaylistUpdateRequestDto {
                targets: vec!["family".to_string(), "mobile".to_string()],
                target_ids: None,
                input_refresh: None,
                input_action: None,
            }
        );
        Ok(())
    }

    #[test]
    fn playlist_update_request_rescan_round_trips_and_rejects_competing_policy() -> Result<(), serde_json::Error> {
        let mut request = PlaylistUpdateRequestDto {
            target_ids: Some(vec![20, 21]),
            input_action: Some(InputUpdateRequest { input_id: 2, action: InputUpdateAction::Rescan }),
            ..PlaylistUpdateRequestDto::default()
        };
        let encoded = serde_json::to_string(&request)?;
        assert_eq!(serde_json::from_str::<PlaylistUpdateRequestDto>(&encoded)?, request);
        assert_eq!(request.manual_input_update(), Ok(request.input_action));
        request.input_refresh = Some(InputRefreshOverride { input_id: 2, policy: InputRefreshPolicy::FORCE });
        assert!(request.manual_input_update().is_err());
        assert_eq!(PlaylistUpdateRequestDto::default().manual_input_update(), Ok(None));
        Ok(())
    }
}
