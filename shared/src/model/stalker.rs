use crate::utils::{is_blank_optional_string, is_false};
use serde::{Deserialize, Serialize};
use strum_macros::{AsRefStr, Display, EnumIter, EnumString};

/// Authentication mode negotiated with the Stalker/Ministra portal.
#[derive(
    Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Default, EnumIter, Display, EnumString, AsRefStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StalkerAuthMode {
    /// Library will pick the most appropriate mode based on the configured
    /// username/password/MAC presence.
    #[default]
    Auto,
    /// MAC only — portal issues token without account credentials.
    MacOnly,
    /// Account username/password only.
    CredentialsOnly,
    /// MAC + account username/password.
    MacPlusCredentials,
}

impl StalkerAuthMode {
    #[inline]
    pub fn is_default(value: &StalkerAuthMode) -> bool { matches!(value, Self::Auto) }
}

/// Pre-baked MAG device profiles. The values are derived from
/// `StalkerPortalRecipes.kt::stalkerMagPresetSpec`.
#[derive(
    Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Default, EnumIter, Display, EnumString, AsRefStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StalkerMagPreset {
    /// Safe default that works against the widest variety of portals.
    #[default]
    GenericSafe,
    /// Legacy MAG250 identity (firmware 0.2.16).
    Mag250Legacy,
    /// Strict MAG254 identity (firmware 0.2.18).
    Mag254Strict,
    /// Modern Ministra/MAG322 identity (firmware 0.2.21).
    MinistraModern,
}

impl StalkerMagPreset {
    #[inline]
    pub fn is_default(value: &StalkerMagPreset) -> bool { matches!(value, Self::GenericSafe) }
}

/// User override for sibling endpoint selection (`server/load.php` vs `portal.php`).
#[derive(
    Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Default, EnumIter, Display, EnumString, AsRefStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StalkerEndpointPreference {
    #[default]
    Auto,
    ServerLoad,
    Portal,
}

impl StalkerEndpointPreference {
    #[inline]
    pub fn is_default(value: &StalkerEndpointPreference) -> bool { matches!(value, Self::Auto) }
}

/// Playback strategy that the resolved stream URL supports. The chosen mode drives
/// reverse-proxy headers and the create_link fallback chain.
#[derive(
    Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Default, EnumIter, Display, EnumString, AsRefStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StalkerPlaybackMode {
    #[default]
    DirectUrl,
    LocalhostCmd,
    PlayLivePortal,
    PlayMoviePortal,
    TempLinkNginx,
    TempLinkFlussonic,
    TempLinkWowza,
}

/// Recipe — a tuple of MAG preset + auth mode + endpoint preference + flavour flags.
/// Drives the handshake fallback chain in `authenticate`.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, EnumIter, Display, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StalkerBootstrapRecipe {
    GenericSafe,
    LegacyMag,
    StrictMag,
    PortalPreferred,
    LocalizationStrict,
    AuthOnly,
    AuthStrictMag,
    ModuleGated,
}

/// Detected portal flavour used to pick the next fallback recipe.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, EnumIter, Display, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StalkerPortalFingerprint {
    BasicMac,
    StrictMag,
    TempLinkStrict,
    AuthOnly,
    AuthStrictMag,
    ModuleGated,
}

/// Order in which the handshake/handshake-extra requests are issued.
#[derive(
    Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Default, EnumIter, Display, EnumString, AsRefStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StalkerBootstrapStrategy {
    #[default]
    Auto,
    MacOnly,
    MacWithAccountInfo,
    MacWithModules,
}

/// Stream classification used by the playlist iterator to switch between
/// `create_link` parameter sets.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, EnumIter, Display, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StalkerStreamKind {
    Live,
    Archive,
    Movie,
    Episode,
}

impl StalkerStreamKind {
    pub fn as_path_segment(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Archive => "archive",
            Self::Movie => "movie",
            Self::Episode => "episode",
        }
    }
}

impl StalkerStreamKind {
    pub fn parse_path_segment(segment: &str) -> Option<Self> {
        match segment.to_ascii_lowercase().as_str() {
            "live" => Some(Self::Live),
            "archive" => Some(Self::Archive),
            "movie" => Some(Self::Movie),
            "episode" => Some(Self::Episode),
            _ => None,
        }
    }
}

/// Optional body cap per HTTP action. Stored inside the runtime config so the
/// caps can be tuned without recompiling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StalkerActionSizeCapDto {
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub create_link_kb: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub ordered_list_mb: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub get_epg_mb: u32,
}

impl Default for StalkerActionSizeCapDto {
    fn default() -> Self { Self { create_link_kb: 64, ordered_list_mb: 8, get_epg_mb: 64 } }
}

