use super::{collect_effective_skip_clusters, has_probe_details, needs_live_probe};
use shared::{
    model::{InputType, PlaylistItem, StreamProperties, XtreamCluster},
    utils::Internable,
};
use tuliprox_core::model::ConfigInput;

pub(in crate::processor::playlist::tests) fn item_with_props(props: StreamProperties) -> PlaylistItem {
    let header = shared::model::PlaylistItemHeader { additional_properties: Some(props), ..Default::default() };
    PlaylistItem { header }
}

pub(in crate::processor::playlist::tests) fn live_item_with_probe_timestamp_and_bitrate(
    last_probed_timestamp: i64,
    bitrate: u32,
) -> PlaylistItem {
    item_with_props(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
        video: Some("{\"codec_name\":\"h264\"}".intern()),
        audio: Some("{\"codec_name\":\"aac\"}".intern()),
        bitrate,
        last_probed_timestamp: Some(last_probed_timestamp),
        ..Default::default()
    })))
}

#[test]
fn has_probe_details_requires_video_and_audio_for_video() {
    let video = shared::model::VideoStreamProperties {
        details: Some(shared::model::VideoStreamDetailProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let item_missing_audio = item_with_props(StreamProperties::Video(Box::new(video)));
    assert!(!has_probe_details(&item_missing_audio));

    let video_complete = shared::model::VideoStreamProperties {
        details: Some(shared::model::VideoStreamDetailProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: Some("{\"codec_name\":\"aac\"}".intern()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let item_complete = item_with_props(StreamProperties::Video(Box::new(video_complete)));
    assert!(has_probe_details(&item_complete));
}

#[test]
fn has_probe_details_requires_video_audio_and_bitrate_for_live() {
    let live_missing_audio = shared::model::LiveStreamProperties {
        video: Some("{\"codec_name\":\"h264\"}".intern()),
        audio: None,
        ..Default::default()
    };
    let item_missing_audio = item_with_props(StreamProperties::Live(Box::new(live_missing_audio)));
    assert!(!has_probe_details(&item_missing_audio));

    let live_missing_bitrate = shared::model::LiveStreamProperties {
        video: Some("{\"codec_name\":\"h264\"}".intern()),
        audio: Some("{\"codec_name\":\"aac\"}".intern()),
        ..Default::default()
    };
    let item_missing_bitrate = item_with_props(StreamProperties::Live(Box::new(live_missing_bitrate)));
    assert!(!has_probe_details(&item_missing_bitrate));

    let live_complete = shared::model::LiveStreamProperties {
        video: Some("{\"codec_name\":\"h264\"}".intern()),
        audio: Some("{\"codec_name\":\"aac\"}".intern()),
        bitrate: 2_500_000,
        ..Default::default()
    };
    let item_complete = item_with_props(StreamProperties::Live(Box::new(live_complete)));
    assert!(has_probe_details(&item_complete));
}

#[test]
fn needs_live_probe_when_fresh_probe_has_no_bitrate() {
    let item = live_item_with_probe_timestamp_and_bitrate(101, 0);

    assert!(needs_live_probe(&item, 100));
}

#[test]
fn does_not_need_live_probe_when_fresh_probe_has_positive_bitrate() {
    let item = live_item_with_probe_timestamp_and_bitrate(101, 2_500_000);

    assert!(!needs_live_probe(&item, 100));
}

#[test]
fn needs_live_probe_when_positive_bitrate_probe_is_older_than_cutoff() {
    let item = live_item_with_probe_timestamp_and_bitrate(99, 2_500_000);

    assert!(needs_live_probe(&item, 100));
}

#[test]
fn has_probe_details_is_false_for_series() {
    let series = shared::model::SeriesStreamProperties::default();
    let item = item_with_props(StreamProperties::Series(Box::new(series)));
    assert!(!has_probe_details(&item));
}

#[test]
fn collect_effective_skip_clusters_uses_input_skip_flags() {
    use tuliprox_core::model::{ConfigInputFlags, ConfigInputOptions};
    let input = ConfigInput {
        name: "skip_live".intern(),
        input_type: InputType::Xtream,
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::SkipLive.into(),
            ..ConfigInputOptions::defaults().clone()
        }),
        ..ConfigInput::default()
    };
    let skip = collect_effective_skip_clusters(&input);
    assert!(skip.contains(&XtreamCluster::Live));
    assert!(!skip.contains(&XtreamCluster::Video));
    assert!(!skip.contains(&XtreamCluster::Series));
}
