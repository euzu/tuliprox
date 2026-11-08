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
fn clean_strips_empty_subconfigs() {
    let mut messaging = MessagingConfigDto {
        telegram: Some(TelegramMessagingConfigDto::default()),
        rest: Some(RestMessagingConfigDto::default()),
        pushover: Some(PushoverMessagingConfigDto::default()),
        discord: Some(DiscordMessagingConfigDto::default()),
        ..Default::default()
    };
    messaging.clean();
    assert!(messaging.telegram.is_none());
    assert!(messaging.rest.is_none());
    assert!(messaging.pushover.is_none());
    assert!(messaging.discord.is_none());
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
fn redaction_masks_every_channel_secret() {
    let mut messaging = MessagingConfigDto {
        telegram: Some(TelegramMessagingConfigDto { bot_token: "real-token".to_string(), ..Default::default() }),
        pushover: Some(PushoverMessagingConfigDto {
            token: "real-app".to_string(),
            user: "real-user".to_string(),
            ..Default::default()
        }),
        rest: Some(RestMessagingConfigDto {
            url: "https://example.test".to_string(),
            headers: vec!["Authorization: Bearer secret".to_string(), "X-Trace: keep-me".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    };
    messaging.redact_secrets();
    assert_eq!(messaging.telegram.as_ref().expect("telegram").bot_token, REDACTED_SECRET);
    assert_eq!(messaging.pushover.as_ref().expect("pushover").token, REDACTED_SECRET);
    assert_eq!(messaging.pushover.as_ref().expect("pushover").user, REDACTED_SECRET);
    let headers = &messaging.rest.as_ref().expect("rest").headers;
    assert!(headers.contains(&format!("Authorization: {REDACTED_SECRET}")), "auth header not masked: {headers:?}");
    // A non-credential header keeps its value; masking everything would
    // make the config unreadable for no gain.
    assert!(headers.contains(&"X-Trace: keep-me".to_string()), "harmless header was masked: {headers:?}");
}

#[test]
fn an_empty_secret_is_not_replaced_by_a_mask() {
    // Otherwise an unconfigured channel would look configured.
    let mut messaging =
        MessagingConfigDto { telegram: Some(TelegramMessagingConfigDto::default()), ..Default::default() };
    messaging.redact_secrets();
    assert_eq!(messaging.telegram.as_ref().expect("telegram").bot_token, "");
}

#[test]
fn a_returned_mask_restores_the_stored_secret() {
    // Without this the UI round-trip would overwrite the real token with
    // the mask and silently break the channel.
    let stored = MessagingConfigDto {
        telegram: Some(TelegramMessagingConfigDto { bot_token: "real-token".to_string(), ..Default::default() }),
        ..Default::default()
    };
    let mut incoming = MessagingConfigDto {
        telegram: Some(TelegramMessagingConfigDto { bot_token: REDACTED_SECRET.to_string(), ..Default::default() }),
        ..Default::default()
    };
    incoming.restore_redacted_secrets(&stored);
    assert_eq!(incoming.telegram.as_ref().expect("telegram").bot_token, "real-token");
}

#[test]
fn a_genuinely_changed_secret_is_written_through() {
    let stored = MessagingConfigDto {
        telegram: Some(TelegramMessagingConfigDto { bot_token: "old".to_string(), ..Default::default() }),
        ..Default::default()
    };
    let mut incoming = MessagingConfigDto {
        telegram: Some(TelegramMessagingConfigDto { bot_token: "new".to_string(), ..Default::default() }),
        ..Default::default()
    };
    incoming.restore_redacted_secrets(&stored);
    assert_eq!(incoming.telegram.as_ref().expect("telegram").bot_token, "new");
}

#[test]
fn a_returned_masked_header_restores_the_stored_one() {
    let stored = MessagingConfigDto {
        rest: Some(RestMessagingConfigDto {
            url: "https://example.test".to_string(),
            headers: vec!["Authorization: Bearer secret".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut incoming = MessagingConfigDto {
        rest: Some(RestMessagingConfigDto {
            url: "https://example.test".to_string(),
            headers: vec![format!("Authorization: {REDACTED_SECRET}")],
            ..Default::default()
        }),
        ..Default::default()
    };
    incoming.restore_redacted_secrets(&stored);
    assert_eq!(incoming.rest.as_ref().expect("rest").headers, vec!["Authorization: Bearer secret".to_string()]);
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
