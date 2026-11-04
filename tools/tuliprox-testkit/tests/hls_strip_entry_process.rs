#[path = "support/strip_entry_fixture.rs"]
mod fixture;

use fixture::{bytes, client, Fixture};
use reqwest::{StatusCode, Url};
use std::{sync::atomic::Ordering, time::Duration};
use tuliprox_testkit::{hls_probe::StartupMode, TestkitError};

fn resources(base: &Url, body: &str) -> Vec<Url> {
    body.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| base.join(line).unwrap())
        .collect()
}
fn sequences(urls: &[Url]) -> Vec<u64> {
    urls.iter().map(|url| url.path().rsplit('/').next().unwrap().split('.').next().unwrap().parse().unwrap()).collect()
}
async fn variant(fixture: &Fixture, player: &reqwest::Client) -> Result<Url, TestkitError> {
    let response = player.get(&fixture.entry).send().await?.error_for_status()?;
    let base = response.url().clone();
    let body = response.text().await?;
    assert!(body.contains("#EXT-X-STREAM-INF"));
    Ok(resources(&base, &body).remove(0))
}
async fn manifest(
    player: &reqwest::Client,
    url: &Url,
    expected: &[u64],
    observations: &mut Vec<String>,
) -> Result<Vec<Url>, TestkitError> {
    let response = player.get(url.clone()).send().await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await?;
    assert!(!body.contains("#EXT-X-ENDLIST"));
    let urls = resources(url, &body);
    assert_eq!(sequences(&urls), expected);
    let media_sequence = body.lines().find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")).unwrap();
    assert_eq!(media_sequence.parse::<u64>().unwrap(), expected[0]);
    for (uri, seq) in urls.iter().zip(expected) {
        assert_eq!(uri.path().rsplit('/').next().unwrap(), format!("{seq:06}.ts"));
    }
    observations.push(body);
    Ok(urls)
}
async fn media(player: &reqwest::Client, urls: &[Url]) -> Result<(), TestkitError> {
    for (url, sequence) in urls.iter().zip(sequences(urls)) {
        assert_eq!(player.get(url.clone()).send().await?.error_for_status()?.bytes().await?.as_ref(), bytes(sequence));
    }
    Ok(())
}

async fn warm_active(
    player: &reqwest::Client,
    url: &Url,
    observations: &mut Vec<String>,
) -> Result<Vec<Url>, TestkitError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let body = player.get(url.clone()).send().await?.error_for_status()?.text().await?;
        let urls = resources(url, &body);
        let selected = sequences(&urls);
        assert!(!body.contains("#EXT-X-ENDLIST"));
        assert!([0, 1, 2, 3, 4, 5].starts_with(&selected));
        observations.push(body);
        media(player, &urls).await?;
        if selected == [0, 1, 2, 3, 4, 5] {
            return Ok(urls);
        }
        assert!(tokio::time::Instant::now() < deadline, "active window must finish warming");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
fn entry_log(fixture: &Fixture, generation: u64, first: u64, last: u64, configured: u64) -> String {
    entry_log_window(fixture, generation, first, last, configured, 3)
}