impl StalkerActionSizeCapDto {
    pub fn is_default(&self) -> bool { self.create_link_kb == 64 && self.ordered_list_mb == 8 && self.get_epg_mb == 64 }

    pub fn clean(&mut self) {
        self.create_link_kb = 64;
        self.ordered_list_mb = 8;
        self.get_epg_mb = 64;
    }
}

const fn is_zero_u32(v: &u32) -> bool { *v == 0 }

/// Stalker device identity (MAC + derived hashes). When the user does not
/// override the derived fields, the library fills them during `prepare`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct StalkerDeviceProfileDto {
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub mac_address: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub device_profile: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub serial_number: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub device_id2: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub x_user_agent: Option<String>,
}

impl StalkerDeviceProfileDto {
    pub fn is_empty(&self) -> bool {
        self.mac_address.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.device_profile.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.serial_number.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.device_id.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.device_id2.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.signature.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.timezone.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.locale.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.user_agent.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.x_user_agent.as_deref().map(str::trim).is_none_or(str::is_empty)
    }

    pub fn clean(&mut self) {
        self.mac_address = None;
        self.device_profile = None;
        self.serial_number = None;
        self.device_id = None;
        self.device_id2 = None;
        self.signature = None;
        self.timezone = None;
        self.locale = None;
        self.user_agent = None;
        self.x_user_agent = None;
    }
}

/// Top-level Stalker input configuration block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct StalkerInputConfigDto {
    /// Device identity block. Optional — library derives defaults from MAC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<StalkerDeviceProfileDto>,
    /// Authentication mode override.
    #[serde(default, skip_serializing_if = "StalkerAuthMode::is_default")]
    pub auth_mode: StalkerAuthMode,
    /// MAG preset override.
    #[serde(default, skip_serializing_if = "StalkerMagPreset::is_default")]
    pub mag_preset: StalkerMagPreset,
    /// Endpoint preference override.
    #[serde(default, skip_serializing_if = "StalkerEndpointPreference::is_default")]
    pub endpoint_preference: StalkerEndpointPreference,
    /// Optional body-size caps. `None` => use library defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_caps: Option<StalkerActionSizeCapDto>,
    /// Optional pagination guard for large portal catalogs. `None` => use library default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_max_pages: Option<u32>,
}

impl StalkerInputConfigDto {
    pub fn is_empty(&self) -> bool {
        self.device.as_ref().is_none_or(StalkerDeviceProfileDto::is_empty)
            && StalkerAuthMode::is_default(&self.auth_mode)
            && StalkerMagPreset::is_default(&self.mag_preset)
            && StalkerEndpointPreference::is_default(&self.endpoint_preference)
            && self.size_caps.as_ref().is_none_or(StalkerActionSizeCapDto::is_default)
            && self.catalog_max_pages.is_none()
    }

    pub fn clean(&mut self) {
        if let Some(device) = self.device.as_mut() {
            if device.is_empty() {
                self.device = None;
            } else {
                device.clean();
            }
        }
        self.auth_mode = StalkerAuthMode::default();
        self.mag_preset = StalkerMagPreset::default();
        self.endpoint_preference = StalkerEndpointPreference::default();
        if let Some(caps) = self.size_caps.as_mut() {
            if caps.is_default() {
                self.size_caps = None;
            } else {
                caps.clean();
            }
        }
        if self.catalog_max_pages == Some(0) {
            self.catalog_max_pages = None;
        }
    }
}

/// Per-channel command variant — supports the `cmd`/`cmd_1`/`cmd_2`/`cmds[]`/`mc_cmd`
/// fallback chain observed in real-world Stalker portals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StalkerCommandVariantDto {
    pub cmd: String,
    pub playback_mode: StalkerPlaybackMode,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub source_key: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub priority: u32,
}

impl Default for StalkerCommandVariantDto {
    fn default() -> Self {
        Self { cmd: String::new(), playback_mode: StalkerPlaybackMode::DirectUrl, source_key: None, priority: 0 }
    }
}

/// Decoded portal capabilities detected from `get_profile` + `get_genres`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct StalkerPortalCapabilitiesDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_http_temporary_link: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub nginx_secure_link: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub flussonic_temporary_link: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub wowza_temporary_link: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_load_balancing: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_local_timeshift: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_local_pvr: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_remote_pvr: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub archive_available: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub module_restricted: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub ambiguous_account_state: bool,
    #[serde(default, skip_serializing_if = "is_default_bootstrap_strategy")]
    pub bootstrap_strategy: StalkerBootstrapStrategy,
}

