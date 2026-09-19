use super::*;
use crate::{
    kernel::{CurationMediaKind, CurationUnavailableReason},
    test_support::{http_response, TestServer},
};
use serde_json::json;
use shared::{
    model::{
        CurationConfigDto, PlaylistItem, PlaylistItemHeader, PlaylistItemType, SeriesStreamProperties,
        StreamProperties, VideoStreamProperties, XtreamCluster,
    },
    utils::{hash_string, Internable},
};

fn catalog() -> Vec<PlaylistGroup> {
    let mut groups = Vec::new();
    for (label, id, series) in [("movie", 7, false), ("alias", 7, false), ("other", 8, false), ("show", 7, true)] {
        let cluster = if series { XtreamCluster::Series } else { XtreamCluster::Video };
        groups.push(PlaylistGroup {
            id: 0,
            title: label.intern(),
            xtream_cluster: cluster,
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: hash_string(label),
                    id: label.intern(),
                    title: label.intern(),
                    name: label.intern(),
                    group: label.intern(),
                    xtream_cluster: cluster,
                    item_type: if series { PlaylistItemType::SeriesInfo } else { PlaylistItemType::Video },
                    additional_properties: Some(if series {
                        StreamProperties::Series(Box::new(SeriesStreamProperties {
                            tmdb: Some(id),
                            ..SeriesStreamProperties::default()
                        }))
                    } else {
                        StreamProperties::Video(Box::new(VideoStreamProperties {
                            tmdb: Some(id),
                            ..VideoStreamProperties::default()
                        }))
                    }),
                    ..PlaylistItemHeader::default()
                },
            }],
        });
    }
    groups
}

fn config(trakt_url: Option<&str>) -> CurationConfig {
    let mut value = json!({"tmdb": {"api": {"access_token": "test-token"}, "trending": [
        {"kind": "movie", "time_window": "week", "scope": "first_page", "category_name": "TMDB Movies"},
        {"kind": "tv", "time_window": "day", "scope": "first_page", "category_name": "TMDB TV"}
    ]}});
    if let Some(url) = trakt_url {
        value["trakt"] = json!({"api": {"api_key": "test-client", "url": url}, "charts": [{"kind": "movies", "chart": "popular", "category_name": "Trakt", "tmdb_only": true}]});
    }
    CurationConfig::from(&serde_json::from_value::<CurationConfigDto>(value).unwrap())
}

async fn run(config: &CurationConfig, playlist: &[PlaylistGroup], server: &TestServer) -> CurationRunOutcome {
    let http = reqwest::Client::new();
    let client = config.tmdb.as_ref().map(|source| TmdbClient::for_test(&http, &source.api, &server.url));
    evaluate_with_tmdb(&http, playlist, "test", config, client).await
}

const PAGE: &str = r#"{"page":1,"total_pages":10,"total_results":100,"results":[{"id":7}]}"#;
const TRAKT: &str = r#"[{"title":"Film","year":2024,"ids":{"tmdb":7,"trakt":1,"slug":"film"}}]"#;

