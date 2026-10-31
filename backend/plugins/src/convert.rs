//! `PlaylistItem` ⇄ WIT `channel` (plan §5 `types`, §14 `convert.rs`).
//!
//! FROZEN W4 API (dispatch builds on these signatures; B1 owns the bodies):
//! [`item_to_channel`], [`apply_channel`], the enum maps both ways, and
//! [`EXTRA_KEYS`]. The handle is a per-call index issued by the caller;
//! `VirtualId` never crosses this boundary (W1 amendment).
//!
//! Direction of travel: the host sends a full [`Channel`] per item
//! ([`item_to_channel`]); a plugin may return a `modified` channel, of which
//! only the plugin-writable fields (`name`, `title`, `group`, `url`, `epg-id`,
//! `logo`, `chno`) are copied back ([`apply_channel`]). Everything else is
//! host-owned and ignored on return, including `extra`.

use shared::{
    model::{PlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster},
    utils::Internable,
};
use std::sync::Arc;
use tuliprox_plugin_api::bindings::tuliprox::plugin::types::{Channel, Cluster as WitCluster, ItemType as WitItemType};

/// The `extra` keys the host sends (plan §18 W0 memo, item 8), in this order,
/// each read from the matching header field: `provider-id` ← `id`,
/// `logo-small` ← `logo_small`, `parent-code`, `time-shift`, `rec`,
/// `audio-track`. An entry whose value is empty is omitted, so an item with
/// none of them sends an empty bag. Host → guest only in ABI 1.0: ignored on
/// return.
pub(crate) const EXTRA_KEYS: [&str; 6] =
    ["provider-id", "logo-small", "parent-code", "time-shift", "rec", "audio-track"];

pub(crate) const fn item_type_to_wit(item_type: PlaylistItemType) -> WitItemType {
    match item_type {
        PlaylistItemType::Live => WitItemType::Live,
        PlaylistItemType::Video => WitItemType::Video,
        PlaylistItemType::Series => WitItemType::Series,
        PlaylistItemType::SeriesInfo => WitItemType::SeriesInfo,
        PlaylistItemType::Catchup => WitItemType::Catchup,
        PlaylistItemType::LiveUnknown => WitItemType::LiveUnknown,
        PlaylistItemType::LiveHls => WitItemType::LiveHls,
        PlaylistItemType::LiveDash => WitItemType::LiveDash,
        PlaylistItemType::LocalVideo => WitItemType::LocalVideo,
        PlaylistItemType::LocalSeries => WitItemType::LocalSeries,
        PlaylistItemType::LocalSeriesInfo => WitItemType::LocalSeriesInfo,
    }
}

pub(crate) const fn cluster_to_wit(cluster: XtreamCluster) -> WitCluster {
    match cluster {
        XtreamCluster::Live => WitCluster::Live,
        XtreamCluster::Video => WitCluster::Video,
        XtreamCluster::Series => WitCluster::Series,
    }
}

#[allow(
    dead_code,
    reason = "the reverse half of the contract; only tests use it until the W6 input providers build channels"
)]
pub(crate) const fn item_type_from_wit(item_type: WitItemType) -> PlaylistItemType {
    match item_type {
        WitItemType::Live => PlaylistItemType::Live,
        WitItemType::Video => PlaylistItemType::Video,
        WitItemType::Series => PlaylistItemType::Series,
        WitItemType::SeriesInfo => PlaylistItemType::SeriesInfo,
        WitItemType::Catchup => PlaylistItemType::Catchup,
        WitItemType::LiveUnknown => PlaylistItemType::LiveUnknown,
        WitItemType::LiveHls => PlaylistItemType::LiveHls,
        WitItemType::LiveDash => PlaylistItemType::LiveDash,
        WitItemType::LocalVideo => PlaylistItemType::LocalVideo,
        WitItemType::LocalSeries => PlaylistItemType::LocalSeries,
        WitItemType::LocalSeriesInfo => PlaylistItemType::LocalSeriesInfo,
    }
}

