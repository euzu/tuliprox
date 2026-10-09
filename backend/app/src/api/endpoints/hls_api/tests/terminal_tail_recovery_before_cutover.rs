use super::terminal_tail::{
    assert_recovery_after_outage, assert_stale_origin_continuation, assert_stale_origin_handoff, observe_stale_origin,
    recovery_before_cutover_fixture, run_recovery_outage, stale_origin_fixture,
};

#[tokio::test]
async fn recovers_before_lease_cutover_without_terminal_tail() {
    let mut fixture = recovery_before_cutover_fixture().await;
    let (progress_generation, last_episode_generation) = run_recovery_outage(&mut fixture).await;
    assert_recovery_after_outage(&mut fixture, progress_generation, last_episode_generation).await;
}

#[tokio::test]
async fn reachable_stale_origin_hands_off_to_progressed_origin_without_terminal_tail() {
    let mut fixture = stale_origin_fixture().await;
    let directive = observe_stale_origin(&mut fixture).await;
    let progress_generation = assert_stale_origin_handoff(&mut fixture, directive).await;
    assert_stale_origin_continuation(&mut fixture, progress_generation).await;
}
