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
async fn provider_hls_diagnostics_follow_the_global_sanitization_setting() -> Result<(), TestkitError> {
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
            let mut manifest = variant(&fixture, &player).await?;
            assert!(manifest.path().starts_with("/hls/shared/live/"));
            manifest.set_query(Some("token=synthetic-client-token&opaque=synthetic-client-opaque"));
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
                    assert!(logs.contains("idle timeout while trying") || logs.contains("Request timed out"));
                }
                ProviderCase::Redirect => assert!(
                    hls_requests.iter().any(|request| request.contains("opaque-target"))
                        && logs.contains("completion=budget-exhausted")
                ),
                ProviderCase::Authentication => assert!(logs.contains("status=407")),
                ProviderCase::Failover => {}
            }
            let label = format!("provider-{sanitize}-{case:?}").to_lowercase();
            let client_query = if sanitize {
                "token=***&opaque=***"
            } else {
                "token=synthetic-client-token&opaque=synthetic-client-opaque"
            };
            let client_reference = format!("{}?{client_query}", manifest.path());
            assert!(
                logs.lines().any(|line| line.contains("Client request") && line.contains(&client_reference)),
                "client request log must preserve the HLS path and follow the global query policy: {label}"
            );
            // Persist a failing case too, so the same fixture provides a red log.
            fixture.stop(&label).await?;
            assert_provider_diagnostic_policy(&logs, sanitize, case, &label);
            println!(
                "case={label} origin_requests={} full_transport_verified=true sanitization_policy_verified=true",
                hls_requests.len()
            );
        }
    }
    Ok(())
}

fn assert_provider_diagnostic_policy(logs: &str, sanitize: bool, case: ProviderCase, label: &str) {
    if sanitize {
        assert!(
            logs.contains("/hls/17/synthetic-auth-path/index.m3u8?token=***&opaque=***"),
            "HLS diagnostic must keep the URL structure and query names: {label}"
        );
        for marker in [
            "synthetic-opaque",
            "synthetic-token",
            "synthetic-user",
            "synthetic-password",
            "synthetic-client-token",
            "synthetic-client-opaque",
        ] {
            assert!(!logs.contains(marker), "HLS diagnostic contains synthetic secret marker: {label} {marker}");
        }
    } else {
        let full_reference = format!(
            "/hls/17/{}/index.m3u8?{}&{}",
            fixture::DIAGNOSTIC_AUTH_PATH,
            fixture::DIAGNOSTIC_TOKEN,
            fixture::DIAGNOSTIC_QUERY
        );
        assert!(logs.contains(&full_reference), "HLS diagnostic must preserve the full reference: {label}");
        if matches!(case, ProviderCase::Connection) {
            assert!(
                logs.lines()
                    .any(|line| line.contains("error sending request for url (") && line.contains(&full_reference)),
                "HLS diagnostic must preserve the transport error and its full reference: {label}"
            );
        }
    }
}

struct LiveView {
    urls: Vec<Url>,
    sequences: Vec<u64>,
    discontinuities: Vec<u64>,
    saved: Vec<bytes::Bytes>,
}

fn proxy_sequences(urls: &[Url]) -> Vec<u64> {
    urls.iter()
        .map(|url| url.path().rsplit('/').next().unwrap().strip_suffix(".ts").unwrap().parse().unwrap())
        .collect()
}

