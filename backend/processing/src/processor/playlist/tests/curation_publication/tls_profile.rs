//! Exercise the configured profile against real TLS, without changing host trust or contacting TMDB.
use super::*;
use reqwest::header::AUTHORIZATION;
use tuliprox_core::utils::network::request::{create_client, create_tmdb_client};

fn insecure_config(directory: &std::path::Path) -> Arc<AppConfig> {
    let config = app_config(directory);
    let mut settings = config.config.load().as_ref().clone();
    settings.accept_insecure_ssl_certificates = true;
    config.config.store(Arc::new(settings));
    config
}

fn local_client(builder: reqwest::ClientBuilder, server: &DiscoveryServer, hostname: &str) -> reqwest::Client {
    builder.no_proxy().resolve(hostname, server.address).timeout(Duration::from_secs(3)).build().unwrap()
}

#[tokio::test]
async fn tmdb_profile_tls_rejects_untrusted_certificate_even_with_global_insecure_flag() {
    let server = DiscoveryServer::start().await;
    let directory = tempdir().unwrap();
    let config = insecure_config(directory.path());
    let client = local_client(create_tmdb_client(&config).unwrap(), &server, "api.themoviedb.org");
    let result = client
        .get(format!("https://api.themoviedb.org{MOVIES}"))
        .header(AUTHORIZATION, "Bearer synthetic-tls-secret")
        .send()
        .await;
    assert!(result.is_err(), "discovery must not inherit the global TLS bypass");
    assert!(result.unwrap_err().is_connect());
    assert!(server.requests.lock().unwrap().is_empty(), "TCP/TLS is not an HTTP request or Bearer disclosure");
}

#[tokio::test]
async fn tmdb_profile_tls_preserves_trusted_ca_and_checks_hostname_with_global_insecure_flag() {
    let server = DiscoveryServer::start().await;
    let directory = tempdir().unwrap();
    let config = insecure_config(directory.path());
    for (hostname, trusted_name) in [("api.themoviedb.org", true), ("wrong-name.invalid", false)] {
        let client = local_client(
            create_tmdb_client(&config).unwrap().add_root_certificate(server.certificate.clone()),
            &server,
            hostname,
        );
        let before = server.requests.lock().unwrap().len();
        let result = client
            .get(format!("https://{hostname}{MOVIES}"))
            .header(AUTHORIZATION, "Bearer synthetic-tls-secret")
            .send()
            .await;
        if trusted_name {
            assert_eq!(result.unwrap().text().await.unwrap(), MOVIE_PAGE);
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), before + 1);
            assert!(requests[before].to_lowercase().contains("authorization: bearer synthetic-tls-secret"));
        } else {
            assert!(result.unwrap_err().is_connect());
            assert_eq!(server.requests.lock().unwrap().len(), before, "wrong hostname must not receive HTTP");
        }
    }
}

#[tokio::test]
async fn tmdb_profile_tls_does_not_change_generic_clients_global_insecure_policy() {
    let server = DiscoveryServer::start().await;
    let directory = tempdir().unwrap();
    let config = insecure_config(directory.path());
    let client = local_client(create_client(&config), &server, "api.trakt.tv");
    let response = client.get(format!("https://api.trakt.tv{TRAKT}")).send().await.unwrap();
    assert_eq!(response.text().await.unwrap(), TRAKT_PAGE);
    assert_eq!(server.requests.lock().unwrap().len(), 1, "generic profile still accepts the untrusted certificate");
}

#[tokio::test]
async fn tmdb_profile_tls_failure_blocks_publication_and_retains_artifacts_even_with_complete_trakt() {
    for policy in ["full", "curated"] {
        let server = DiscoveryServer::start().await;
        let mut run = Publication::new(&server, &target(policy, true, true));
        let config = insecure_config(run.directory.path());
        let rejected = local_client(create_tmdb_client(&config).unwrap(), &server, "api.themoviedb.org");
        let trusted = local_client(
            create_tmdb_client(&config).unwrap().add_root_certificate(server.certificate.clone()),
            &server,
            "api.themoviedb.org",
        );
        for previously_published in [false, true] {
            if previously_published {
                run.tmdb_client = trusted.clone();
                run.publish().await.unwrap();
            }
            let files = file_snapshot(run.directory.path());
            let cache = run.cache_signature().await;
            assert_eq!(!cache.is_empty(), previously_published);
            let before = server.requests.lock().unwrap().len();
            run.tmdb_client = rejected.clone();
            let mut changed = catalog();
            changed[0].channels[0].header.title = "Must not publish".intern();
            assert!(run.publish_catalog(changed).await.is_err());
            assert_eq!(file_snapshot(run.directory.path()), files, "all files, IDs and watches retained");
            assert_eq!(run.cache_signature().await, cache, "no partial cache publication");
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), before + 1, "only the required Trakt sibling reached HTTP; no fallback");
            assert!(requests[before].starts_with(&format!("GET {TRAKT} ")));
        }
    }
}