#[allow(
    dead_code,
    reason = "the reverse half of the contract; only tests use it until the W6 input providers build channels"
)]
pub(crate) const fn cluster_from_wit(cluster: WitCluster) -> XtreamCluster {
    match cluster {
        WitCluster::Live => XtreamCluster::Live,
        WitCluster::Video => XtreamCluster::Video,
        WitCluster::Series => XtreamCluster::Series,
    }
}

/// The `extra` bag for `header`: [`EXTRA_KEYS`] order, empty values omitted.
fn extra_bag(header: &PlaylistItemHeader) -> Vec<(String, String)> {
    let values: [&str; EXTRA_KEYS.len()] =
        [&header.id, &header.logo_small, &header.parent_code, &header.time_shift, &header.rec, &header.audio_track];
    EXTRA_KEYS
        .iter()
        .zip(values)
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| ((*key).to_string(), value.to_string()))
        .collect()
}

/// The guest-facing view of `item`, issued under `handle`.
pub(crate) fn item_to_channel(handle: u32, item: &PlaylistItem) -> Channel {
    let header = &item.header;
    Channel {
        handle,
        name: header.name.to_string(),
        title: header.title.to_string(),
        group: header.group.to_string(),
        url: header.url.to_string(),
        epg_id: header.epg_channel_id.as_ref().map(ToString::to_string),
        logo: header.logo.to_string(),
        chno: (header.chno != 0).then_some(header.chno),
        item_type: item_type_to_wit(header.item_type),
        cluster: cluster_to_wit(header.xtream_cluster),
        input_name: header.input_name.to_string(),
        input_stream_id: header.input_stream_id.to_string(),
        extra: extra_bag(header),
    }
}

fn set_text(field: &mut Arc<str>, value: String) -> bool {
    if **field == *value {
        return false;
    }
    *field = value.intern();
    true
}

