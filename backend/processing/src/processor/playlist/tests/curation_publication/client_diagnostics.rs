use super::*;
use target::build_target_tmdb_client;
use tuliprox_core::{model::ProxyConfig, utils::network::request::create_tmdb_client};

#[tokio::test]
async fn curation_tmdb_client_construction_reports_only_safe_phases_and_keeps_publication_closed() {
    for policy in ["full", "curated"] {
        for phase in ["phase=profile_configuration", "phase=client_build"] {
            let server = DiscoveryServer::start().await;
            let run = Publication::new(&server, &target(policy, true, true));
            for previously_published in [false, true] {
                if previously_published {
                    run.publish().await.unwrap();
                }
                let files = file_snapshot(run.directory.path());
                let cache = run.cache_signature().await;
                assert_eq!(!cache.is_empty(), previously_published);
                let before = server.requests.lock().unwrap().len();
                let config = app_config(run.directory.path());
                let mut settings = config.config.load().as_ref().clone();
                if phase == "phase=profile_configuration" {
                    settings.proxy = Some(ProxyConfig {
                        url: "http://[synthetic-proxy-secret".into(),
                        username: Some("synthetic-user-secret".into()),
                        password: Some("synthetic-password-secret".into()),
                    });
                }
                config.config.store(Arc::new(settings));
                let mut diagnostics = Vec::new();
                let tmdb_client = build_target_tmdb_client(
                    &run.target,
                    || {
                        create_tmdb_client(&config).map(|builder| {
                            // A controlled reqwest build error, not a new runtime fallback.
                            builder.user_agent("synthetic-header-secret\ninvalid")
                        })
                    },
                    |label| diagnostics.push(label),
                );
                assert!(tmdb_client.is_none());
                // Exact allowlist, independent of Display/source and global log sanitization:
                // no sanitizer is invoked between construction failure and this reporter.
                assert_eq!(diagnostics, [phase]);
                assert!(!diagnostics[0].contains("synthetic-"));
                let mut changed = catalog();
                changed[0].channels[0].header.title = "Must not publish".intern();
                let prepared = PreparedTarget {
                    target: run.target.clone(),
                    playlist: changed,
                    epg: Vec::new(),
                    processing: PipelineStats::default(),
                    accepted_empty_clusters: ClusterFlags::empty(),
                    library_empty: tuliprox_repository::LibraryEmptyPublication::None,
                };
                let (result, errors) = tokio::time::timeout(
                    Duration::from_secs(5),
                    target::finalize_prepared_target_with_tmdb(
                        Arc::clone(&run.context),
                        prepared,
                        tmdb_client.as_ref(),
                    ),
                )
                .await
                .unwrap();
                assert!(result.is_err());
                assert!(errors.is_empty());
                assert_eq!(file_snapshot(run.directory.path()), files);
                assert_eq!(run.cache_signature().await, cache);
                let requests = server.requests.lock().unwrap();
                assert_eq!(requests.len(), before + 1, "complete Trakt sibling cannot authorize publication");
                assert!(requests[before].starts_with(&format!("GET {TRAKT} ")));
            }
        }
    }
}

#[test]
fn curation_tmdb_disabled_or_selectorless_skips_construction_and_diagnostics() {
    for state in ["absent", "block_disabled", "source_disabled", "source_absent", "no_selectors"] {
        let mut dto = target("full", false, true);
        match state {
            "absent" => dto.curation = None,
            "block_disabled" => dto.curation.as_mut().unwrap().enabled = false,
            "source_disabled" => dto.curation.as_mut().unwrap().tmdb.as_mut().unwrap().enabled = false,
            "source_absent" => dto.curation.as_mut().unwrap().tmdb = None,
            "no_selectors" => dto.curation.as_mut().unwrap().tmdb.as_mut().unwrap().trending.clear(),
            _ => unreachable!(),
        }
        assert!(build_target_tmdb_client(
            &ConfigTarget::from(&dto),
            || panic!("{state} must not construct a client"),
            |_| panic!("{state} must not emit a construction diagnostic"),
        )
        .is_none());
    }
}

#[test]
fn curation_tmdb_successful_construction_has_no_failure_diagnostic() {
    let directory = tempdir().unwrap();
    let config = app_config(directory.path());
    assert!(build_target_tmdb_client(
        &ConfigTarget::from(&target("full", false, true)),
        || create_tmdb_client(&config),
        |_| panic!("successful construction is not a failure"),
    )
    .is_some());
}
