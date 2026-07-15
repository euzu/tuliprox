use crate::utils::is_blank_optional_string;
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
///
/// The per-field serde defaults MUST match `Default::default()` — otherwise a
/// partially-specified YAML block (`size_caps: { create_link_kb: 96 }`) would
/// silently zero out the remaining caps and break every capped request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StalkerActionSizeCapDto {
    #[serde(default = "default_create_link_kb")]
    pub create_link_kb: u32,
    #[serde(default = "default_ordered_list_mb")]
    pub ordered_list_mb: u32,
    #[serde(default = "default_get_epg_mb")]
    pub get_epg_mb: u32,
}

const fn default_create_link_kb() -> u32 { 64 }
const fn default_ordered_list_mb() -> u32 { 8 }
const fn default_get_epg_mb() -> u32 { 64 }

impl Default for StalkerActionSizeCapDto {
    fn default() -> Self {
        Self {
            create_link_kb: default_create_link_kb(),
            ordered_list_mb: default_ordered_list_mb(),
            get_epg_mb: default_get_epg_mb(),
        }
    }
}

impl StalkerActionSizeCapDto {
    pub fn is_default(&self) -> bool { *self == Self::default() }

    pub fn clean(&mut self) { *self = Self::default(); }
}

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
        // Normalize to `None` — a `Some(empty)` block would be re-serialized
        // as a noise `device: {}` entry on the next config save.
        self.device = None;
        self.auth_mode = StalkerAuthMode::default();
        self.mag_preset = StalkerMagPreset::default();
        self.endpoint_preference = StalkerEndpointPreference::default();
        self.size_caps = None;
        self.catalog_max_pages = None;
    }
}

/// Per-channel command variant — supports the `cmd`/`cmd_1`/`cmd_2`/`cmds[]`/`mc_cmd`
/// fallback chain observed in real-world Stalker portals.
///
/// NOTE: this struct is embedded in [`crate::model::stalker_item::StalkerPlaylistItem`],
/// which is persisted via positional MessagePack (`rmp_serde::to_vec`). Optional
/// fields must therefore NEVER use `skip_serializing_if` — a skipped field shifts
/// every subsequent value one slot left on read and corrupts the record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StalkerCommandVariantDto {
    pub cmd: String,
    pub playback_mode: StalkerPlaybackMode,
    #[serde(default)]
    pub source_key: Option<String>,
    #[serde(default)]
    pub priority: u32,
}

impl Default for StalkerCommandVariantDto {
    fn default() -> Self {
        Self { cmd: String::new(), playback_mode: StalkerPlaybackMode::DirectUrl, source_key: None, priority: 0 }
    }
}

/// Decoded portal capabilities detected from `get_profile` + `get_genres`.
///
/// NOTE: embedded in the B+Tree-persisted `StalkerPlaylistItem` (positional
/// MessagePack) — fields must NOT use `skip_serializing_if` (see
/// `StalkerCommandVariantDto`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StalkerPortalCapabilitiesDto {
    #[serde(default)]
    pub use_http_temporary_link: bool,
    #[serde(default)]
    pub nginx_secure_link: bool,
    #[serde(default)]
    pub flussonic_temporary_link: bool,
    #[serde(default)]
    pub wowza_temporary_link: bool,
    #[serde(default)]
    pub use_load_balancing: bool,
    #[serde(default)]
    pub allow_local_timeshift: bool,
    #[serde(default)]
    pub allow_local_pvr: bool,
    #[serde(default)]
    pub allow_remote_pvr: bool,
    #[serde(default)]
    pub archive_available: bool,
    #[serde(default)]
    pub module_restricted: bool,
    #[serde(default)]
    pub ambiguous_account_state: bool,
    #[serde(default)]
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
///
/// NOTE: embedded in the B+Tree-persisted `StalkerPlaylistItem` (positional
/// MessagePack) — fields must NOT use `skip_serializing_if` (see
/// `StalkerCommandVariantDto`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StalkerPlaybackDescriptorDto {
    pub primary_mode: StalkerPlaybackMode,
    pub candidates: Vec<StalkerCommandVariantDto>,
    #[serde(default)]
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
