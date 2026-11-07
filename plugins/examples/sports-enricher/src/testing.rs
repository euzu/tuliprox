//! Builders shared by the unit tests (compiled only for tests).

use crate::{fixtures, Settings};
use tuliprox_plugin_sdk::{mock, prelude::*};

/// A live channel.
pub fn channel(handle: u32, name: &str, group: &str, epg_id: Option<&str>, logo: &str) -> Channel {
    Channel {
        handle,
        name: name.to_string(),
        title: String::new(),
        group: group.to_string(),
        url: format!("http://example.invalid/{handle}.ts"),
        epg_id: epg_id.map(str::to_string),
        logo: logo.to_string(),
        chno: None,
        item_type: ItemType::Live,
        cluster: Cluster::Live,
        input_name: "demo".to_string(),
        input_stream_id: handle.to_string(),
        extra: Vec::new(),
    }
}

/// A VOD item.
pub fn movie(name: &str, group: &str) -> Channel {
    let mut item = channel(0, name, group, None, "");
    item.item_type = ItemType::Video;
    item.cluster = Cluster::Video;
    item
}

/// Fresh mock host and memo, then the settings parsed from `json`.
pub fn settings(json: &str) -> Settings {
    mock::reset();
    fixtures::forget();
    Settings::parse(json).unwrap()
}
