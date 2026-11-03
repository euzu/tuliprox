use super::*;

#[test]
fn clean_preserves_disk_alert_with_default_values() {
    let mut messaging = MessagingConfigDto { disk_alert: Some(DiskAlertConfigDto::default()), ..Default::default() };
    messaging.clean();
    assert_eq!(messaging.disk_alert, None);
}

#[test]
fn clean_preserves_disk_alert_with_non_default_values() {
    let custom = DiskAlertConfigDto { warn_percent: 70.0, critical_percent: 90.0, repeat_interval_secs: 600 };
    let mut messaging = MessagingConfigDto { disk_alert: Some(custom.clone()), ..Default::default() };
    messaging.clean();
    assert_eq!(messaging.disk_alert, Some(custom));
}

#[test]
fn prepare_rewrites_legacy_notify_on_names_to_canonical_ids() {
    let mut messaging = MessagingConfigDto {
        notify_on: vec!["disk_alert".to_string(), "recording_completed".to_string()],
        ..Default::default()
    };
    messaging.prepare(false).expect("prepare");
    assert_eq!(messaging.notify_on, vec!["system.disk.alert", "recording.completed"]);
}

#[test]
fn prepare_leaves_glob_patterns_untouched() {
    let mut messaging = MessagingConfigDto {
        notify_on: vec!["recording.*".to_string(), "!system.info".to_string(), "*".to_string()],
        ..Default::default()
    };
    messaging.prepare(false).expect("prepare");
    assert_eq!(messaging.notify_on, vec!["recording.*", "!system.info", "*"]);
}

#[test]
fn is_empty_default_messaging() {
    assert!(MessagingConfigDto::default().is_empty());
}

#[test]
fn is_empty_with_only_notify_on_is_not_empty() {
    let messaging = MessagingConfigDto { notify_on: vec!["disk_alert".to_string()], ..Default::default() };
    assert!(!messaging.is_empty());
}
