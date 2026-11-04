#[path = "support/origin_restart_fixture.rs"]
mod fixture;

use fixture::{bytes, client, Fixture, ProviderCase};
use reqwest::{StatusCode, Url};
use std::{sync::atomic::Ordering, time::Duration};
use tuliprox_testkit::{hls_probe::StartupMode, TestkitError};

fn resources(base: &Url, body: &str) -> Vec<Url> {
    body.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| base.join(line).unwrap())
        .collect()
}
fn sequence(body: &str) -> u64 {
    body.lines().find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")).unwrap().parse().unwrap()
}

async fn variant(fixture: &Fixture, player: &reqwest::Client) -> Result<Url, TestkitError> {
    let response = player.get(&fixture.entry).send().await?.error_for_status()?;
    let base = response.url().clone();
    let body = response.text().await?;
    assert!(body.contains("#EXT-X-STREAM-INF"));
    Ok(resources(&base, &body).remove(0))
}

async fn terminalize(fixture: &Fixture, player: &reqwest::Client, old: &Url) -> Result<(), TestkitError> {
    fixture.origin.phase.store(1, Ordering::Release);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(18);
    while !fixture.log_contains("outcome=terminal_committed")
        && !fixture.log_contains("HLS autonomous terminal owner completed: outcome=committed")
    {
        let response = player.get(old.clone()).send().await?;
        let _ = response.bytes().await?;
        assert!(tokio::time::Instant::now() < deadline, "old entry must become terminal under repeated 407 responses");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let fault_requests = fixture
        .origin
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|path| path.split('?').next().is_some_and(|path| path.ends_with("index.m3u8")))
        .count();
    assert!(fault_requests >= 3);
    // Terminal publication is observed above. Honor the existing configured
    // failure backoff before creating a fresh entry; no retry deadline or
    // budget is reset by the fixture.
    tokio::time::sleep(fixture::MAX_MANIFEST_FAILURE_BACKOFF).await;
    Ok(())
}

async fn run(mode: StartupMode, unchanged_first: bool) -> Result<(), TestkitError> {
    let fixture = Fixture::start(mode, unchanged_first).await?;
    let player = client("origin-restart-old-client");
    let old = variant(&fixture, &player).await?;
    let response = player.get(old.clone()).send().await?.error_for_status()?;
    let old_body = response.text().await?;
    assert!(!old_body.contains("#EXT-X-ENDLIST"));
    let old_sequence = sequence(&old_body);
    let old_resources = resources(&old, &old_body);
    assert_eq!(
        old_sequence + u64::try_from(old_resources.len()).unwrap(),
        5,
        "the original live window ends at origin highwater 4"
    );
    let mut saved = Vec::new();
    for (index, url) in old_resources.iter().enumerate() {
        let body = player.get(url.clone()).send().await?.error_for_status()?.bytes().await?;
        assert_eq!(body.as_ref(), bytes(0, old_sequence + u64::try_from(index).unwrap()));
        saved.push(body);
    }
    terminalize(&fixture, &player, &old).await?;
    fixture.origin.phase.store(2, Ordering::Release);
    let new_player = client("origin-restart-new-client");
    let new = variant(&fixture, &new_player).await?;
    assert_ne!(new, old, "new access lease");
    let response = new_player.get(new.clone()).send().await?;
    assert_eq!(response.status(), StatusCode::OK, "qualified restart must produce live media");
    let new_body = response.text().await?;
    assert!(!new_body.contains("#EXT-X-ENDLIST"));
    assert!(new_body.contains("#EXT-X-DISCONTINUITY"));
    let new_sequence = sequence(&new_body);
    assert!(new_sequence >= 5);
    assert_eq!(new_sequence + u64::try_from(resources(&new, &new_body).len()).unwrap(), 9);
    for (index, url) in resources(&new, &new_body).iter().enumerate() {
        assert_eq!(
            new_player.get(url.clone()).send().await?.error_for_status()?.bytes().await?.as_ref(),
            bytes(1, new_sequence - 5 + u64::try_from(index).unwrap())
        );
    }
    let old_tail = old_resources.last().unwrap();
    let old_tail_bytes = saved.last().unwrap();
    assert_eq!(
        &player.get(old_tail.clone()).send().await?.error_for_status()?.bytes().await?,
        old_tail_bytes,
        "terminal tail keeps its previous origin bytes"
    );
    fixture.origin.phase.store(3, Ordering::Release);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let body = new_player.get(new.clone()).send().await?.error_for_status()?.text().await?;
        if resources(&new, &body).last().is_some_and(|url| url.path().ends_with("000009.ts")) {
            assert!(!body.contains("#EXT-X-ENDLIST"));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "restart must continue");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let requests = fixture.origin.requests.lock().unwrap().len();
    assert!(requests < 150, "bounded origin requests");
    if unchanged_first {
        assert!(fixture.origin.reentry_manifests.load(Ordering::Acquire) >= 2);
    }
    println!("mode={mode} unchanged_first={unchanged_first} old_sequence=0 new_sequence=5 requests={requests} new_media_verified=true old_lease_verified=true");
    fixture.stop(&format!("{mode}-{unchanged_first}")).await
}

