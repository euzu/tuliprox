use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::cmp::{max, min};
use std::sync::Arc;

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
        Self {
            start,
            stop,
            channels,
        }
    }
}

impl PartialEq for EpgTv {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start
            && self.stop == other.stop
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
    pub fn new(id: Arc<str>) -> Self {
        Self {
            id,
            title: None,
            icon: None,
            programmes: Vec::new(),
        }
    }

    pub fn get_programme_with_limit(&self, limit: u32) -> Vec<&EpgProgramme> {
        let now = Utc::now().timestamp();

        // find index for first relevant entry
        let start_idx = self.programmes.iter()
            .position(|p| (p.start <= now && now <= p.stop) || (p.start >= now))
            .unwrap_or(self.programmes.len()); // nothing found, empty response

        // slice from start_idx, max. limit
        self.programmes
            .iter()
            .skip(start_idx)
            .take(limit as usize)
            .collect()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EpgProgramme {
    pub start: i64,
    pub stop: i64,
    pub title: Option<Arc<str>>,
    pub desc: Option<Arc<str>>,
    #[serde(skip)]
    channel: Arc<str>,
}

impl EpgProgramme {
    // the channel_id is only available when read from xml file, reading from db do not return any epg_id
    pub fn get_transient_channel_id(&self) -> &Arc<str> {
        &self.channel
    }
}

impl EpgProgramme {
    pub fn new(start: i64, stop: i64, channel: Arc<str>) -> Self {
        Self {
            start,
            stop,
            channel,
            title: None,
            desc: None,
        }
    }
    pub fn new_all(start: i64, stop: i64, channel: Arc<str>, title: Option<Arc<str>>, desc: Option<Arc<str>>) -> Self {
        Self {
            start,
            stop,
            channel,
            title,
            desc,
        }
    }
}
