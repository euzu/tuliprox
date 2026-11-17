#![allow(clippy::wildcard_imports)]
use super::*;
use crate::{
    imdb::ImdbRatingRuntime,
    processor::playlist::transform::{execute_pipe_with_insertions, ManagedStageItems},
};
use shared::{
    foundation::get_filter,
    model::{
        ConfigTargetDto, NoPlugins, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        SeriesStreamProperties, StreamProperties, VideoStreamProperties, XtreamCluster,
    },
    utils::Internable,
};
use tuliprox_core::model::{ConfigInput, ConfigTarget, ImdbConfig, TransformStage};
use tuliprox_repository::{ImdbLookup, ImdbRow};

fn test_rows(rows: &[(u32, u16, u32)]) -> Vec<ImdbRow> {
    rows.iter().map(|(id, rating_x10, votes)| ImdbRow { id: *id, votes: *votes, rating_x10: *rating_x10 }).collect()
}

fn enabled_runtime(rows: &[(u32, u16, u32)]) -> ImdbRatingRuntime {
    ImdbRatingRuntime::new(
        ImdbConfig { enabled: true, min_votes: 1_000, ..ImdbConfig::default() },
        Some(ImdbLookup::from_rows(test_rows(rows))),
    )
}

fn vod_item(title: &str, provider_rating: Option<f64>) -> PlaylistItem {
    PlaylistItem {
        header: PlaylistItemHeader {
            id: title.intern(),
            name: title.intern(),
            title: title.intern(),
            group: "Movies".intern(),
            xtream_cluster: XtreamCluster::Video,
            item_type: PlaylistItemType::Video,
            additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                rating: provider_rating,
                ..VideoStreamProperties::default()
            }))),
            ..PlaylistItemHeader::default()
        },
    }
}

fn series_item(title: &str, provider_rating: f64) -> PlaylistItem {
    PlaylistItem {
        header: PlaylistItemHeader {
            id: title.intern(),
            name: title.intern(),
            title: title.intern(),
            group: "Series".intern(),
            xtream_cluster: XtreamCluster::Series,
            item_type: PlaylistItemType::SeriesInfo,
            additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                rating: provider_rating,
                ..SeriesStreamProperties::default()
            }))),
            ..PlaylistItemHeader::default()
        },
    }
}

fn filtered_target(filter: &str) -> ConfigTarget {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.filter = get_filter(filter, None).expect("filter should parse").into();
    target
}

async fn run_pipeline(
    target: &ConfigTarget,
    groups: Vec<PlaylistGroup>,
    runtime: &ImdbRatingRuntime,
) -> Vec<PlaylistItem> {
    let input = ConfigInput { name: "provider".intern(), ..ConfigInput::default() };
    let mut fpl = crate::fetched_playlist::FetchedPlaylist {
        input: &input,
        source: MemoryPlaylistSource::new(groups).into_source(),
        epg: None,
    };
    let (mut result, _) = execute_pipe_with_insertions(
        target,
        &vec![TransformStage::Filter],
        &mut fpl,
        &mut std::collections::HashSet::default(),
        false,
        None,
        &NoPlugins,
        None,
        ManagedStageItems::default(),
        runtime,
    )
    .await
    .expect("pipeline should not abort");
    result.items().map(std::borrow::Cow::into_owned).collect()
}

#[tokio::test]
async fn rating_filter_keeps_only_entries_above_the_imdb_threshold() {
    // 8.2 with enough votes, 6.4 below the filter, 4.5 far below, and one title without an id.
    let runtime = enabled_runtime(&[(1_111_111, 82, 5_000), (2_222_222, 64, 5_000), (3_333_333, 45, 5_000)]);
    let groups = vec![PlaylistGroup {
        id: 1,
        title: "Movies".intern(),
        xtream_cluster: XtreamCluster::Video,
        channels: vec![
            vod_item("Good movie [tt1111111]", None),
            vod_item("Borderline movie imdb=tt2222222", None),
            vod_item("Bad movie imdbid-tt3333333", None),
            vod_item("Provider rated movie", Some(7.1)),
            vod_item("Unrated movie", None),
        ],
    }];

    let items = run_pipeline(&filtered_target("Rating >= 6.5"), groups, &runtime).await;

    let mut titles = items.iter().map(|item| item.header.title.to_string()).collect::<Vec<_>>();
    titles.sort();
    assert_eq!(titles, vec!["Good movie [tt1111111]".to_string(), "Provider rated movie".to_string()]);
}

#[tokio::test]
async fn a_target_without_a_filter_still_stores_the_rating_fields() {
    let runtime = enabled_runtime(&[(6_549_510, 42, 5_000)]);
    let groups = vec![PlaylistGroup {
        id: 1,
        title: "Series".intern(),
        xtream_cluster: XtreamCluster::Series,
        channels: vec![series_item("Series {imdb-tt6549510}", 3.0)],
    }];
    let target = ConfigTarget::from(&ConfigTargetDto::default());

    let items = run_pipeline(&target, groups, &runtime).await;

    assert_eq!(items.len(), 1);
    match items[0].header.additional_properties.as_ref() {
        Some(StreamProperties::Series(series)) => {
            assert_eq!(series.imdb, Some(6_549_510));
            assert_eq!(series.imdb_rating, Some(4.2));
            assert_eq!(series.imdb_votes, Some(5_000));
            assert_eq!(series.effective_rating, Some(4.2));
            // The provider value stays stored: a later update must still be able to fall back.
            assert_eq!(series.rating, 3.0);
        }
        other => panic!("expected series properties, got {other:?}"),
    }
}

#[tokio::test]
async fn votes_filter_uses_the_raw_imdb_vote_count() {
    let runtime = enabled_runtime(&[(1_111_111, 82, 5_000), (2_222_222, 79, 400)]);
    let groups = vec![PlaylistGroup {
        id: 1,
        title: "Movies".intern(),
        xtream_cluster: XtreamCluster::Video,
        channels: vec![vod_item("Many votes [tt1111111]", None), vod_item("Few votes [tt2222222]", None)],
    }];

    let items = run_pipeline(&filtered_target("Votes >= 1000"), groups, &runtime).await;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].header.title.as_ref(), "Many votes [tt1111111]");
}