/// Copy the plugin-writable fields of a `modified` channel onto `item`
/// (`name`, `title`, `group`, `url`, `logo`, `epg-id`, `chno`) and return
/// whether any of them changed. Host-owned channel fields (`handle`,
/// `item-type`, `cluster`, `input-name`, `input-stream-id`, `extra`) are
/// ignored, and header fields with no channel counterpart (`virtual_id`,
/// `uuid`, `id`, `category_id`, `source_ordinal`, `additional_properties`, ...)
/// are never touched. A blank `epg-id` becomes `None`; `chno` `None` is `0`.
pub(crate) fn apply_channel(item: &mut PlaylistItem, channel: Channel) -> bool {
    let header = &mut item.header;
    let mut changed = set_text(&mut header.name, channel.name);
    changed |= set_text(&mut header.title, channel.title);
    changed |= set_text(&mut header.group, channel.group);
    changed |= set_text(&mut header.url, channel.url);
    changed |= set_text(&mut header.logo, channel.logo);
    // A blank EPG id is no id: the playlist model treats `None` as "unmapped".
    let epg_id = channel.epg_id.filter(|id| !id.trim().is_empty());
    if header.epg_channel_id.as_deref() != epg_id.as_deref() {
        header.epg_channel_id = epg_id.map(Internable::intern);
        changed = true;
    }
    let chno = channel.chno.unwrap_or(0);
    changed |= header.chno != chno;
    header.chno = chno;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ITEM_TYPES: [PlaylistItemType; 11] = [
        PlaylistItemType::Live,
        PlaylistItemType::Video,
        PlaylistItemType::Series,
        PlaylistItemType::SeriesInfo,
        PlaylistItemType::Catchup,
        PlaylistItemType::LiveUnknown,
        PlaylistItemType::LiveHls,
        PlaylistItemType::LiveDash,
        PlaylistItemType::LocalVideo,
        PlaylistItemType::LocalSeries,
        PlaylistItemType::LocalSeriesInfo,
    ];
    const ALL_CLUSTERS: [XtreamCluster; 3] = [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series];
    const ALL_WIT_ITEM_TYPES: [WitItemType; 11] = [
        WitItemType::Live,
        WitItemType::Video,
        WitItemType::Series,
        WitItemType::SeriesInfo,
        WitItemType::Catchup,
        WitItemType::LiveUnknown,
        WitItemType::LiveHls,
        WitItemType::LiveDash,
        WitItemType::LocalVideo,
        WitItemType::LocalSeries,
        WitItemType::LocalSeriesInfo,
    ];
    const ALL_WIT_CLUSTERS: [WitCluster; 3] = [WitCluster::Live, WitCluster::Video, WitCluster::Series];

    // The matches below have no wildcard arm: adding a variant to any of the four
    // enums fails to compile here until the lists above are extended too.
    const fn item_type_slot(item_type: PlaylistItemType) -> usize {
        match item_type {
            PlaylistItemType::Live => 0,
            PlaylistItemType::Video => 1,
            PlaylistItemType::Series => 2,
            PlaylistItemType::SeriesInfo => 3,
            PlaylistItemType::Catchup => 4,
            PlaylistItemType::LiveUnknown => 5,
            PlaylistItemType::LiveHls => 6,
            PlaylistItemType::LiveDash => 7,
            PlaylistItemType::LocalVideo => 8,
            PlaylistItemType::LocalSeries => 9,
            PlaylistItemType::LocalSeriesInfo => 10,
        }
    }
    const fn wit_item_type_slot(item_type: WitItemType) -> usize {
        match item_type {
            WitItemType::Live => 0,
            WitItemType::Video => 1,
            WitItemType::Series => 2,
            WitItemType::SeriesInfo => 3,
            WitItemType::Catchup => 4,
            WitItemType::LiveUnknown => 5,
            WitItemType::LiveHls => 6,
            WitItemType::LiveDash => 7,
            WitItemType::LocalVideo => 8,
            WitItemType::LocalSeries => 9,
            WitItemType::LocalSeriesInfo => 10,
        }
    }
    const fn cluster_slot(cluster: XtreamCluster) -> usize {
        match cluster {
            XtreamCluster::Live => 0,
            XtreamCluster::Video => 1,
            XtreamCluster::Series => 2,
        }
    }
    const fn wit_cluster_slot(cluster: WitCluster) -> usize {
        match cluster {
            WitCluster::Live => 0,
            WitCluster::Video => 1,
            WitCluster::Series => 2,
        }
    }

    /// Each list holds every variant exactly once, in slot order.
    #[test]
    fn variant_lists_are_exhaustive() {
        for (slot, item_type) in ALL_ITEM_TYPES.into_iter().enumerate() {
            assert_eq!(item_type_slot(item_type), slot);
        }
        for (slot, item_type) in ALL_WIT_ITEM_TYPES.into_iter().enumerate() {
            assert_eq!(wit_item_type_slot(item_type), slot);
        }
        for (slot, cluster) in ALL_CLUSTERS.into_iter().enumerate() {
            assert_eq!(cluster_slot(cluster), slot);
        }
        for (slot, cluster) in ALL_WIT_CLUSTERS.into_iter().enumerate() {
            assert_eq!(wit_cluster_slot(cluster), slot);
        }
    }

    #[test]
    fn item_types_map_one_to_one_both_ways() {
        for item_type in ALL_ITEM_TYPES {
            let wit = item_type_to_wit(item_type);
            assert_eq!(item_type_from_wit(wit), item_type, "{item_type:?}");
            // Same slot on both sides: the mapping is a bijection, not just a round trip.
            assert_eq!(wit_item_type_slot(wit), item_type_slot(item_type), "{item_type:?}");
        }
        for wit in ALL_WIT_ITEM_TYPES {
            assert_eq!(item_type_to_wit(item_type_from_wit(wit)), wit, "{wit:?}");
        }
    }

    #[test]
    fn clusters_map_one_to_one_both_ways() {
        for cluster in ALL_CLUSTERS {
            let wit = cluster_to_wit(cluster);
            assert_eq!(cluster_from_wit(wit), cluster, "{cluster:?}");
            assert_eq!(wit_cluster_slot(wit), cluster_slot(cluster), "{cluster:?}");
        }
        for wit in ALL_WIT_CLUSTERS {
            assert_eq!(cluster_to_wit(cluster_from_wit(wit)), wit, "{wit:?}");
        }
    }

    fn base_item() -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                id: "p-1".intern(),
                name: "Name".intern(),
                title: "Title".intern(),
                group: "Group".intern(),
                url: "http://host/1".intern(),
                logo: "http://host/logo.png".intern(),
                input_name: "input".intern(),
                input_stream_id: "stream-1".intern(),
                chno: 7,
                category_id: 11,
                source_ordinal: 5,
                ..PlaylistItemHeader::default()
            },
        }
    }

    /// Every header field, compared through `Debug` (the header has no `PartialEq`).
    fn header_dump(item: &PlaylistItem) -> String { format!("{:?}", item.header) }

    fn bare_item() -> PlaylistItem { PlaylistItem { header: PlaylistItemHeader::default() } }

    fn changed(value: &str) -> String { format!("{value}-changed") }

    #[test]
    fn identity_round_trip_changes_nothing() {
        type Tweak = fn(&mut PlaylistItemHeader);
        let rows: [(&str, Tweak); 11] = [
            ("defaults", |_| {}),
            ("chno zero", |h| h.chno = 0),
            ("chno non-zero", |h| h.chno = 4_000_000_000),
            ("epg none", |h| h.epg_channel_id = None),
            ("epg some", |h| h.epg_channel_id = Some("epg.one".intern())),
            ("all text empty", |h| {
                h.name = "".intern();
                h.title = "".intern();
                h.group = "".intern();
                h.url = "".intern();
                h.logo = "".intern();
            }),
            ("unicode", |h| {
                h.name = "Das Erste HD \u{1f4fa}".intern();
                h.title = "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30bf}\u{30a4}\u{30c8}\u{30eb}".intern();
                h.group = "Gr\u{fc}\u{df}e / \u{415}\u{432}\u{440}\u{43e}\u{43f}\u{430}".intern();
                h.epg_channel_id = Some("\u{e9}pg.\u{4e2d}".intern());
            }),
            ("surrounding spaces in text", |h| h.name = "  padded  ".intern()),
            ("extras populated", |h| {
                h.logo_small = "small".intern();
                h.parent_code = "0000".intern();
                h.time_shift = "3".intern();
                h.rec = "1".intern();
                h.audio_track = "de".intern();
            }),
            ("input fields empty", |h| {
                h.input_name = "".intern();
                h.input_stream_id = "".intern();
                h.id = "".intern();
            }),
            ("upstream user agent", |h| h.upstream_user_agent = Some("agent/1".intern())),
        ];
        for (label, tweak) in rows {
            for (item_type, cluster) in ALL_ITEM_TYPES.into_iter().zip(ALL_CLUSTERS.into_iter().cycle()) {
                let mut item = base_item();
                tweak(&mut item.header);
                item.header.item_type = item_type;
                item.header.xtream_cluster = cluster;
                let before = header_dump(&item);
                let mut copy = item.clone();
                let channel = item_to_channel(3, &item);
                assert!(!apply_channel(&mut copy, channel), "{label} / {item_type:?}: reported a change");
                assert_eq!(header_dump(&copy), before, "{label} / {item_type:?}: header changed");
            }
        }
    }

    #[test]
    fn channel_carries_the_header_fields() {
        let mut item = base_item();
        item.header.epg_channel_id = Some("epg.one".intern());
        item.header.item_type = PlaylistItemType::LocalSeriesInfo;
        item.header.xtream_cluster = XtreamCluster::Series;
        let channel = item_to_channel(42, &item);
        assert_eq!(channel.handle, 42);
        assert_eq!(channel.name, "Name");
        assert_eq!(channel.title, "Title");
        assert_eq!(channel.group, "Group");
        assert_eq!(channel.url, "http://host/1");
        assert_eq!(channel.logo, "http://host/logo.png");
        assert_eq!(channel.epg_id.as_deref(), Some("epg.one"));
        assert_eq!(channel.chno, Some(7));
        assert_eq!(channel.item_type, WitItemType::LocalSeriesInfo);
        assert_eq!(channel.cluster, WitCluster::Series);
        assert_eq!(channel.input_name, "input");
        assert_eq!(channel.input_stream_id, "stream-1");
    }

    #[test]
    fn chno_zero_is_none_and_back() {
        let mut item = base_item();
        item.header.chno = 0;
        let channel = item_to_channel(0, &item);
        assert_eq!(channel.chno, None);
        item.header.chno = 9;
        assert!(apply_channel(&mut item, channel));
        assert_eq!(item.header.chno, 0);
        // `Some(0)` is also "unassigned": no change against an unassigned item.
        let mut zero = item_to_channel(0, &item);
        zero.chno = Some(0);
        assert!(!apply_channel(&mut item, zero));
    }

    #[test]
    fn blank_epg_id_becomes_none() {
        for blank in ["", " ", "\t\n "] {
            let mut item = base_item();
            item.header.epg_channel_id = Some("epg.one".intern());
            let mut channel = item_to_channel(0, &item);
            channel.epg_id = Some(blank.to_string());
            assert!(apply_channel(&mut item, channel), "{blank:?}");
            assert!(item.header.epg_channel_id.is_none(), "{blank:?}");

            // Already unmapped: a blank id is no change.
            let mut channel = item_to_channel(0, &item);
            channel.epg_id = Some(blank.to_string());
            assert!(!apply_channel(&mut item, channel), "{blank:?}");
        }
    }

    #[test]
    fn each_writable_field_is_written() {
        type Edit = fn(&mut Channel);
        type Read = fn(&PlaylistItem) -> String;
        let rows: [(&str, Edit, Read, &str); 7] = [
            ("name", |c| c.name = changed(&c.name), |i| i.header.name.to_string(), "Name-changed"),
            ("title", |c| c.title = changed(&c.title), |i| i.header.title.to_string(), "Title-changed"),
            ("group", |c| c.group = changed(&c.group), |i| i.header.group.to_string(), "Group-changed"),
            ("url", |c| c.url = changed(&c.url), |i| i.header.url.to_string(), "http://host/1-changed"),
            ("logo", |c| c.logo = changed(&c.logo), |i| i.header.logo.to_string(), "http://host/logo.png-changed"),
            (
                "epg-id",
                |c| c.epg_id = Some("epg.new".to_string()),
                |i| i.header.epg_channel_id.as_deref().unwrap_or("<none>").to_string(),
                "epg.new",
            ),
            ("chno", |c| c.chno = Some(99), |i| i.header.chno.to_string(), "99"),
        ];
        for (label, edit, read, expected) in rows {
            let mut item = base_item();
            let mut channel = item_to_channel(0, &item);
            edit(&mut channel);
            let before = header_dump(&item);
            assert!(apply_channel(&mut item, channel), "{label}: no change reported");
            assert_eq!(read(&item), expected, "{label}: not written");
            assert_ne!(header_dump(&item), before, "{label}: header unchanged");
        }
    }

    #[test]
    fn writing_one_field_leaves_the_others_alone() {
        let mut item = base_item();
        let mut channel = item_to_channel(0, &item);
        channel.group = "Elsewhere".to_string();
        assert!(apply_channel(&mut item, channel));
        let mut expected = base_item();
        expected.header.group = "Elsewhere".intern();
        assert_eq!(header_dump(&item), header_dump(&expected));
    }

    #[test]
    fn host_owned_fields_are_ignored() {
        type Edit = fn(&mut Channel);
        let rows: [(&str, Edit); 6] = [
            ("handle", |c| c.handle = 12_345),
            ("item-type", |c| c.item_type = WitItemType::LocalVideo),
            ("cluster", |c| c.cluster = WitCluster::Series),
            ("input-name", |c| c.input_name = "other-input".to_string()),
            ("input-stream-id", |c| c.input_stream_id = "other-stream".to_string()),
            ("extra", |c| {
                c.extra = vec![
                    ("provider-id".to_string(), "hijacked".to_string()),
                    ("logo-small".to_string(), "hijacked".to_string()),
                    ("unknown-key".to_string(), "x".to_string()),
                ];
            }),
        ];
        for (label, edit) in rows {
            let mut item = base_item();
            item.header.item_type = PlaylistItemType::Live;
            item.header.xtream_cluster = XtreamCluster::Live;
            let before = header_dump(&item);
            let mut channel = item_to_channel(0, &item);
            edit(&mut channel);
            assert!(!apply_channel(&mut item, channel), "{label}: reported a change");
            assert_eq!(header_dump(&item), before, "{label}: header changed");
        }
    }

    #[test]
    fn handle_is_the_callers_index_and_nothing_else() {
        let item = base_item();
        for handle in [0, 1, u32::MAX] {
            assert_eq!(item_to_channel(handle, &item).handle, handle);
        }
        // A channel issued for another item under another handle changes nothing here.
        let mut target = base_item();
        let before = header_dump(&target);
        assert!(!apply_channel(&mut target, item_to_channel(77, &item)));
        assert_eq!(header_dump(&target), before);
    }

    #[test]
    fn extra_carries_every_key_in_order() {
        type Set = fn(&mut PlaylistItemHeader);
        let setters: [Set; 6] = [
            |h| h.id = "p-1".intern(),
            |h| h.logo_small = "small.png".intern(),
            |h| h.parent_code = "1234".intern(),
            |h| h.time_shift = "3".intern(),
            |h| h.rec = "1".intern(),
            |h| h.audio_track = "de".intern(),
        ];
        let values = ["p-1", "small.png", "1234", "3", "1", "de"];

        let mut item = bare_item();
        for set in setters {
            set(&mut item.header);
        }
        let expected: Vec<(String, String)> =
            EXTRA_KEYS.iter().zip(values).map(|(k, v)| ((*k).to_string(), v.to_string())).collect();
        assert_eq!(item_to_channel(0, &item).extra, expected);
        assert_eq!(EXTRA_KEYS, ["provider-id", "logo-small", "parent-code", "time-shift", "rec", "audio-track"]);
    }

    #[test]
    fn extra_omits_empty_values_and_keeps_key_order() {
        // Each row sets a subset (by EXTRA_KEYS position) in the listed order;
        // only those come back, always in key order.
        let rows: [(&[usize], &str); 6] = [
            (&[], "none"),
            (&[0], "id only"),
            (&[5], "audio-track only"),
            (&[4, 1], "rec and logo-small"),
            (&[3, 2, 0], "time-shift, parent-code, id"),
            (&[0, 1, 2, 3, 4, 5], "all"),
        ];
        for (set, label) in rows {
            let mut item = bare_item();
            for &index in set {
                let value = format!("v{index}").intern();
                let h = &mut item.header;
                match index {
                    0 => h.id = value,
                    1 => h.logo_small = value,
                    2 => h.parent_code = value,
                    3 => h.time_shift = value,
                    4 => h.rec = value,
                    _ => h.audio_track = value,
                }
            }
            let mut sorted = set.to_vec();
            sorted.sort_unstable();
            let expected: Vec<(String, String)> =
                sorted.iter().map(|&i| (EXTRA_KEYS[i].to_string(), format!("v{i}"))).collect();
            assert_eq!(item_to_channel(0, &item).extra, expected, "{label}");
        }
    }

    #[test]
    fn extra_sent_back_is_ignored() {
        let mut item = base_item();
        item.header.logo_small = "small.png".intern();
        item.header.rec = "1".intern();
        let before = header_dump(&item);
        let mut channel = item_to_channel(0, &item);
        assert_eq!(channel.extra.len(), 3); // provider-id, logo-small, rec
        for (_, value) in &mut channel.extra {
            *value = changed(value);
        }
        channel.extra.push(("audio-track".to_string(), "fr".to_string()));
        channel.extra.reverse();
        assert!(!apply_channel(&mut item, channel));
        assert_eq!(header_dump(&item), before);

        let mut channel = item_to_channel(0, &item);
        channel.extra.clear();
        assert!(!apply_channel(&mut item, channel));
        assert_eq!(header_dump(&item), before);
    }
}
