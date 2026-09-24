use super::*;
use crate::test_support::with_paused_io;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn missing_transport_fails_every_tmdb_selector_including_selection_only_after_complete_trakt() {
    for curated in [false, true] {
        let server = TestServer::new(http_response(200, TRAKT)).await;
        let mut config = config(Some(&server.url));
        config.catalog_selection = if curated {
            shared::model::CurationCatalogSelection::Curated
        } else {
            shared::model::CurationCatalogSelection::Full
        };
        config.tmdb.as_mut().unwrap().trending[0].create_xtream_category = false;
        let outcome = evaluate_curation(&reqwest::Client::new(), None, &catalog(), "test", &config).await;
        let CurationRunOutcome::Failed(failure) = outcome else { panic!("missing transport must fail admission") };
        assert_eq!(failure.selector_outcomes.len(), 3);
        assert!(matches!(&failure.selector_outcomes[0], SelectorOutcome::Complete { key, memberships, .. }
            if *key == CurationSelectorKey(0) && memberships.len() == 2));
        for (outcome, key) in failure.selector_outcomes[1..].iter().zip([1, 2]) {
            assert_eq!(
                *outcome,
                SelectorOutcome::Unavailable {
                    key: CurationSelectorKey(key),
                    reason: CurationUnavailableReason::Configuration,
                }
            );
        }
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn tmdb_batch_clock_starts_after_trakt_acquisition_not_client_construction() {
    let (trakt, mut trakt_requests) = TestServer::controlled().await;
    let (tmdb, mut tmdb_requests) = TestServer::controlled().await;
    let config = config(Some(&trakt.url));
    let playlist = catalog();
    let (outcome, ()) = with_paused_io(async {
        tokio::join!(run(&config, &playlist, &tmdb), async {
            let mut stream = trakt_requests.recv().await.unwrap();
            tokio::time::advance(Duration::from_secs(181)).await;
            stream.write_all(http_response(200, TRAKT).as_bytes()).await.unwrap();
            for _ in 0..2 {
                let mut stream = tmdb_requests.recv().await.unwrap();
                stream.write_all(http_response(200, PAGE).as_bytes()).await.unwrap();
            }
        })
    })
    .await;
    let CurationRunOutcome::Complete(evaluation) = outcome else { panic!("Trakt is outside the TMDB clock") };
    assert_eq!(evaluation.selectors.len(), 3);
    assert_eq!(tmdb.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn mixed_selectors_keep_keys_ranks_and_selection_only_slots_through_projection() {
    let list = r#"[{"id":1,"rank":4,"listed_at":"fixture","type":"movie","movie":{"title":"Film","ids":{"tmdb":7,"trakt":1,"slug":"film"}}}]"#;
    let trakt =
        TestServer::sequence(vec![http_response(200, list), http_response(200, list), http_response(200, TRAKT)]).await;
    let tmdb = TestServer::new(http_response(200, PAGE)).await;
    let dto: CurationConfigDto = serde_json::from_value(json!({
        "trakt": {"api":{"api_key":"test-client","url":trakt.url},
            "lists": [
                {"user":"fixture","list_slug":"selection","content_type":"vod","create_xtream_category":false,"tmdb_only":true},
                {"user":"fixture","list_slug":"projected","content_type":"vod","category_name":"List","tmdb_only":true}
            ],
            "charts":[{"kind":"movies","chart":"popular","category_name":"Chart","tmdb_only":true}]},
        "tmdb": {"api":{"access_token":"test-token"},"trending":[
            {"kind":"movie","time_window":"week","limit":1,"create_xtream_category":false},
            {"kind":"tv","time_window":"day","limit":1,"category_name":"TV"}
        ]}
    })).unwrap();
    let config = CurationConfig::from(&dto);
    let playlist = catalog();
    let CurationRunOutcome::Complete(result) = run(&config, &playlist, &tmdb).await else {
        panic!("all selectors complete")
    };
    assert_eq!(
        result.selectors.iter().map(|s| (s.key.0, s.membership_count)).collect::<Vec<_>>(),
        [(0, 2), (1, 2), (2, 2), (3, 2), (4, 1)]
    );
    assert_eq!(result.memberships[0].rank, Some(4));
    let groups = project_curation_categories(&result, &playlist, &config);
    assert_eq!(groups.iter().map(|g| g.title.as_ref()).collect::<Vec<_>>(), ["List", "Chart", "TV"]);
    assert_eq!(trakt.requests.lock().unwrap().len(), 3);
    assert_eq!(tmdb.requests.lock().unwrap().len(), 2);
}
