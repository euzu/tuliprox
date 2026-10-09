use crate::utils::{arc_str_none_default_on_null, arc_str_option_serde};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CatchupAttribute {
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub name: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub value: Arc<str>,
}

#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CatchupProperties {
    #[serde(default, with = "arc_str_option_serde")]
    pub mode: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub days: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub source: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub time: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub correction: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub catchup_type: Option<Arc<str>>,
    #[serde(default)]
    pub extra_attributes: Vec<CatchupAttribute>,
    /// Origin window for an automatically generated bounded Flussonic source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flussonic_archive_max_duration_secs: Option<u32>,
}

impl CatchupProperties {
    pub fn is_empty(&self) -> bool {
        self.mode.is_none()
            && self.days.is_none()
            && self.source.is_none()
            && self.time.is_none()
            && self.correction.is_none()
            && self.catchup_type.is_none()
            && self.extra_attributes.is_empty()
            && self.flussonic_archive_max_duration_secs.is_none()
    }

    /// Prefer `catchup-type` when both fields are present because providers may leave
    /// a stale `catchup` mode alongside the authoritative player type.
    pub fn effective_mode(&self) -> Option<&str> {
        self.catchup_type
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| self.mode.as_deref().map(str::trim).filter(|value| !value.is_empty()))
    }

    pub(super) fn is_flussonic_mode_str(mode: Option<&str>) -> bool {
        mode.is_some_and(|m| {
            matches!(m.trim().to_ascii_lowercase().as_str(), "flussonic" | "flussonic-hls" | "flussonic-ts" | "fs")
        })
    }

    /// Prefer `catchup-type` when both are set so a leftover `catchup="shift"`/`append`
    /// from a provider cannot steal Flussonic path-rewrite channels (v3.3.81 behavior).
    pub fn is_flussonic(&self) -> bool { Self::is_flussonic_mode_str(self.effective_mode()) }

    pub fn native_flussonic_player_mode(&self) -> Option<&'static str> {
        if !self.is_flussonic() {
            return None;
        }
        let raw = self.effective_mode().unwrap_or("flussonic");
        if raw.eq_ignore_ascii_case("flussonic-ts") {
            Some("flussonic-ts")
        } else {
            Some("flussonic")
        }
    }

    /// Canonical append label for unified M3U export (`catchup-type="append"` only).
    pub fn append_player_type(&self) -> Option<&'static str> {
        if self.native_flussonic_player_mode().is_some() {
            return None;
        }
        self.effective_mode()?.eq_ignore_ascii_case("append").then_some("append")
    }
}