#[tokio::test]
async fn tmdb_only_selects_exact_same_kind_subjects_and_retains_local_aliases() {
    let server = TestServer::new(http_response(200, PAGE)).await;
    let config = config(None);
    let playlist = catalog();
    let CurationRunOutcome::Complete(result) = run(&config, &playlist, &server).await else {
        panic!("TMDB-only succeeds without Trakt identity")
    };
    assert_eq!(result.selectors.iter().map(|s| (s.key.0, s.membership_count)).collect::<Vec<_>>(), [(0, 2), (1, 1)]);
    assert_eq!(
        result.memberships.iter().map(|m| (m.subject_uuid, m.media_kind)).collect::<Vec<_>>(),
        [
            (hash_string("movie"), CurationMediaKind::Movie),
            (hash_string("alias"), CurationMediaKind::Movie),
            (hash_string("show"), CurationMediaKind::Series),
        ]
    );
    let groups = project_curation_categories(&result, &playlist, &config);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "TMDB Movies");
    assert_eq!(
        groups[0].channels[0].header.uuid,
        hash_string(&format!("trakt-category:TMDB Movies:{}", hash_string("movie")))
    );
    assert_eq!(groups[1].xtream_cluster, XtreamCluster::Series);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn mixed_sources_use_unique_keys_and_the_same_pre_projection_snapshot() {
    let trakt = TestServer::new(http_response(200, TRAKT)).await;
    let tmdb = TestServer::new(http_response(200, PAGE)).await;
    let config = config(Some(&trakt.url));
    let playlist = catalog();
    let CurationRunOutcome::Complete(result) = run(&config, &playlist, &tmdb).await else {
        panic!("both sources succeed")
    };
    assert_eq!(
        result.selectors.iter().map(|s| (s.key.0, s.membership_count)).collect::<Vec<_>>(),
        [(0, 2), (1, 2), (2, 1)]
    );
    assert_eq!(result.memberships.len(), 5, "later adapters must not match earlier projections");
    let groups = project_curation_categories(&result, &playlist, &config);
    assert_eq!(groups.iter().map(|g| g.title.as_ref()).collect::<Vec<_>>(), ["Trakt", "TMDB Movies", "TMDB TV"]);
    assert_ne!(groups[0].channels[0].header.uuid, groups[1].channels[0].header.uuid);
    assert_eq!(trakt.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn either_failed_provider_blocks_the_whole_run_under_both_policies() {
    for curated in [false, true] {
        for tmdb_fails in [false, true] {
            let trakt = TestServer::new(http_response(if tmdb_fails { 200 } else { 503 }, TRAKT)).await;
            let tmdb = TestServer::new(http_response(if tmdb_fails { 503 } else { 200 }, PAGE)).await;
            let mut config = config(Some(&trakt.url));
            if curated {
                config.catalog_selection = shared::model::CurationCatalogSelection::Curated;
            }
            let CurationRunOutcome::Failed(failure) = run(&config, &catalog(), &tmdb).await else {
                panic!("successful siblings are not publishable partial data")
            };
            assert_eq!(failure.selector_outcomes.len(), 3);
            assert_eq!(
                failure.selector_outcomes.iter().filter(|o| matches!(o, SelectorOutcome::Complete { .. })).count(),
                if tmdb_fails { 1 } else { 2 }
            );
        }
    }
}

#[tokio::test]
async fn empty_and_no_match_are_complete_but_disabled_sources_are_not_configured() {
    for body in [
        r#"{"page":1,"total_pages":0,"total_results":0,"results":[]}"#,
        r#"{"page":1,"total_pages":1,"total_results":1,"results":[{"id":999,"title":"movie","name":"show"}]}"#,
    ] {
        let server = TestServer::new(http_response(200, body)).await;
        let CurationRunOutcome::Complete(result) = run(&config(None), &catalog(), &server).await else {
            panic!("valid empty or no-match is complete")
        };
        assert!(result.memberships.is_empty());
        assert_eq!(result.selectors.len(), 2);
    }
    let server = TestServer::new(http_response(200, PAGE)).await;
    let mut disabled = config(None);
    disabled.enabled = false;
    assert_eq!(run(&disabled, &catalog(), &server).await, CurationRunOutcome::NotConfigured);
    disabled.enabled = true;
    disabled.tmdb.as_mut().unwrap().enabled = false;
    assert_eq!(run(&disabled, &catalog(), &server).await, CurationRunOutcome::NotConfigured);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_credentials_mark_every_required_selector_with_its_assigned_key() {
    let server = TestServer::new(http_response(200, PAGE)).await;
    let mut config = config(Some(&server.url));
    config.trakt.as_mut().unwrap().api.api_key.clear();
    config.tmdb.as_mut().unwrap().api.access_token.clear();
    let CurationRunOutcome::Failed(result) = run(&config, &catalog(), &server).await else {
        panic!("credentials unavailable")
    };
    assert_eq!(
        result.selector_outcomes,
        (0..3)
            .map(|key| SelectorOutcome::Unavailable {
                key: CurationSelectorKey(key),
                reason: CurationUnavailableReason::Configuration
            })
            .collect::<Vec<_>>()
    );
    assert!(server.requests.lock().unwrap().is_empty());
}
