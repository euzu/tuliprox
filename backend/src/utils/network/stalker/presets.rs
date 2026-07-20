use shared::model::stalker::StalkerMagPreset;

use crate::model::StalkerDeviceProfile;

/// The fingerprint values for a single MAG device preset. The portal uses these to
/// distinguish "this is a real MAG device" from "this is a script" — the values
/// here mirror the values emitted by real firmware. The `token_header` is the name of
/// the `Authorization` scheme the portal expects from this preset (Bearer vs MAC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalkerMagPresetSpec {
    pub preset: StalkerMagPreset,
    pub user_agent: &'static str,
    pub x_user_agent: &'static str,
    pub token_header: &'static str,
    /// Whether the portal expects us to send a `Mac` query parameter on every call.
    pub emit_mac_query: bool,
    /// Whether the portal expects a `token` query parameter alongside the bearer header.
    pub emit_token_query: bool,
    /// Default device model label embedded in the `device_profile` field.
    pub default_device_model: &'static str,
}

pub fn stalker_mag_preset_spec(preset: StalkerMagPreset) -> StalkerMagPresetSpec {
    match preset {
        StalkerMagPreset::GenericSafe => StalkerMagPresetSpec {
            preset,
            user_agent: "Mozilla/5.0 (QtEmbedded; U; Linux; C) AppleWebKit/533.3 (KHTML, like Gecko) MAG200 stbapp ver: 2 rev: 250 Safari/533.3",
            x_user_agent: "Model: MAG250; Link: Ethernet",
            token_header: "Bearer",
            emit_mac_query: true,
            emit_token_query: false,
            default_device_model: "MAG250",
        },
        StalkerMagPreset::Mag250Legacy => StalkerMagPresetSpec {
            preset,
            user_agent: "Mozilla/5.0 (QtEmbedded; U; Linux; C) AppleWebKit/533.3 (KHTML, like Gecko) MAG200 stbapp ver: 0.2.16 rev: 250; Embedded_Series; MAC: 00:1A:79:XX:XX:XX Safari/533.3",
            x_user_agent: "Model: MAG250; Link: Ethernet",
            token_header: "Bearer",
            emit_mac_query: true,
            emit_token_query: false,
            default_device_model: "MAG250",
        },
        StalkerMagPreset::Mag254Strict => StalkerMagPresetSpec {
            preset,
            user_agent: "Mozilla/5.0 (QtEmbedded; U; Linux; C) AppleWebKit/533.3 (KHTML, like Gecko) MAG200 stbapp ver: 0.2.18 rev: 254; Embedded_Series; MAC: 00:1A:79:XX:XX:XX Safari/533.3",
            x_user_agent: "Model: MAG254; Link: Ethernet",
            token_header: "Bearer",
            emit_mac_query: true,
            emit_token_query: true,
            default_device_model: "MAG254",
        },
        StalkerMagPreset::MinistraModern => StalkerMagPresetSpec {
            preset,
            user_agent: "Mozilla/5.0 (QtEmbedded; U; Linux; C) AppleWebKit/533.3 (KHTML, like Gecko) MAG200 stbapp ver: 0.2.21-3.10 (miniAPI; treat as MAG322); Linux; MAC: 00:1A:79:XX:XX:XX Safari/533.3",
            x_user_agent: "Model: MAG322; Link: Ethernet; API: 3.10",
            token_header: "Bearer",
            emit_mac_query: false,
            emit_token_query: false,
            default_device_model: "MAG322",
        },
    }
}

/// Merge the user-supplied `device_profile` with the preset defaults. We never overwrite
/// a field the user explicitly set; we only fill in blanks. This means a user that
/// provides only a MAC address still ends up with a fully populated profile.
pub fn merge_profile_with_preset(
    mut profile: StalkerDeviceProfile,
    preset: StalkerMagPreset,
) -> StalkerDeviceProfile {
    let spec = stalker_mag_preset_spec(preset);
    if profile.user_agent.as_deref().map(str::trim).is_none_or(str::is_empty) {
        profile.user_agent = Some(spec.user_agent.to_string());
    }
    if profile.x_user_agent.as_deref().map(str::trim).is_none_or(str::is_empty) {
        profile.x_user_agent = Some(spec.x_user_agent.to_string());
    }
    if profile.device_profile.as_deref().map(str::trim).is_none_or(str::is_empty) {
        profile.device_profile = Some(spec.default_device_model.to_string());
    }
    if profile.locale.as_deref().map(str::trim).is_none_or(str::is_empty) {
        profile.locale = Some("en".to_string());
    }
    if profile.timezone.as_deref().map(str::trim).is_none_or(str::is_empty) {
        profile.timezone = Some("UTC".to_string());
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_PRESETS: [StalkerMagPreset; 4] = [
        StalkerMagPreset::GenericSafe,
        StalkerMagPreset::Mag250Legacy,
        StalkerMagPreset::Mag254Strict,
        StalkerMagPreset::MinistraModern,
    ];

    #[test]
    fn presets_have_distinct_user_agents() {
        let mut agents: Vec<String> = ALL_PRESETS
            .into_iter()
            .map(|p| {
                let spec = stalker_mag_preset_spec(p);
                spec.user_agent.to_string()
            })
            .collect();
        agents.sort();
        agents.dedup();
        assert_eq!(agents.len(), 4, "each MAG preset must ship a distinct UA");
    }

    #[test]
    fn merge_keeps_user_overrides() {
        let profile = StalkerDeviceProfile {
            mac_address: Some("00:1A:79:01:02:03".to_string()),
            device_profile: Some("MyCustomBox".to_string()),
            user_agent: Some("custom-ua".to_string()),
            x_user_agent: Some("custom-x-ua".to_string()),
            locale: Some("fr".to_string()),
            timezone: Some("Europe/Paris".to_string()),
            ..StalkerDeviceProfile::default()
        };
        let merged = merge_profile_with_preset(profile, StalkerMagPreset::Mag254Strict);
        assert_eq!(merged.device_profile.as_deref(), Some("MyCustomBox"));
        assert_eq!(merged.user_agent.as_deref(), Some("custom-ua"));
        assert_eq!(merged.x_user_agent.as_deref(), Some("custom-x-ua"));
        assert_eq!(merged.locale.as_deref(), Some("fr"));
        assert_eq!(merged.timezone.as_deref(), Some("Europe/Paris"));
    }

    #[test]
    fn merge_fills_blanks() {
        let profile = StalkerDeviceProfile {
            mac_address: Some("00:1A:79:01:02:03".to_string()),
            ..StalkerDeviceProfile::default()
        };
        let merged = merge_profile_with_preset(profile, StalkerMagPreset::MinistraModern);
        let spec = stalker_mag_preset_spec(StalkerMagPreset::MinistraModern);
        assert_eq!(merged.device_profile.as_deref(), Some(spec.default_device_model));
        assert_eq!(merged.user_agent.as_deref(), Some(spec.user_agent));
        assert_eq!(merged.x_user_agent.as_deref(), Some(spec.x_user_agent));
        assert_eq!(merged.locale.as_deref(), Some("en"));
        assert_eq!(merged.timezone.as_deref(), Some("UTC"));
    }
}