#[tokio::test]
#[ignore = "requires built proxy selected by TULIPROX_TESTKIT_SUT_BINARY"]
async fn client_recovers_after_407_with_new_origin_generation() -> Result<(), TestkitError> {
    for mode in [StartupMode::FirstReady, StartupMode::Progressive, StartupMode::Optimistic] {
        for unchanged_first in [false, true] {
            run(mode, unchanged_first).await?;
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires built proxy selected by TULIPROX_TESTKIT_SUT_BINARY"]
async fn repeated_old_media_exhausts_reentry_without_publishing_a_restart() -> Result<(), TestkitError> {
    for mode in [StartupMode::FirstReady, StartupMode::Progressive, StartupMode::Optimistic] {
        let fixture = Fixture::start_replay(mode).await?;
        let old_player = client("origin-replay-old-client");
        let old = variant(&fixture, &old_player).await?;
        let body = old_player.get(old.clone()).send().await?.error_for_status()?.text().await?;
        let old_sequence = sequence(&body);
        let old_resources = resources(&old, &body);
        assert!(!body.contains("#EXT-X-ENDLIST"));
        let mut saved = Vec::new();
        for (index, url) in old_resources.iter().enumerate() {
            let bytes = old_player.get(url.clone()).send().await?.error_for_status()?.bytes().await?;
            assert_eq!(bytes.as_ref(), fixture::bytes(0, old_sequence + u64::try_from(index).unwrap()));
            saved.push(bytes);
        }
        terminalize(&fixture, &old_player, &old).await?;
        fixture.origin.phase.store(2, Ordering::Release);
        let player = client("origin-replay-new-client");
        let new = variant(&fixture, &player).await?;
        let response = player.get(new).send().await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await?;
        assert!(body.contains("#EXT-X-ENDLIST"), "unchanged media must finish in the existing CVR");
        assert!(fixture.log_contains("decision=unchanged-media"));
        assert!(!fixture.log_contains("decision=restart-committed"));
        assert!(fixture.log_contains("completion=budget-exhausted"));
        let requests = fixture
            .origin
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                request.starts_with("phase=2 ")
                    && request.split('?').next().is_some_and(|path| path.ends_with("index.m3u8"))
            })
            .count();
        assert!((2..=5).contains(&requests), "existing manifest budget: {requests}");
        assert_eq!(
            old_player.get(old_resources.last().unwrap().clone()).send().await?.error_for_status()?.bytes().await?,
            *saved.last().unwrap()
        );
        println!("mode={mode} scenario=repeated-old-media manifest_requests={requests} completion=budget-exhausted old_lease_verified=true");
        fixture.stop(&format!("{mode}-replay-only")).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires built proxy selected by TULIPROX_TESTKIT_SUT_BINARY"]
async fn provider_hls_diagnostics_protect_transport_values_under_both_sanitization_settings() -> Result<(), TestkitError>
{
    for sanitize in [true, false] {
        for case in [
            ProviderCase::Failover,
            ProviderCase::Authentication,
            ProviderCase::Status,
            ProviderCase::Connection,
            ProviderCase::Timeout,
            ProviderCase::Redirect,
        ] {
            let fixture = Fixture::start_provider(sanitize, case).await?;
            let player = client("provider-hls-diagnostics");
            let manifest = variant(&fixture, &player).await?;
            let response = player.get(manifest.clone()).send().await?;
            let body = response.text().await?;
            if matches!(case, ProviderCase::Failover) {
                assert!(!body.contains("#EXT-X-ENDLIST"));
                let sequence = sequence(&body);
                for (index, url) in resources(&manifest, &body).iter().enumerate() {
                    assert_eq!(
                        player.get(url.clone()).send().await?.error_for_status()?.bytes().await?.as_ref(),
                        bytes(0, sequence + u64::try_from(index).unwrap())
                    );
                }
                assert!(fixture.log_contains("switching from URL index 0 to 1"));
            } else {
                assert!(body.contains("#EXT-X-ENDLIST"), "provider failure must finish in the existing CVR: {case:?}");
            }
            let requests = fixture.origin.requests.lock().unwrap().clone();
            let hls_requests: Vec<_> = requests.iter().filter(|request| !request.contains("/catalog/")).collect();
            assert!(!hls_requests.is_empty(), "the configured provider must reach the HLS origin");
            assert!(hls_requests.len() <= 60, "provider work stays within the existing retry and redirect limits");
            assert!(
                hls_requests.iter().all(|request| request.contains(fixture::DIAGNOSTIC_AUTH_PATH)
                    && request.contains(fixture::DIAGNOSTIC_QUERY)
                    && request.contains(fixture::DIAGNOSTIC_TOKEN)),
                "transport must preserve every required path and query marker"
            );
            let logs = std::fs::read_to_string(&fixture.log)?;
            assert!(logs.contains("Provider 'fixture' acquiring URL"));
            match case {
                ProviderCase::Status => assert!(logs.contains("exhausted all") && logs.contains("status 503")),
                ProviderCase::Connection => assert!(logs.contains("connection error while trying")),
                ProviderCase::Timeout => {
                    assert!(logs.contains("idle timeout while trying") || logs.contains("HLS timeout error"));
                }
                ProviderCase::Redirect => assert!(
                    hls_requests.iter().any(|request| request.contains("opaque-target"))
                        && logs.contains("completion=budget-exhausted")
                ),
                ProviderCase::Authentication => assert!(logs.contains("status=407")),
                ProviderCase::Failover => {}
            }
            let label = format!("provider-{sanitize}-{case:?}").to_lowercase();
            // Persist a failing case too, so the same fixture provides a red log.
            fixture.stop(&label).await?;
            for marker in
                ["synthetic-auth-path", "synthetic-opaque", "synthetic-token", "synthetic-user", "synthetic-password"]
            {
                assert!(!logs.contains(marker), "HLS diagnostic contains synthetic secret marker: {label} {marker}");
            }
            println!(
                "case={label} origin_requests={} full_transport_verified=true privacy_verified=true",
                hls_requests.len()
            );
        }
    }
    Ok(())
}
