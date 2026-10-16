use crate::model::ChannelGroupPolicy;
use std::collections::HashSet;

pub const CHANNEL_GROUP_OVERRIDES_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum CapacityTier {
    Free,
    OwnRelease,
    Grace,
    Preempt,
    Exhausted,
}

/// Why a running group stream replaced its member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchReason {
    UpstreamError,
    UpstreamEof,
    Stall,
    Underrun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    OpenUntil(std::time::Instant),
    HalfOpenInFlight,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ChannelMemberRefDto {
    pub input: String,
    pub stream_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManualChannelGroupDto {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ChannelGroupPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ChannelMemberRefDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AutoGroupOverrideDto {
    pub auto_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ChannelGroupPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<ChannelMemberRefDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<ChannelMemberRefDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<ChannelMemberRefDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChannelGroupOverridesDto {
    pub version: u32,
    pub target: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ManualChannelGroupDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_overrides: Vec<AutoGroupOverrideDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<ChannelMemberRefDto>,
}

impl ChannelGroupOverridesDto {
    pub fn new(target: &str) -> Self {
        Self {
            version: CHANNEL_GROUP_OVERRIDES_VERSION,
            target: target.to_string(),
            revision: 0,
            groups: Vec::new(),
            auto_overrides: Vec::new(),
            excluded: Vec::new(),
        }
    }

    pub fn canonicalize(&mut self) {
        self.target = self.target.trim().to_string();
        for group in &mut self.groups {
            group.id = group.id.trim().to_string();
            group.name = group.name.trim().to_string();
            group.logo = group.logo.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            group.epg_channel_id =
                group.epg_channel_id.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            group.playlist_group =
                group.playlist_group.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            for m in &mut group.members {
                m.input = m.input.trim().to_string();
                m.stream_id = m.stream_id.trim().to_string();
            }
            let mut seen = HashSet::new();
            group.members.retain(|m| {
                !m.input.is_empty() && !m.stream_id.is_empty() && seen.insert((m.input.clone(), m.stream_id.clone()))
            });
        }
        for auto in &mut self.auto_overrides {
            auto.auto_key = auto.auto_key.trim().to_string();
            auto.name = auto.name.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            auto.logo = auto.logo.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            auto.epg_channel_id = auto.epg_channel_id.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            auto.playlist_group = auto.playlist_group.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            for m in &mut auto.order {
                m.input = m.input.trim().to_string();
                m.stream_id = m.stream_id.trim().to_string();
            }
            let mut seen = HashSet::new();
            auto.order.retain(|m| {
                !m.input.is_empty() && !m.stream_id.is_empty() && seen.insert((m.input.clone(), m.stream_id.clone()))
            });

            for m in &mut auto.add {
                m.input = m.input.trim().to_string();
                m.stream_id = m.stream_id.trim().to_string();
            }
            seen.clear();
            auto.add.retain(|m| {
                !m.input.is_empty() && !m.stream_id.is_empty() && seen.insert((m.input.clone(), m.stream_id.clone()))
            });

            for m in &mut auto.remove {
                m.input = m.input.trim().to_string();
                m.stream_id = m.stream_id.trim().to_string();
            }
            seen.clear();
            auto.remove.retain(|m| {
                !m.input.is_empty() && !m.stream_id.is_empty() && seen.insert((m.input.clone(), m.stream_id.clone()))
            });
        }
        for m in &mut self.excluded {
            m.input = m.input.trim().to_string();
            m.stream_id = m.stream_id.trim().to_string();
        }
        let mut seen = HashSet::new();
        self.excluded.retain(|m| {
            !m.input.is_empty() && !m.stream_id.is_empty() && seen.insert((m.input.clone(), m.stream_id.clone()))
        });
    }
}
