use super::*;

#[test]
fn uncommitted_shared_meter_registration_is_rolled_back() {
    let app_cfg = create_test_app_config();
    let events = Arc::new(EventManager::new());
    let providers = Arc::new(ActiveProviderManager::new(&app_cfg, &events));
    let manager = Arc::new(SharedStreamManager::new(providers));
    let url = "https://example.invalid/live/meter-rollback.ts";

    let (meter_uid, pending) = manager.reserve_meter_uid(url, || 41);
    assert_eq!(meter_uid, 41);
    assert_eq!(manager.meter_count(), 1);

    drop(pending);
    assert_eq!(manager.meter_count(), 0);
}

#[test]
fn successor_adoption_overwrites_stale_owner_and_survives_old_teardown() {
    let app_cfg = create_test_app_config();
    let events = Arc::new(EventManager::new());
    let providers = Arc::new(ActiveProviderManager::new(&app_cfg, &events));
    let manager = Arc::new(SharedStreamManager::new(providers));
    let url = "https://example.invalid/live/meter-successor.ts";

    let (uid, guard) = manager.reserve_meter_uid(url, || 71);
    manager.adopt_meter_uid(url, 100);
    // A successor origin commits the same URL and takes over the meter.
    manager.adopt_meter_uid(url, 200);
    assert_eq!(manager.lock_meter_uids().get(url).and_then(|entry| entry.owner), Some(200));

    // A stale teardown by the predecessor must not remove the successor's entry.
    manager.remove_meter_uid_if_owned_by(url, Some(100));
    assert_eq!(manager.meter_count(), 1);
    assert_eq!(manager.lock_meter_uids().get(url).map(|entry| entry.uid), Some(uid));
    guard.expect("reservation").commit();
}
