use super::*;

#[test]
fn prepare_rejects_blank_media_server_credentials_and_selectors() {
    let mut emby = ConfigInputDto {
        name: "emby_media_server".intern(),
        input_type: InputType::Emby,
        url: "https://media.example.invalid".to_string(),
        media_server: Some(MediaServerInputConfigDto {
            token: Some("   ".to_string()),
            api_key: Some(String::new()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };
    let err = prepare_dto(&mut emby).expect_err("blank token/api_key should be rejected");
    assert!(err.to_string().contains("requires media_server token/api_key"));

    let mut plex = ConfigInputDto {
        name: "plex_media_server".intern(),
        input_type: InputType::Plex,
        media_server: Some(MediaServerInputConfigDto {
            account_token: Some("   ".to_string()),
            server_id: Some("   ".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };
    let err = prepare_dto(&mut plex).expect_err("blank plex token should be rejected");
    assert!(err.to_string().contains("requires media_server.account_token"));
}

#[test]
fn prepare_accepts_media_server_max_connections_as_stream_limit() {
    let mut dto = ConfigInputDto {
        name: "emby_media_server".intern(),
        input_type: InputType::Emby,
        url: "https://media.example.invalid".to_string(),
        max_connections: 1,
        media_server: Some(MediaServerInputConfigDto {
            token: Some("token-value".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };

    prepare_dto(&mut dto).expect("media_server inputs reuse max_connections stream-limit semantics");
}

#[test]
fn media_server_defaults_are_conservative() {
    let media_server = MediaServerInputConfigDto::default();

    assert_eq!(media_server.catalog.page_size, 100);
    assert_eq!(media_server.catalog.request_delay_ms, 250);
    assert!(media_server.catalog.include_media_sources);
    assert!(!media_server.catalog.include_paths);
    assert!(!media_server.catalog.include_user_state);
    assert!(!media_server.catalog.refresh_on_startup);
    assert!(media_server.playback.direct_play_only);
    assert!(!media_server.playback.allow_transcode);
    assert!(!media_server.playback.preflight_streams);
    assert_eq!(media_server.image_policy, MediaServerImagePolicy::ProxyOnDemand);
    assert!(!media_server.allow_relay);
}

#[test]
fn media_server_library_key_selector_accepts_numeric_yaml_scalars() {
    let media_server =
        serde_json::from_str::<MediaServerInputConfigDto>(r#"{"libraries":[{"key":10,"kind":"movies"}]}"#)
            .expect("numeric YAML-like key selectors should deserialize as strings");

    assert_eq!(
        media_server.libraries,
        vec![MediaServerLibrarySelector::Detailed(MediaServerLibrarySelectorDetailsDto {
            key: Some("10".to_string()),
            kind: Some(MediaServerLibraryKind::Movies),
            ..MediaServerLibrarySelectorDetailsDto::default()
        })]
    );

    let by_id = serde_json::from_str::<MediaServerInputConfigDto>(r#"{"libraries":[{"id":42,"kind":"tvshows"}]}"#)
        .expect("numeric id selectors should deserialize as strings");
    assert_eq!(
        by_id.libraries,
        vec![MediaServerLibrarySelector::Detailed(MediaServerLibrarySelectorDetailsDto {
            id: Some("42".to_string()),
            kind: Some(MediaServerLibraryKind::TvShows),
            ..MediaServerLibrarySelectorDetailsDto::default()
        })]
    );
}

#[test]
fn media_server_enrichment_block_is_not_part_of_schema() {
    let err = serde_json::from_str::<MediaServerInputConfigDto>(
        r#"{"libraries":["Movies"],"enrichment":{"ffprobe":true,"tmdb_lookup":true,"fetch_images":true}}"#,
    )
    .expect_err("media_server.enrichment must not be accepted");

    assert!(err.to_string().contains("unknown field `enrichment`"));
}

#[test]
fn prepare_accepts_emby_media_server_with_token_and_library() {
    let mut dto = ConfigInputDto {
        name: "emby_media_server".intern(),
        input_type: InputType::Emby,
        url: " https://media.example.invalid/ ".to_string(),
        media_server: Some(MediaServerInputConfigDto {
            token: Some(" token-value ".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };

    prepare_dto(&mut dto).expect("emby media_server config should prepare");

    assert_eq!(dto.url, "https://media.example.invalid/");
    assert!(dto.input_type.is_media_server());
    assert_eq!(dto.media_server.as_ref().and_then(|media_server| media_server.token.as_deref()), Some("token-value"));
}

#[test]
fn prepare_rejects_media_server_without_media_server_block() {
    let mut dto = ConfigInputDto {
        name: "emby_media_server".intern(),
        input_type: InputType::Emby,
        url: "https://media.example.invalid".to_string(),
        ..ConfigInputDto::default()
    };

    let err = prepare_dto(&mut dto).expect_err("media_server block should be mandatory");
    assert!(err.to_string().contains("media_server configuration is mandatory"));
}

#[test]
fn prepare_allows_disabled_media_server_input_with_incomplete_config() {
    let mut dto = ConfigInputDto {
        name: "disabled_plex".intern(),
        input_type: InputType::Plex,
        enabled: false,
        ..ConfigInputDto::default()
    };

    prepare_dto(&mut dto).expect("disabled media_server input should not require active playback/catalog config");
    assert_eq!(dto.input_type, InputType::Plex);
    assert!(!dto.enabled);
}

#[test]
fn prepare_normalizes_disabled_media_server_config_without_enforcing_invariants() {
    let mut dto = ConfigInputDto {
        name: "disabled_emby".intern(),
        input_type: InputType::Emby,
        enabled: false,
        media_server: Some(MediaServerInputConfigDto {
            token: Some(" token-value ".to_string()),
            libraries: vec![MediaServerLibrarySelector::Name("   ".to_string())],
            catalog: MediaServerCatalogConfigDto { page_size: 0, ..MediaServerCatalogConfigDto::default() },
            ..MediaServerInputConfigDto::default()
        }),
        ..ConfigInputDto::default()
    };

    prepare_dto(&mut dto).expect("disabled media_server input can preserve incomplete config for later repair");
    let media_server = dto.media_server.as_ref().expect("media_server config should be preserved");
    assert_eq!(media_server.token.as_deref(), Some("token-value"));
    assert!(media_server.libraries[0].is_empty());
}

#[test]
fn prepare_rejects_emby_media_server_without_input_url() {
    let mut dto = ConfigInputDto {
        name: "emby_media_server".intern(),
        input_type: InputType::Emby,
        media_server: Some(MediaServerInputConfigDto {
            token: Some("token-value".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };

    let err = prepare_dto(&mut dto).expect_err("emby media_server input should require a direct server URL");
    assert!(err.to_string().contains("url is mandatory for input type emby"));
}

#[test]
fn prepare_rejects_media_server_provider_scheme_url() {
    let mut dto = ConfigInputDto {
        name: "emby_media_server".intern(),
        input_type: InputType::Emby,
        url: " provider://media-server ".to_string(),
        media_server: Some(MediaServerInputConfigDto {
            token: Some("token-value".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };

    let err = prepare_dto(&mut dto).expect_err("media_server input must not use provider URLs");
    assert!(err.to_string().contains("does not support batch:// or provider://"));
}

#[test]
fn prepare_rejects_plex_without_token_or_server_selector() {
    let mut without_token = ConfigInputDto {
        name: "plex_media_server".intern(),
        input_type: InputType::Plex,
        media_server: Some(MediaServerInputConfigDto {
            server_id: Some("server".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };
    let err = prepare_dto(&mut without_token).expect_err("plex token should be mandatory");
    assert!(err.to_string().contains("requires media_server.account_token"));

    let mut without_selector = ConfigInputDto {
        name: "plex_media_server".intern(),
        input_type: InputType::Plex,
        media_server: Some(MediaServerInputConfigDto {
            account_token: Some("token".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };
    let err = prepare_dto(&mut without_selector).expect_err("plex server selector should be mandatory");
    assert!(err.to_string().contains("requires a server selector"));
}

#[test]
fn prepare_accepts_plex_without_input_url_when_discovery_is_configured() {
    let mut dto = ConfigInputDto {
        name: "plex_media_server".intern(),
        input_type: InputType::Plex,
        media_server: Some(MediaServerInputConfigDto {
            account_token: Some("token".to_string()),
            server_id: Some("server".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };

    prepare_dto(&mut dto).expect("plex discovery config should not require input.url");
    assert_eq!(dto.input_type, InputType::Plex);
}

#[test]
fn prepare_accepts_plex_media_server_with_direct_url_without_selector() {
    let mut dto = ConfigInputDto {
        name: "plex_media_server".intern(),
        input_type: InputType::Plex,
        url: "https://plex.example.invalid".to_string(),
        media_server: Some(MediaServerInputConfigDto {
            token: Some("token".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };

    prepare_dto(&mut dto).expect("direct Plex URL should not require MyPlex server selector");
    assert_eq!(dto.input_type, InputType::Plex);
}

#[test]
fn prepare_rejects_plex_direct_url_without_server_token() {
    let mut dto = ConfigInputDto {
        name: "plex_media_server".intern(),
        input_type: InputType::Plex,
        url: "https://plex.example.invalid".to_string(),
        media_server: Some(MediaServerInputConfigDto {
            account_token: Some("account-token".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };

    let err = prepare_dto(&mut dto).expect_err("direct Plex URL requires the PMS server token");
    assert!(err.to_string().contains("requires media_server.token"));
}

#[test]
fn prepare_rejects_plex_discovery_without_account_token() {
    let mut dto = ConfigInputDto {
        name: "plex_media_server".intern(),
        input_type: InputType::Plex,
        media_server: Some(MediaServerInputConfigDto {
            token: Some("server-token".to_string()),
            server_id: Some("server".to_string()),
            ..media_server_config_with_library()
        }),
        ..ConfigInputDto::default()
    };

    let err = prepare_dto(&mut dto).expect_err("Plex discovery requires the MyPlex account token");
    assert!(err.to_string().contains("requires media_server.account_token"));
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