impl StalkerPortalCapabilitiesDto {
    pub fn is_default(&self) -> bool {
        !self.use_http_temporary_link
            && !self.nginx_secure_link
            && !self.flussonic_temporary_link
            && !self.wowza_temporary_link
            && !self.use_load_balancing
            && !self.allow_local_timeshift
            && !self.allow_local_pvr
            && !self.allow_remote_pvr
            && !self.archive_available
            && !self.module_restricted
            && !self.ambiguous_account_state
            && is_default_bootstrap_strategy(&self.bootstrap_strategy)
    }

    pub fn clean(&mut self) {
        self.use_http_temporary_link = false;
        self.nginx_secure_link = false;
        self.flussonic_temporary_link = false;
        self.wowza_temporary_link = false;
        self.use_load_balancing = false;
        self.allow_local_timeshift = false;
        self.allow_local_pvr = false;
        self.allow_remote_pvr = false;
        self.archive_available = false;
        self.module_restricted = false;
        self.ambiguous_account_state = false;
        self.bootstrap_strategy = StalkerBootstrapStrategy::default();
    }
}

fn is_default_bootstrap_strategy(value: &StalkerBootstrapStrategy) -> bool {
    matches!(value, StalkerBootstrapStrategy::Auto)
}

/// Aggregated playback descriptor (primary mode + ordered candidates + capabilities).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct StalkerPlaybackDescriptorDto {
    pub primary_mode: StalkerPlaybackMode,
    pub candidates: Vec<StalkerCommandVariantDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<StalkerPortalCapabilitiesDto>,
}

impl StalkerPlaybackDescriptorDto {
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty() && self.capabilities.as_ref().is_none_or(StalkerPortalCapabilitiesDto::is_default)
    }

    pub fn clean(&mut self) {
        self.candidates.clear();
        if let Some(cap) = self.capabilities.as_mut() {
            if cap.is_default() {
                self.capabilities = None;
            } else {
                cap.clean();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_mode_default_is_auto() {
        assert_eq!(StalkerAuthMode::default(), StalkerAuthMode::Auto);
        assert!(StalkerAuthMode::is_default(&StalkerAuthMode::Auto));
        assert!(!StalkerAuthMode::is_default(&StalkerAuthMode::MacOnly));
    }

    #[test]
    fn stream_kind_round_trip() {
        for kind in
            [StalkerStreamKind::Live, StalkerStreamKind::Archive, StalkerStreamKind::Movie, StalkerStreamKind::Episode]
        {
            let segment = kind.as_path_segment();
            assert_eq!(StalkerStreamKind::parse_path_segment(segment), Some(kind));
        }
        assert_eq!(StalkerStreamKind::parse_path_segment("unknown"), None);
    }

    #[test]
    fn device_profile_is_empty_for_defaults() {
        let profile = StalkerDeviceProfileDto::default();
        assert!(profile.is_empty());
        let mut profile = profile;
        profile.mac_address = Some("00:1A:79:00:00:01".to_string());
        assert!(!profile.is_empty());
        profile.clean();
        assert!(profile.is_empty());
    }

    #[test]
    fn input_config_clean_resets_to_defaults() {
        let mut cfg = StalkerInputConfigDto {
            device: Some(StalkerDeviceProfileDto {
                mac_address: Some("00:1A:79:00:00:01".to_string()),
                ..Default::default()
            }),
            auth_mode: StalkerAuthMode::MacOnly,
            mag_preset: StalkerMagPreset::Mag254Strict,
            endpoint_preference: StalkerEndpointPreference::Portal,
            size_caps: Some(StalkerActionSizeCapDto::default()),
            catalog_max_pages: Some(256),
        };
        cfg.clean();
        assert!(cfg.is_empty());
    }

    #[test]
    fn action_size_caps_default_values_match_documented_caps() {
        let caps = StalkerActionSizeCapDto::default();
        assert_eq!(caps.create_link_kb, 64);
        assert_eq!(caps.ordered_list_mb, 8);
        assert_eq!(caps.get_epg_mb, 64);
        assert!(caps.is_default());
    }

    #[test]
    fn playback_descriptor_empty_for_minimal_input() {
        let mut descriptor = StalkerPlaybackDescriptorDto {
            primary_mode: StalkerPlaybackMode::DirectUrl,
            candidates: vec![StalkerCommandVariantDto {
                cmd: "ffmpeg http://example/stream.ts".to_string(),
                playback_mode: StalkerPlaybackMode::DirectUrl,
                ..Default::default()
            }],
            capabilities: Some(StalkerPortalCapabilitiesDto::default()),
        };
        assert!(!descriptor.is_empty());
        descriptor.clean();
        // clean() drops empty candidates, so the descriptor is now fully empty.
        assert!(descriptor.candidates.is_empty());
        assert!(descriptor.capabilities.is_none());
        assert!(descriptor.is_empty());
    }
}
