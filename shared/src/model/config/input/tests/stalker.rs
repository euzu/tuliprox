use super::*;

#[test]
fn stalker_options_reject_retired_playback_resolution_switches() -> Result<(), serde_json::Error> {
    let dto: ConfigInputOptionsDto = serde_json::from_str(r#"{"stalker_bulk_epg":true}"#)?;
    assert!(dto.stalker_bulk_epg);
    assert!(serde_json::from_str::<ConfigInputOptionsDto>(r#"{"stalker_pre_resolve_playback":true}"#).is_err());
    assert!(serde_json::from_str::<ConfigInputOptionsDto>(r#"{"stalker_runtime_resolve_playback":false}"#).is_err());
    Ok(())
}

#[test]
fn new_stalker_input_has_configuration_block() {
    let input = ConfigInputDto::new_with_type(InputType::Stalker);

    assert!(input.stalker.is_some());
    assert!(ConfigInputDto::new_with_type(InputType::M3u).stalker.is_none());
}

#[test]
fn stalker_mac_is_normalized_and_multicast_is_rejected() {
    let mut input = ConfigInputDto::new_with_type(InputType::Stalker);
    input.name = "portal".into();
    input.stalker.as_mut().expect("stalker config").device =
        Some(StalkerDeviceProfileDto { mac_address: Some("00:1A:79:AA:BB:CC".to_string()), ..Default::default() });
    input.prepare_stalker_input().expect("unicast MAC should be valid");
    assert_eq!(
        input
            .stalker
            .as_ref()
            .and_then(|config| config.device.as_ref())
            .and_then(|device| device.mac_address.as_deref()),
        Some("00:1a:79:aa:bb:cc")
    );

    input.stalker.as_mut().expect("stalker config").device.as_mut().expect("device").mac_address =
        Some("01:00:00:00:00:00".to_string());
    assert!(input.prepare_stalker_input().is_err());
}

#[test]
fn stalker_main_input_requires_identity_but_batch_aliases_may_inherit() {
    let mut input = ConfigInputDto::new_with_type(InputType::Stalker);
    input.name = "portal".into();
    assert!(input.prepare_stalker_input().is_err());

    let mut alias =
        ConfigInputAliasDto { name: "alias".into(), url: "http://portal.example/c/".to_string(), ..Default::default() };
    assert!(alias.prepare(0, &InputType::Stalker).is_ok());
}

#[test]
fn stalker_auth_mode_requires_matching_identity() {
    for (auth_mode, has_mac, has_credentials, valid) in [
        (StalkerAuthMode::Auto, true, false, true),
        (StalkerAuthMode::Auto, false, true, true),
        (StalkerAuthMode::MacOnly, true, false, true),
        (StalkerAuthMode::MacOnly, false, true, false),
        (StalkerAuthMode::CredentialsOnly, false, true, true),
        (StalkerAuthMode::CredentialsOnly, true, false, false),
        (StalkerAuthMode::MacPlusCredentials, true, true, true),
        (StalkerAuthMode::MacPlusCredentials, true, false, false),
    ] {
        let mut input = ConfigInputDto::new_with_type(InputType::Stalker);
        input.name = "portal".into();
        input.username = has_credentials.then(|| "user".to_string());
        input.password = has_credentials.then(|| "password".to_string());
        input.stalker = Some(StalkerInputConfigDto {
            auth_mode,
            device: has_mac.then(|| StalkerDeviceProfileDto {
                mac_address: Some("00:1a:79:aa:bb:cc".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });

        assert_eq!(input.prepare_stalker_input().is_ok(), valid, "auth mode: {auth_mode}");
    }
}

#[test]
fn non_stalker_input_rejects_stalker_configuration() {
    let mut input =
        ConfigInputDto { name: "m3u".into(), stalker: Some(StalkerInputConfigDto::default()), ..Default::default() };
    assert!(input.prepare_stalker_input().is_err());
}

#[test]
fn stalker_config_round_trips_extended_device_and_size_caps() {
    let mut input = ConfigInputDto::new_with_type(InputType::Stalker);
    input.name = "portal".into();
    input.stalker = Some(StalkerInputConfigDto {
        device: Some(StalkerDeviceProfileDto {
            mac_address: Some("00:1a:79:aa:bb:cc".to_string()),
            device_profile: Some("MAG254".to_string()),
            serial_number: Some("serial-1".to_string()),
            device_id: Some("device-1".to_string()),
            device_id2: Some("device-2".to_string()),
            signature: Some("sig-1".to_string()),
            ..Default::default()
        }),
        size_caps: Some(crate::model::stalker::StalkerActionSizeCapDto {
            create_link_kb: 96,
            ordered_list_mb: 12,
            get_epg_mb: 80,
        }),
        catalog_max_pages: Some(2048),
        ..Default::default()
    });

    let value = serde_json::to_value(&input).expect("serialize stalker input");
    let decoded: ConfigInputDto = serde_json::from_value(value).expect("deserialize stalker input");
    let stalker = decoded.stalker.expect("stalker config");
    let device = stalker.device.expect("device");
    assert_eq!(device.device_profile.as_deref(), Some("MAG254"));
    assert_eq!(device.serial_number.as_deref(), Some("serial-1"));
    assert_eq!(device.device_id.as_deref(), Some("device-1"));
    assert_eq!(device.device_id2.as_deref(), Some("device-2"));
    assert_eq!(device.signature.as_deref(), Some("sig-1"));
    let caps = stalker.size_caps.expect("size caps");
    assert_eq!(caps.create_link_kb, 96);
    assert_eq!(caps.ordered_list_mb, 12);
    assert_eq!(caps.get_epg_mb, 80);
    assert_eq!(stalker.catalog_max_pages, Some(2048));
}
