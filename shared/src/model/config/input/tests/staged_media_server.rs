use super::*;

#[test]
fn sequential_group_rejects_zero_and_staged_inputs() {
    let mut zero = ConfigInputDto {
        name: "zero_group".intern(),
        url: "http://example.com/list.m3u".to_string(),
        sequential_group: Some(0),
        ..ConfigInputDto::default()
    };
    let err = prepare_dto(&mut zero).expect_err("group zero must be rejected");
    assert!(err.to_string().contains("sequential_group"), "Error: {err}");

    let mut staged = ConfigInputDto {
        name: "staged_group".intern(),
        input_type: InputType::Staged,
        url: "http://example.com/list.m3u".to_string(),
        staged: Some(ConfigInputStagedDto {
            for_input: Some("provider_a".intern()),
            ..ConfigInputStagedDto::default()
        }),
        sequential_group: Some(1),
        ..ConfigInputDto::default()
    };
    let err = prepare_dto(&mut staged).expect_err("staged groups must be rejected");
    assert!(err.to_string().contains("sequential_group"), "Error: {err}");
}

#[test]
fn prepare_type_does_not_validate_media_server_config() {
    let mut dto = ConfigInputDto {
        name: "emby_media_server".intern(),
        input_type: InputType::Emby,
        url: "https://media.example.invalid".to_string(),
        ..ConfigInputDto::default()
    };

    dto.prepare_type().expect("prepare_type only normalizes type/url");
    let err = prepare_dto(&mut dto).expect_err("full prepare should validate missing media_server block");
    assert!(err.to_string().contains("media_server configuration is mandatory"));
}

#[test]
fn test_staged_requires_provider() {
    let mut dto = create_test_dto();
    dto.input_type = InputType::Staged;
    dto.url = "http://staged.com/playlist.m3u".to_string();

    let err = prepare_dto(&mut dto).expect_err("staged input without provider must be rejected");
    assert!(err.to_string().contains("requires a provider input"), "Error: {err}");
}

#[test]
fn test_staged_requires_url() {
    let mut dto = create_test_dto();
    dto.input_type = InputType::Staged;
    dto.staged =
        Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), ..ConfigInputStagedDto::default() });
    dto.url = String::new();

    let err = prepare_dto(&mut dto).expect_err("staged input without url must be rejected");
    assert!(err.to_string().contains("url for staged input is mandatory"), "Error: {err}");
}

#[test]
fn test_staged_requires_non_empty_clusters() {
    let mut dto = create_test_dto();
    dto.input_type = InputType::Staged;
    dto.staged = Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), clusters: ClusterFlags::empty() });
    dto.url = "http://staged.com/playlist.m3u".to_string();

    let err = prepare_dto(&mut dto).expect_err("staged input without clusters must be rejected");
    assert!(err.to_string().contains("requires at least one staged cluster"), "Error: {err}");
}

#[test]
fn test_staged_config_only_allowed_for_staged() {
    let mut dto = create_test_dto();
    dto.input_type = InputType::M3u;
    dto.url = "http://main.com/playlist.m3u".to_string();
    dto.staged =
        Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), ..ConfigInputStagedDto::default() });

    let err = prepare_dto(&mut dto).expect_err("non-staged input with staged config must be rejected");
    assert!(err.to_string().contains("staged configuration is only allowed for staged inputs"), "Error: {err}");
}

#[test]
fn test_staged_rejects_media_server() {
    let mut dto = create_test_dto();
    dto.input_type = InputType::Staged;
    dto.staged =
        Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), ..ConfigInputStagedDto::default() });
    dto.url = "http://staged.com/playlist.m3u".to_string();
    dto.media_server = Some(MediaServerInputConfigDto::default());

    let err = prepare_dto(&mut dto).expect_err("staged input with media_server must be rejected");
    assert!(err.to_string().contains("does not support media_server configuration"), "Error: {err}");
}

#[test]
fn test_staged_valid() {
    let mut dto = create_test_dto();
    dto.input_type = InputType::Staged;
    dto.staged =
        Some(ConfigInputStagedDto { for_input: Some(" provider_a ".intern()), ..ConfigInputStagedDto::default() });
    dto.url = "http://staged.com/playlist.m3u".to_string();

    prepare_dto(&mut dto).expect("valid staged input should prepare successfully");
    assert!(dto.input_type.is_staged());
    assert_eq!(dto.staged.as_ref().and_then(|staged| staged.for_input.as_deref()), Some("provider_a"));
}

#[test]
fn test_staged_provider_alias_deserializes_as_for_input() {
    let staged: ConfigInputStagedDto =
        serde_json::from_str(r#"{"provider":"provider_a"}"#).expect("legacy provider field should deserialize");

    assert_eq!(staged.for_input.as_deref(), Some("provider_a"));
}

#[test]
fn test_staged_ignores_stream_runtime_fields() {
    let mut dto = create_test_dto();
    dto.input_type = InputType::Staged;
    dto.url = "http://staged.com/playlist.m3u".to_string();
    dto.staged =
        Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), ..ConfigInputStagedDto::default() });
    dto.priority = -10;
    dto.max_connections = 2;
    dto.cache_duration = Some("not-a-duration".to_string());

    prepare_dto(&mut dto).expect("staged stream runtime fields should be ignored");

    assert_eq!(dto.priority, 0);
    assert_eq!(dto.max_connections, 0);
    assert_eq!(dto.cache_duration, None);
    assert_eq!(dto.cache_duration_seconds, 0);
}