fn discontinuities(body: &str) -> Vec<u64> {
    let mut current = body
        .lines()
        .find_map(|line| line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:"))
        .map_or(0, |value| value.parse::<u64>().unwrap());
    let mut result = Vec::new();
    for line in body.lines() {
        if line == "#EXT-X-DISCONTINUITY" {
            current += 1;
        }
        if !line.is_empty() && !line.starts_with('#') {
            result.push(current);
        }
    }
    result
}

async fn live_window(
    player: &reqwest::Client,
    url: &Url,
    generation: u8,
    base: u64,
    last: u64,
    observed: &mut Vec<String>,
) -> Result<LiveView, TestkitError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(18);
    loop {
        let response = player.get(url.clone()).send().await?;
        let status = response.status();
        let retry_after = response.headers().contains_key(reqwest::header::RETRY_AFTER);
        let body = response.text().await?;
        observed.push(format!("generation={generation} base={base} expected_last={last} status={status}\n{body}"));
        if status == StatusCode::SERVICE_UNAVAILABLE {
            assert!(retry_after && body.is_empty(), "only the existing typed transient response may be retried");
        } else {
            assert_eq!(status, StatusCode::OK);
            assert!(!body.contains("#EXT-X-ENDLIST"), "CVR is not new live media: generation {generation}");
            let urls = resources(url, &body);
            assert!(!urls.is_empty(), "live media must contain resources");
            let selected = proxy_sequences(&urls);
            assert_eq!(sequence(&body), selected[0]);
            assert!(selected.windows(2).all(|pair| pair[1] == pair[0] + 1));
            if selected[0] >= base {
                assert!(*selected.last().unwrap() <= last, "unexpected publication highwater");
                let mut saved = Vec::new();
                for (uri, proxy) in urls.iter().zip(&selected) {
                    assert_eq!(uri.path().rsplit('/').next().unwrap(), format!("{proxy:06}.ts"));
                    let payload = player.get(uri.clone()).send().await?.error_for_status()?.bytes().await?;
                    assert_eq!(
                        payload.as_ref(),
                        bytes(generation, proxy - base),
                        "occurrence {generation}/{}",
                        proxy - base
                    );
                    saved.push(payload);
                }
                if selected.last() == Some(&last) {
                    let discontinuities = discontinuities(&body);
                    assert_eq!(discontinuities.len(), urls.len());
                    return Ok(LiveView { urls, sequences: selected, discontinuities, saved });
                }
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "generation {generation} must publish and progress to {last}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn observed_pause(
    fixture: &Fixture,
    player: &reqwest::Client,
    manifest: &Url,
    index: usize,
    observed: &mut Vec<String>,
) -> Result<(), TestkitError> {
    let phase = if index == 0 { 1 } else { 4 };
    fixture.origin.phase.store(phase, Ordering::Release);
    let request_player = player.clone();
    let request_url = manifest.clone();
    let origin = std::sync::Arc::clone(&fixture.origin);
    let request = tokio::spawn(async move {
        let mut statuses = Vec::new();
        while origin.phase.load(Ordering::Acquire) == phase {
            let response = request_player.get(request_url.clone()).send().await?;
            statuses.push(response.status().as_u16());
            let _ = response.bytes().await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok::<_, reqwest::Error>(statuses)
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let first = loop {
        if let Some(first) = fixture.origin.pause_observed(index) {
            break first;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "SUT must observe pause {} through an origin request",
            index + 1
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    tokio::time::sleep_until(first + Duration::from_secs(2)).await;
    let duration = first.elapsed();
    fixture.origin.phase.store(phase + 1, Ordering::Release);
    assert!((Duration::from_secs(2)..Duration::from_millis(2_500)).contains(&duration));
    let statuses =
        tokio::time::timeout(Duration::from_secs(20), request).await.expect("bounded pause client request").unwrap()?;
    observed.push(format!(
        "pause={} first_observed_407=true duration_ms={} triggering_client_statuses={statuses:?}",
        index + 1,
        duration.as_millis()
    ));
    Ok(())
}

async fn retained_media(player: &reqwest::Client, view: &LiveView) -> Result<(), TestkitError> {
    for (url, saved) in view.urls.iter().zip(&view.saved) {
        assert_eq!(
            &player.get(url.clone()).send().await?.error_for_status()?.bytes().await?,
            saved,
            "a previously published URI must retain its exact bytes: {url}"
        );
    }
    Ok(())
}

fn restart_commits(fixture: &Fixture) -> usize {
    std::fs::read_to_string(&fixture.log)
        .unwrap()
        .lines()
        .filter(|line| line.contains("decision=restart-committed"))
        .count()
}

async fn repeated_origin_reload(
    fixture: &Fixture,
    player: &reqwest::Client,
    url: &Url,
    generation: u8,
    base: u64,
    observed: &mut Vec<String>,
) -> Result<LiveView, TestkitError> {
    let phase = if generation == 1 { 3 } else { 6 };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut polls = 0;
    loop {
        let view = live_window(player, url, generation, base, base + 6, observed).await?;
        polls += 1;
        let reloads = fixture
            .origin
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with(&format!("phase={phase} ")) && request.ends_with("/index.m3u8"))
            .count();
        assert_eq!(restart_commits(fixture), usize::from(generation));
        if polls >= 3 && reloads >= 2 {
            observed.push(format!(
                "phase={phase} client_polls={polls} observed_origin_manifest_reloads={reloads} no_extra_restart=true"
            ));
            return Ok(view);
        }
        assert!(tokio::time::Instant::now() < deadline, "same-generation origin reload must be observed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn double_restart_run(mode: StartupMode) -> Result<(), TestkitError> {
    let fixture = Fixture::start_double_restart(mode).await?;
    let mut observed = Vec::new();
    let a_player = client("double-restart-a");
    let a = variant(&fixture, &a_player).await?;
    let a_view = live_window(&a_player, &a, 0, 0, 5, &mut observed).await?;
    let a_high = *a_view.sequences.last().unwrap();
    let a_disc = *a_view.discontinuities.last().unwrap();
    assert!(a_view.discontinuities.iter().all(|value| *value == a_disc));
    observed_pause(&fixture, &a_player, &a, 0, &mut observed).await?;
    let b_player = client("double-restart-b");
    let b = variant(&fixture, &b_player).await?;
    assert_ne!(a, b, "new public access lease");
    assert_eq!(a.path_segments().unwrap().nth(3), b.path_segments().unwrap().nth(3), "same proxy session");
    let b_base = a_high + 1;
    let b_view = live_window(&b_player, &b, 1, b_base, b_base + 5, &mut observed).await?;
    assert!(b_view.sequences.iter().all(|value| *value > a_high));
    assert!(b_view.discontinuities.iter().all(|value| *value == a_disc + 1), "exactly one restart discontinuity");
    assert_eq!(restart_commits(&fixture), 1);
    fixture.origin.phase.store(3, Ordering::Release);
    let b_progress = live_window(&b_player, &b, 1, b_base, b_base + 6, &mut observed).await?;
    assert!(b_progress.discontinuities.iter().all(|value| *value == a_disc + 1));
    let reload = repeated_origin_reload(&fixture, &b_player, &b, 1, b_base, &mut observed).await?;
    assert_eq!(reload.urls, b_progress.urls);
    assert_eq!(restart_commits(&fixture), 1, "reloads of B do not restart the stream");
    observed_pause(&fixture, &b_player, &b, 1, &mut observed).await?;
    let b_high = *b_progress.sequences.last().unwrap();
    let c_player = client("double-restart-c");
    let c = variant(&fixture, &c_player).await?;
    assert_ne!(b, c);
    assert_eq!(a.path_segments().unwrap().nth(3), c.path_segments().unwrap().nth(3));
    let c_base = b_high + 1;
    let c_view = live_window(&c_player, &c, 2, c_base, c_base + 5, &mut observed).await?;
    assert!(c_view.sequences.iter().all(|value| *value > b_high));
    assert!(c_view.discontinuities.iter().all(|value| *value == a_disc + 2), "exactly two restart discontinuities");
    assert_eq!(restart_commits(&fixture), 2);
    fixture.origin.phase.store(6, Ordering::Release);
    let c_progress = live_window(&c_player, &c, 2, c_base, c_base + 6, &mut observed).await?;
    assert!(c_progress.discontinuities.iter().all(|value| *value == a_disc + 2));
    let reload = repeated_origin_reload(&fixture, &c_player, &c, 2, c_base, &mut observed).await?;
    assert_eq!(reload.urls, c_progress.urls);
    retained_media(&a_player, &a_view).await?;
    retained_media(&b_player, &b_view).await?;
    retained_media(&b_player, &b_progress).await?;
    let logs = std::fs::read_to_string(&fixture.log)?;
    assert_eq!(restart_commits(&fixture), 2, "reloads of C do not restart the stream");
    assert_eq!(logs.lines().filter(|line| line.contains("HLS session created:")).count(), 1, "same session lifetime");
    for epoch in [1, 2] {
        assert!(logs.lines().any(|line| line.contains(&format!("decision=restart-committed origin_epoch={epoch} "))));
    }
    let requests = fixture.origin.requests.lock().unwrap().clone();
    assert!(requests.len() < 250, "bounded real origin work");
    for phase in [0, 1, 2, 3, 4, 5, 6] {
        assert!(
            requests
                .iter()
                .any(|request| request.starts_with(&format!("phase={phase} ")) && request.ends_with("/index.m3u8")),
            "observed manifest phase {phase}"
        );
    }
    for namespace in ["a", "b", "c"] {
        assert!(
            requests.iter().any(|request| request.contains(&format!("/hls/17/{namespace}/"))),
            "fetched generation {namespace}"
        );
    }
    observed.push(format!("mode={mode} a={:?} b={:?} b_progress={:?} c={:?} c_progress={:?} origin_requests={} restart_commits=2 session_creations=1 retained_a_b_exact_bytes=true", a_view.sequences, b_view.sequences, b_progress.sequences, c_view.sequences, c_progress.sequences, requests.len()));
    println!("case=double-restart mode={mode} a={:?} b={:?} b_progress={:?} c={:?} c_progress={:?} origin_requests={} restart_commits=2 session_creations=1 exact_bytes_and_discontinuities=true retained_a_b=true", a_view.sequences, b_view.sequences, b_progress.sequences, c_view.sequences, c_progress.sequences, requests.len());
    fixture.stop_with_observations(&format!("double-restart-{mode}"), &observed).await
}

#[tokio::test]
#[ignore = "requires built proxy selected by TULIPROX_TESTKIT_SUT_BINARY"]
async fn two_origin_restarts_keep_one_session_in_all_modes() -> Result<(), TestkitError> {
    for mode in [StartupMode::Conservative, StartupMode::FirstReady, StartupMode::Progressive, StartupMode::Optimistic]
    {
        double_restart_run(mode).await?;
    }
    Ok(())
}

fn save_rollover_media(label: &str, bytes: &[u8]) -> Result<(), TestkitError> {
    if let Some(directory) = std::env::var_os("TULIPROX_TESTKIT_ARTIFACT_DIR") {
        let directory = std::path::PathBuf::from(directory).join("redirect-rollover-media");
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join(format!("{label}.ts")), bytes)?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires built proxy selected by TULIPROX_TESTKIT_SUT_BINARY"]
async fn healthy_redirect_host_rollover_proves_overlap_and_keeps_old_client_bytes() -> Result<(), TestkitError> {
    let fixture = Fixture::start_redirect_rollover().await?;
    let player = client("redirect-rollover-client");
    let media = variant(&fixture, &player).await?;
    assert!(media.path().starts_with("/hls/shared/live/"), "public shared HLS client route");
    let original = player.get(media.clone()).send().await?.error_for_status()?.text().await?;
    assert!(!original.contains("#EXT-X-ENDLIST"));
    let old_sequence = sequence(&original);
    let old_resources = resources(&media, &original);
    assert_eq!(old_sequence + u64::try_from(old_resources.len()).unwrap(), 6);
    let mut saved = Vec::new();
    for (index, url) in old_resources.iter().enumerate() {
        let body = player.get(url.clone()).send().await?.error_for_status()?.bytes().await?;
        assert_eq!(body.as_ref(), bytes(0, 432 + old_sequence + u64::try_from(index).unwrap()));
        save_rollover_media(&format!("warm-origin-{}", 432 + old_sequence + u64::try_from(index).unwrap()), &body)?;
        saved.push(body);
    }
    fixture.origin.phase.store(1, Ordering::Release);
    let mut observations = vec![format!("phase=warm first_proxy={old_sequence} last_proxy=5 origin_tail=437")];
    let mut last_tail = 5;
    for (phase, target_tail) in [(1, 7), (2, 9)] {
        fixture.origin.phase.store(phase, Ordering::Release);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let body = player.get(media.clone()).send().await?.error_for_status()?.text().await?;
            assert!(!body.contains("#EXT-X-ENDLIST"), "healthy local reserve must remain live");
            let first = sequence(&body);
            let urls = resources(&media, &body);
            let tail = first + u64::try_from(urls.len()).unwrap() - 1;
            assert!(tail >= last_tail, "proxy sequence must stay monotone");
            observations.push(format!("phase={phase} first_proxy={first} last_proxy={tail} live=true"));
            if tail >= target_tail {
                for (index, url) in urls.iter().enumerate() {
                    let proxy_seq = first + u64::try_from(index).unwrap();
                    let delivered = player.get(url.clone()).send().await?.error_for_status()?.bytes().await?;
                    assert_eq!(delivered.as_ref(), bytes(0, 432 + proxy_seq));
                    save_rollover_media(&format!("phase-{phase}-origin-{}", 432 + proxy_seq), &delivered)?;
                    observations.push(format!(
                        "phase={phase} proxy_seq={proxy_seq} origin_seq={} raw_bytes_equal=true length={}",
                        432 + proxy_seq,
                        delivered.len()
                    ));
                }
                last_tail = tail;
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "host rollover must produce new verified media");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    for (index, (url, expected)) in old_resources.iter().zip(&saved).enumerate() {
        let retained = player.get(url.clone()).send().await?.error_for_status()?.bytes().await?;
        assert_eq!(&retained, expected);
        save_rollover_media(
            &format!("retained-origin-{}", 432 + old_sequence + u64::try_from(index).unwrap()),
            &retained,
        )?;
    }
    let protocol = fixture.origin.protocol();
    let alternate_media: Vec<_> = protocol
        .iter()
        .filter(|event| {
            event["host"].as_str().is_some_and(|host| host.starts_with("localhost:"))
                && event["uri"].as_str().is_some_and(|uri| uri.starts_with("/media/"))
        })
        .collect();
    assert_eq!(
        alternate_media.first().unwrap()["uri"],
        "/media/437.ts?synthetic=1",
        "the candidate's matching occurrence must be probed first"
    );
    assert!(alternate_media.iter().any(|event| event["uri"] == "/media/438.ts?synthetic=1"));
    assert!(protocol.iter().any(|event| event["host"].as_str().is_some_and(|host| host.starts_with("127.0.0.1:"))
        && event["uri"] == "/rollover/index.m3u8"));
    assert!(!fixture.log_contains("outcome=terminal_committed"));
    assert!(!fixture.log_contains("HLS autonomous terminal owner completed: outcome=committed"));
    let requests = fixture.origin.requests.lock().unwrap().len();
    assert!(requests < 100, "bounded process fixture work: {requests}");
    println!("mode=conservative pinned_host=127.0.0.1 candidate_host=localhost anchor_origin=437 first_continuation=438 final_origin_tail=441 old_uris_equal=true live=true requests={requests}");
    fixture.stop_with_observations("redirect-rollover", &observations).await
}
