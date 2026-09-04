use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    cmp::{max, min},
    sync::Arc,
};

fn get_epg_interval(channels: &Vec<EpgChannel>) -> (i64, i64) {
    if channels.is_empty() {
        return (0, 0);
    }
    let mut epg_start = i64::MAX;
    let mut epg_stop = i64::MIN;
    for channel in channels {
        for programme in &channel.programmes {
            epg_start = min(epg_start, programme.start);
            epg_stop = max(epg_stop, programme.stop);
        }
    }
    // Handle case where channels exist but have no programmes
    if epg_start == i64::MAX {
        return (0, 0);
    }
    (epg_start, epg_stop)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EpgTv {
    pub start: i64,
    pub stop: i64,
    pub channels: Vec<EpgChannel>,
}

impl EpgTv {
    pub fn new(channels: Vec<EpgChannel>) -> Self {
        let (start, stop) = get_epg_interval(&channels);
        Self { start, stop, channels }
    }
}

impl PartialEq for EpgTv {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.stop == other.stop
        // Note: self.channels is skipped
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EpgChannel {
    pub id: Arc<str>,
    pub title: Option<Arc<str>>,
    pub icon: Option<Arc<str>>,
    pub programmes: Vec<EpgProgramme>,
}

impl EpgChannel {
    pub fn new(id: Arc<str>) -> Self { Self { id, title: None, icon: None, programmes: Vec::new() } }

    pub fn get_programme_with_limit(&self, limit: u32) -> Vec<&EpgProgramme> {
        let now = Utc::now().timestamp();
        self.get_programme_with_limit_at(limit, now)
    }

    fn get_programme_with_limit_at(&self, limit: u32, now: i64) -> Vec<&EpgProgramme> {
        // Programmes are sorted by start, not by stop. Use binary search only
        // for the first future entry, then scan the earlier prefix for an
        // overlapping programme whose stop time has not passed.
        let first_future = self.programmes.partition_point(|programme| programme.start < now);
        let start_idx =
            self.programmes[..first_future].iter().position(|programme| programme.stop >= now).unwrap_or(first_future);
        self.programmes.iter().skip(start_idx).filter(|programme| programme.stop >= now).take(limit as usize).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{EpgChannel, EpgProgramme};
    use crate::utils::Internable;

    #[test]
    fn programme_limit_handles_non_monotonic_stop_times() {
        let channel = EpgChannel {
            id: "channel".intern(),
            title: None,
            icon: None,
            programmes: vec![
                EpgProgramme::new(0, 100, "channel".intern()),
                EpgProgramme::new(10, 20, "channel".intern()),
                EpgProgramme::new(30, 40, "channel".intern()),
            ],
        };

        let programmes = channel.get_programme_with_limit_at(1, 50);
        assert_eq!(programmes.len(), 1);
        assert_eq!(programmes[0].start, 0);
    }

    #[test]
    fn programme_limit_excludes_expired_entries_after_current_programme() {
        let channel = EpgChannel {
            id: "channel".intern(),
            title: None,
            icon: None,
            programmes: vec![
                EpgProgramme::new(0, 100, "channel".intern()),
                EpgProgramme::new(10, 20, "channel".intern()),
                EpgProgramme::new(60, 80, "channel".intern()),
            ],
        };

        let programmes = channel.get_programme_with_limit_at(2, 50);
        assert_eq!(programmes.len(), 2);
        assert_eq!(programmes[0].start, 0);
        assert_eq!(programmes[1].start, 60);
    }

    #[test]
    fn airing_status_uses_is_new_for_new() {
        let mut p = EpgProgramme::new(0, 1, "c".intern());
        p.is_new = true;
        p.previously_shown = false;
        assert!(matches!(p.airing_status(), crate::model::recording::AiringStatus::New));
    }

    #[test]
    fn airing_status_uses_previously_shown_for_repeat() {
        let mut p = EpgProgramme::new(0, 1, "c".intern());
        p.is_new = false;
        p.previously_shown = true;
        assert!(matches!(p.airing_status(), crate::model::recording::AiringStatus::Repeat));
    }

    #[test]
    fn airing_status_unknown_when_neither_flag_set() {
        let p = EpgProgramme::new(0, 1, "c".intern());
        assert!(matches!(p.airing_status(), crate::model::recording::AiringStatus::Unknown));
    }

    #[test]
    fn airing_status_is_new_wins_over_previously_shown() {
        // An explicit `<new>` survives a merged `<previously-shown>`
        // from another source.
        let mut p = EpgProgramme::new(0, 1, "c".intern());
        p.is_new = true;
        p.previously_shown = true;
        assert!(matches!(p.airing_status(), crate::model::recording::AiringStatus::New));
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct EpgCategory {
    pub value: Arc<str>,
    #[serde(default)]
    pub lang: Option<Arc<str>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EpgProgramme {
    pub start: i64,
    pub stop: i64,
    pub title: Option<Arc<str>>,
    pub desc: Option<Arc<str>>,
    #[serde(default)]
    pub icon: Option<Arc<str>>,
    #[serde(default)]
    pub catchup_id: Option<Arc<str>>,
    #[serde(default)]
    pub categories: Vec<EpgCategory>,
    #[serde(default)]
    pub is_live: bool,
    #[serde(default)]
    pub is_new: bool,
    /// XMLTV `<previously-shown>` flag. Required for the
    /// tri-state `AiringStatus` (Unknown / New / Repeat) used by
    /// new-episode rules. The legacy `is_new` field stays
    /// serialized for backward compatibility; new code should
    /// derive `airing_status()` from both flags.
    #[serde(default)]
    pub previously_shown: bool,
    #[serde(skip)]
    channel: Arc<str>,
}

impl EpgProgramme {
    // the channel_id is only available when read from xml file, reading from db do not return any epg_id
    pub fn get_transient_channel_id(&self) -> &Arc<str> { &self.channel }
}

impl EpgProgramme {
    pub fn new(start: i64, stop: i64, channel: Arc<str>) -> Self {
        Self::new_all(start, stop, channel, None, None, None)
    }
    pub fn new_all(
        start: i64,
        stop: i64,
        channel: Arc<str>,
        title: Option<Arc<str>>,
        desc: Option<Arc<str>>,
        catchup_id: Option<Arc<str>>,
    ) -> Self {
        Self {
            start,
            stop,
            channel,
            title,
            desc,
            icon: None,
            catchup_id,
            categories: Vec::new(),
            is_live: false,
            is_new: false,
            previously_shown: false,
        }
    }

    /// Pure: collapse the two XMLTV booleans into the tri-state
    /// `AiringStatus`. Never infer `Repeat` only from old
    /// `is_new == false`. Both `false` means `Unknown`; `is_new`
    /// wins over `previously_shown` so an
    /// explicit `<new>` survives a merged `<previously-shown>`
    /// from another source.
    pub fn airing_status(&self) -> crate::model::recording::AiringStatus {
        use crate::model::recording::AiringStatus;
        if self.is_new {
            AiringStatus::New
        } else if self.previously_shown {
            AiringStatus::Repeat
        } else {
            AiringStatus::Unknown
        }
    }
}

/// Request DTO for per-stream EPG lookup.
/// The Vec allows future batch expansion (multiple `epg_channel_ids`) without a redesign.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamEpgRequest {
    pub items: Vec<StreamEpgItemRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEpgItemRequest {
    pub epg_channel_id: String,
    #[serde(default)]
    pub target_id: Option<u16>,
    #[serde(default)]
    pub reference_ts: Option<i64>,
}

/// Response DTO for per-stream EPG lookup.
/// Contains entries per unique `epg_channel_id`, programmes already filtered to 8h window.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamEpgResponse {
    pub entries: Vec<StreamEpgEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEpgEntry {
    pub epg_channel_id: String,
    #[serde(default)]
    pub target_id: Option<u16>,
    pub programmes: Vec<EpgProgrammeDto>,
}

/// Programme DTO with timeshift-adjusted display strings and raw timestamps for local current/next computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpgProgrammeDto {
    /// Raw UTC start timestamp — use for local current/next computation.
    pub start_timestamp: i64,
    /// Raw UTC stop timestamp.
    pub stop_timestamp: i64,
    /// Timeshift-adjusted display string, e.g. "20260421090000 +0200".
    pub start: String,
    /// Timeshift-adjusted display string.
    pub stop: String,
    pub title: String,
}