fn entry_log_window(
    fixture: &Fixture,
    generation: u64,
    first: u64,
    last: u64,
    configured: u64,
    count: usize,
) -> String {
    let logs = fixture.logs();
    let line = logs.lines().rev().find(|line| line.contains("HLS initial strip applied:")).unwrap();
    for field in [
        format!("entry_generation={generation}"),
        format!("first={first}"),
        format!("last={last}"),
        format!("configured={configured}"),
        format!("visible_segments={count}"),
        "strip_mode=segments".into(),
        "exception=none".into(),
    ] {
        assert!(line.contains(&field), "committed strip diagnostic: {field}: {line}");
    }
    line.split_whitespace().find_map(|field| field.strip_prefix("lease=")).unwrap().to_owned()
}
async fn idle(
    fixture: &Fixture,
    player_a: &reqwest::Client,
    a: &Url,
    lease: &str,
    observations: &mut Vec<String>,
) -> Result<(), TestkitError> {
    let start = fixture.logs().len();
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(8);
    loop {
        // Ordinary origin progress keeps AP1 freshness valid during the real
        // Idle wait. No lease clock, source epoch or publication highwater is reset.
        if started.elapsed() >= Duration::from_millis(1500) {
            fixture.origin.head.store(1, Ordering::Release);
        }
        let body = player_a.get(a.clone()).send().await?.error_for_status()?.text().await?;
        let urls = resources(a, &body);
        let selected = sequences(&urls);
        assert!(selected == [0, 1, 2, 3, 4, 5] || selected == [1, 2, 3, 4, 5, 6]);
        assert!(!body.contains("#EXT-X-ENDLIST"));
        observations.push(body);
        media(player_a, &urls[5..]).await?;
        if fixture.logs()[start..]
            .lines()
            .any(|line| line.contains("HLS access lease idled:") && line.contains(&format!("lease={lease}")))
        {
            assert_eq!(selected, [1, 2, 3, 4, 5, 6]);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "B must enter Idle through the actual lifecycle");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Idle,
    RegressiveSuccess,
    RegressiveTimeout,
}

#[expect(clippy::too_many_lines, reason = "Keep each ordered client lifecycle and its exact byte assertions together.")]
async fn run(mode: StartupMode, scenario: Scenario) -> Result<(), TestkitError> {
    let negative = matches!(scenario, Scenario::RegressiveTimeout);
    let fixture = Fixture::start(mode).await?;
    let mut observations = Vec::new();
    let player_a = client("strip-viewer-a");
    let a = variant(&fixture, &player_a).await?;
    let a_entry = manifest(&player_a, &a, &[0, 1, 2], &mut observations).await?;
    media(&player_a, &a_entry).await?;
    let a_active = warm_active(&player_a, &a, &mut observations).await?;
    fixture.reload(3, 1).await?;
    assert_eq!(manifest(&player_a, &a, &[0, 1, 2, 3, 4, 5], &mut observations).await?, a_active);
    let warm_downloads = fixture.origin.media_requests.load(Ordering::Acquire);
    let player_b = client("strip-viewer-b");
    let b = variant(&fixture, &player_b).await?;
    assert_ne!(a, b, "independent access leases share the session");
    assert_eq!(a.path_segments().unwrap().nth(3), b.path_segments().unwrap().nth(3), "same shared proxy session");
    let b_entry = manifest(&player_b, &b, &[2, 3, 4], &mut observations).await?;
    let b_log_id = entry_log(&fixture, 1, 2, 4, 1);
    media(&player_b, &b_entry).await?;
    assert_eq!(
        fixture.origin.media_requests.load(Ordering::Acquire),
        warm_downloads,
        "a warm entry view and its bound media need no origin download"
    );
    assert_eq!(manifest(&player_a, &a, &[0, 1, 2, 3, 4, 5], &mut observations).await?, a_active);
    media(&player_a, &a_active).await?;
    let b_active = manifest(&player_b, &b, &[2, 3, 4, 5], &mut observations).await?;
    if matches!(scenario, Scenario::Idle) {
        idle(&fixture, &player_a, &a, &b_log_id, &mut observations).await?;
        media(&player_b, &b_active[3..]).await?;
        let reentry = manifest(&player_b, &b, &[3, 4, 5], &mut observations).await?;
        assert!(
            reentry.iter().all(|url| url.path_segments().unwrap().nth(4) == b.path_segments().unwrap().nth(4)),
            "same lease after Idle"
        );
        assert_eq!(entry_log(&fixture, 2, 3, 5, 1), b_log_id);
        media(&player_b, &reentry).await?;
        manifest(&player_b, &b, &[3, 4, 5, 6], &mut observations).await?;
        let current_a = manifest(&player_a, &a, &[1, 2, 3, 4, 5, 6], &mut observations).await?;
        media(&player_a, &current_a).await?;
        media(&player_a, &a_active).await?;
        observations
            .push("entry_generations=1,2 same_lease=true old_media_did_not_activate=true bytes_verified=true".into());
        println!(
            "mode={mode} scenario=Idle exact_uris_and_bytes_verified=true warm_origin_media_requests={warm_downloads}"
        );
        return fixture.stop(&format!("{mode}-idle"), &observations).await;
    }
    fixture.reload(1, 3).await?;
    idle(&fixture, &player_a, &a, &b_log_id, &mut observations).await?;
    let a_current = manifest(&player_a, &a, &[1, 2, 3, 4, 5, 6], &mut observations).await?;
    let log_start = fixture.logs().len();
    let request_player = player_b.clone();
    let request_url = b.clone();
    let started = tokio::time::Instant::now();
    let task = tokio::spawn(async move { request_player.get(request_url).send().await });
    let deadline = started + Duration::from_secs(2);
    while !fixture.logs()[log_start..].contains("regressive_entry_window") {
        assert!(!task.is_finished(), "a regressive entry must not return its old body");
        assert!(tokio::time::Instant::now() < deadline, "entry must reach the policy wait");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(manifest(&player_a, &a, &[1, 2, 3, 4, 5, 6], &mut observations).await?, a_current);
    if !negative {
        fixture.origin.head.store(3, Ordering::Release);
    }
    let mut response =
        tokio::time::timeout(Duration::from_secs(10), task).await.expect("bounded existing request budget").unwrap()?;
    let elapsed = started.elapsed();
    if negative {
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "existing typed error for a permanently regressive entry"
        );
        assert!(response.headers().contains_key(reqwest::header::RETRY_AFTER));
        assert_eq!(response.bytes().await?.len(), 0, "no retained body on a rejected entry");
        assert!(elapsed < Duration::from_secs(9));
        assert!(!fixture.logs()[log_start..]
            .lines()
            .any(|line| line.contains("HLS initial strip applied:") && line.contains(&format!("lease={b_log_id}"))));
        let retry = player_b.get(b.clone()).send().await?;
        assert_eq!(retry.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(manifest(&player_a, &a, &[1, 2, 3, 4, 5, 6], &mut observations).await?, a_current);
        media(&player_a, &a_current).await?;
        observations.push(format!(
            "negative status=503 elapsed_ms={} retry_status=503 active_a_unchanged=true",
            elapsed.as_millis()
        ));
    } else {
        // The public cache probe can return its existing Retry-After response
        // immediately. A failed probe must not consume this entry generation.
        let deadline = started + Duration::from_secs(8);
        while response.status() == StatusCode::SERVICE_UNAVAILABLE {
            assert!(response.headers().contains_key(reqwest::header::RETRY_AFTER));
            assert!(response.bytes().await?.is_empty(), "no retained live body on a rejected entry");
            assert!(tokio::time::Instant::now() < deadline, "matching origin window must admit the same entry");
            tokio::time::sleep(Duration::from_millis(100)).await;
            response = player_b.get(b.clone()).send().await?;
        }
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await?;
        let urls = resources(&b, &body);
        assert_eq!(sequences(&urls), [3, 4, 5]);
        observations.push(body);
        assert_eq!(entry_log(&fixture, 2, 3, 5, 3), b_log_id);
        media(&player_b, &urls).await?;
        observations.push(format!("positive completion_elapsed_ms={} initial_probe_elapsed_ms={} warm_origin_media_requests={warm_downloads} bytes_verified=true entry_generations=1,2", started.elapsed().as_millis(), elapsed.as_millis()));
    }
    println!("mode={mode} negative={negative} warm_origin_media_requests={warm_downloads} exact_uris_and_bytes_verified=true completion_elapsed_ms={} initial_probe_elapsed_ms={}", started.elapsed().as_millis(), elapsed.as_millis());
    fixture.stop(&format!("{mode}-{negative}"), &observations).await
}

#[tokio::test]
#[ignore = "requires built proxy selected by TULIPROX_TESTKIT_SUT_BINARY"]
async fn client_pending_and_idle_entries_follow_the_same_strip_contract() -> Result<(), TestkitError> {
    for mode in [StartupMode::FirstReady, StartupMode::Progressive, StartupMode::Optimistic] {
        run(mode, Scenario::Idle).await?;
        run(mode, Scenario::RegressiveSuccess).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires built proxy selected by TULIPROX_TESTKIT_SUT_BINARY"]
async fn regressive_entry_deadline_is_bounded_without_affecting_other_leases() -> Result<(), TestkitError> {
    for mode in [StartupMode::FirstReady, StartupMode::Progressive, StartupMode::Optimistic] {
        run(mode, Scenario::RegressiveTimeout).await?;
    }
    Ok(())
}

async fn warm_window(
    player: &reqwest::Client,
    url: &Url,
    expected: &[u64],
    observations: &mut Vec<String>,
) -> Result<Vec<Url>, TestkitError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let response = player.get(url.clone()).send().await?;
        if response.status() == StatusCode::NOT_FOUND && observations.is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "cold manifest must become ready");
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let body = response.error_for_status()?.text().await?;
        assert!(!body.contains("#EXT-X-ENDLIST"), "warmup requires live media");
        let urls = resources(url, &body);
        let actual = sequences(&urls);
        assert!(expected.starts_with(&actual), "bounded warmup prefix: {actual:?}, expected {expected:?}");
        observations.push(body);
        media(player, &urls).await?;
        if actual == expected {
            return Ok(urls);
        }
        assert!(tokio::time::Instant::now() < deadline, "warmup did not reach {expected:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn strip_two_run(mode: StartupMode) -> Result<(), TestkitError> {
    let fixture = Fixture::start_strip_two(mode).await?;
    let mut observed = Vec::new();
    let a_player = client("strip-two-a");
    let a = variant(&fixture, &a_player).await?;
    warm_window(&a_player, &a, &[0, 1, 2, 3, 4], &mut observed).await?;
    fixture.origin.window_count.store(6, Ordering::Release);
    let a_warm = warm_window(&a_player, &a, &[0, 1, 2, 3, 4, 5], &mut observed).await?;
    let warm_count = fixture.origin.media_requests.load(Ordering::Acquire);
    assert_eq!(warm_count, 6);
    let b_player = client("strip-two-b");
    let b = variant(&fixture, &b_player).await?;
    assert_ne!(a, b);
    assert_eq!(a.path_segments().unwrap().nth(3), b.path_segments().unwrap().nth(3));
    let (pending, active, reentry): (&[u64], &[u64], &[u64]) = if matches!(mode, StartupMode::Conservative) {
        (&[0, 1, 2, 3], &[0, 1, 2, 3, 4, 5], &[1, 2, 3, 4])
    } else {
        (&[1, 2, 3], &[1, 2, 3, 4, 5], &[2, 3, 4])
    };
    let mut b_pending = Vec::new();
    for _ in 0..3 {
        let current = manifest(&b_player, &b, pending, &mut observed).await?;
        if b_pending.is_empty() {
            b_pending = current;
        } else {
            assert_eq!(current, b_pending);
        }
    }
    let safe_b = entry_log_window(&fixture, 1, pending[0], *pending.last().unwrap(), 2, pending.len());
    assert_eq!(manifest(&a_player, &a, &[0, 1, 2, 3, 4, 5], &mut observed).await?, a_warm);
    media(&a_player, &a_warm).await?;
    media(&b_player, &b_pending).await?;
    assert_eq!(fixture.origin.media_requests.load(Ordering::Acquire), warm_count);
    let b_active = manifest(&b_player, &b, active, &mut observed).await?;
    assert_eq!(manifest(&a_player, &a, &[0, 1, 2, 3, 4, 5], &mut observed).await?, a_warm);
    media(&a_player, &a_warm).await?;
    idle(&fixture, &a_player, &a, &safe_b, &mut observed).await?;
    let a_slid = manifest(&a_player, &a, &[1, 2, 3, 4, 5, 6], &mut observed).await?;
    media(&a_player, &a_slid).await?;
    let after_progress = fixture.origin.media_requests.load(Ordering::Acquire);
    assert_eq!(after_progress, warm_count + 1, "only origin occurrence 6 is new");
    media(&b_player, &b_active).await?;
    let mut b_idle = Vec::new();
    for _ in 0..3 {
        let current = manifest(&b_player, &b, reentry, &mut observed).await?;
        if b_idle.is_empty() {
            b_idle = current;
        } else {
            assert_eq!(current, b_idle);
        }
    }
    assert_eq!(entry_log_window(&fixture, 2, reentry[0], *reentry.last().unwrap(), 2, reentry.len()), safe_b);
    assert!(b_idle.iter().all(|uri| uri.path_segments().unwrap().nth(4) == b.path_segments().unwrap().nth(4)));
    assert_eq!(manifest(&a_player, &a, &[1, 2, 3, 4, 5, 6], &mut observed).await?, a_slid);
    media(&a_player, &a_slid).await?;
    media(&b_player, &b_idle).await?;
    let reactivated: Vec<_> = (*reentry.first().unwrap()..=6).collect();
    manifest(&b_player, &b, &reactivated, &mut observed).await?;
    assert_eq!(fixture.origin.media_requests.load(Ordering::Acquire), after_progress);
    observed.push(format!("mode={mode} pending={pending:?} active={active:?} idle={reentry:?} warm_origin_media={warm_count} after_progress_origin_media={after_progress} polls_per_entry=3 same_session=true same_lease=true exact_bytes=true"));
    println!("case=strip-two mode={mode} pending={pending:?} active={active:?} idle={reentry:?} warm_origin_media={warm_count} progress_origin_media={} exact_proxy_uris_media_sequences_and_bytes=true", after_progress - warm_count);
    fixture.stop(&format!("strip-two-{mode}"), &observed).await
}

#[tokio::test]
#[ignore = "requires built proxy selected by TULIPROX_TESTKIT_SUT_BINARY"]
async fn strip_two_warm_session_and_real_idle_in_all_modes() -> Result<(), TestkitError> {
    for mode in [StartupMode::Conservative, StartupMode::FirstReady, StartupMode::Progressive, StartupMode::Optimistic]
    {
        strip_two_run(mode).await?;
    }
    Ok(())
}
