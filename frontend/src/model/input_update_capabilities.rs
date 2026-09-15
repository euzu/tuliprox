pub use shared::model::InputUpdateCapabilities;
use shared::model::{ConfigInputDto, InputRefreshPolicy, InputUpdateAction};

/// Presentation adapters for the shared capability contract, without provider decisions.
pub trait InputUpdateCapabilitiesExt {
    fn for_input(input: &ConfigInputDto) -> Self;
    fn selected_action(self, policy: InputRefreshPolicy) -> InputUpdateAction;
    fn description_key(self, policy: InputRefreshPolicy) -> &'static str;
}

impl InputUpdateCapabilitiesExt for InputUpdateCapabilities {
    fn for_input(input: &ConfigInputDto) -> Self { Self::for_input_type(input.input_type, input.enabled) }

    fn selected_action(self, policy: InputRefreshPolicy) -> InputUpdateAction {
        match self {
            Self::Rescan => InputUpdateAction::Rescan,
            Self::Manual { .. } | Self::BulkOnly | Self::ImportUnavailable | Self::Disabled => {
                InputUpdateAction::Provider(policy)
            }
        }
    }

    fn description_key(self, policy: InputRefreshPolicy) -> &'static str {
        match self {
            Self::Manual { .. } if !self.supports(policy) => "MESSAGES.PLAYLIST_UPDATE.MANUAL_UNAVAILABLE",
            Self::Manual { .. } => {
                if policy == InputRefreshPolicy::FORCE {
                    "MESSAGES.PLAYLIST_UPDATE.POLICY_FORCE_DESCRIPTION"
                } else if self.supports(InputRefreshPolicy::FORCE) {
                    if policy == InputRefreshPolicy::REFRESH {
                        "MESSAGES.PLAYLIST_UPDATE.POLICY_REFRESH_DESCRIPTION"
                    } else {
                        "MESSAGES.PLAYLIST_UPDATE.POLICY_UPDATE_DESCRIPTION"
                    }
                } else if policy == InputRefreshPolicy::REFRESH {
                    "MESSAGES.PLAYLIST_UPDATE.CACHE_REFRESH_DESCRIPTION"
                } else {
                    "MESSAGES.PLAYLIST_UPDATE.CACHE_UPDATE_DESCRIPTION"
                }
            }
            Self::Rescan => "MESSAGES.PLAYLIST_UPDATE.LIBRARY_DESCRIPTION",
            Self::BulkOnly => "MESSAGES.PLAYLIST_UPDATE.BULK_ONLY_DESCRIPTION",
            Self::ImportUnavailable => "MESSAGES.PLAYLIST_UPDATE.PROVIDER_IMPORT_UNAVAILABLE",
            Self::Disabled => "MESSAGES.PLAYLIST_UPDATE.INPUT_DISABLED",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::InputType;

    #[test]
    fn input_update_capabilities_uses_shared_contract_for_every_type() {
        for input_type in [
            InputType::Xtream,
            InputType::Stalker,
            InputType::M3u,
            InputType::Plex,
            InputType::Library,
            InputType::Jellyfin,
            InputType::Emby,
            InputType::M3uBatch,
            InputType::XtreamBatch,
            InputType::StalkerBatch,
            InputType::Staged,
        ] {
            for enabled in [false, true] {
                let input = ConfigInputDto { input_type, enabled, ..ConfigInputDto::default() };
                assert_eq!(
                    InputUpdateCapabilities::for_input(&input),
                    InputUpdateCapabilities::for_input_type(input_type, enabled)
                );
            }
        }
    }

    #[test]
    fn input_update_capabilities_rescan_is_not_a_provider_policy_and_uses_targets() {
        let capability = InputUpdateCapabilities::Rescan;
        assert!(capability.policies().is_empty());
        assert!(capability.rebuilds_targets());
        assert_eq!(capability.selected_action(InputRefreshPolicy::NORMAL), InputUpdateAction::Rescan);
        for unavailable in [
            InputUpdateCapabilities::BulkOnly,
            InputUpdateCapabilities::ImportUnavailable,
            InputUpdateCapabilities::Disabled,
        ] {
            assert!(!unavailable.rebuilds_targets());
        }
    }

    #[test]
    fn input_update_capabilities_notes_are_translated_and_explain_complete_rescan() -> Result<(), serde_json::Error> {
        for source in [
            include_str!("../../public/assets/i18n/en.json"),
            include_str!("../../public/assets/i18n/ru.json"),
            include_str!("../../public/assets/i18n/ar.json"),
        ] {
            let locale: serde_json::Value = serde_json::from_str(source)?;
            for key in ["LIBRARY_DESCRIPTION", "PROVIDER_IMPORT_UNAVAILABLE", "BULK_ONLY_DESCRIPTION"] {
                assert!(locale["MESSAGES"]["PLAYLIST_UPDATE"][key].as_str().is_some_and(|text| !text.is_empty()));
            }
            for key in ["RESCAN", "START_RESCAN"] {
                assert!(locale["LABEL"][key].as_str().is_some_and(|text| !text.is_empty()));
            }
        }
        let en: serde_json::Value = serde_json::from_str(include_str!("../../public/assets/i18n/en.json"))?;
        assert!(en["MESSAGES"]["PLAYLIST_UPDATE"]["LIBRARY_DESCRIPTION"]
            .as_str()
            .unwrap()
            .contains("selected targets"));
        Ok(())
    }
}
