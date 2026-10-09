use super::*;

#[test]
fn config_input_alias_serialization_uses_block_default_and_reads_both_styles() {
    for aliases in [
            "- {name: alias-one, url: provider://example, username: alias, password: 'pass # with: {}, commas', max_connections: 1, exp_date: 1788779145}\n",
            "- name: alias-one\n  url: provider://example\n  username: alias\n  password: 'pass # with: {}, commas'\n  max_connections: 1\n  exp_date: 1788779145\n",
        ] {
            let yaml = format!(
                "name: provider\ntype: xtream\nurl: provider://example\nusername: root\npassword: pass\noptions:\n  update_quality:\n    live: 85\n    vod: 85\n    series: 85\n  resolve_background: false\ncache_duration: 20h\nmax_connections: 1\nexp_date: 1788779143\naliases:\n{aliases}panel_api:\n  url: http://panel.example\n  api_key: synthetic-key\n"
            );
            let input: ConfigInputDto = serde_saphyr::from_str(&yaml).expect("both alias styles parse");
            let serialized = serde_saphyr::to_string(&input).expect("serialize input");
            assert!(serialized.contains("- name: alias-one\n"));
            assert!(!serialized.contains("- {"));
            let reparsed: ConfigInputDto = serde_saphyr::from_str(&serialized).expect("read serialized input");
            assert_eq!(reparsed, input);
            let json = serde_json::to_string(&input).expect("JSON serialization");
            assert_eq!(serde_json::from_str::<ConfigInputDto>(&json).expect("JSON roundtrip"), input);

            let mut prepared = input;
            prepared.prepare(0, false, &HashSet::from(["example".to_string()]), None).expect("prepare input");
            assert_eq!(prepared.cache_duration_seconds, 72_000);
            let options = prepared.options.as_ref().expect("options");
            assert_eq!(options.update_quality, ConfigInputUpdateQualityDto { live: 85, vod: 85, series: 85 });
            assert!(!options.resolve_background);
            assert_eq!(prepared.aliases.as_ref().expect("aliases").len(), 1);
            assert!(prepared.panel_api.is_some());
        }
}

#[test]
fn config_input_alias_serialization_preserves_empty_and_missing_lists() {
    for aliases in [None, Some(Vec::new())] {
        let input = ConfigInputDto { name: "provider".intern(), aliases, ..Default::default() };
        let serialized = serde_saphyr::to_string(&input).expect("serialize input");
        assert_eq!(serde_saphyr::from_str::<ConfigInputDto>(&serialized).expect("roundtrip"), input);
    }
}

#[test]
fn test_epg_url_from_explicit_main_credentials() {
    let mut dto = create_test_dto();
    // Hier testen wir auch gleich mit, ob der Trailing Slash sauber entfernt wird!
    dto.url = "http://myprovider.com/".to_string();
    dto.username = Some("hello".to_string());
    dto.password = Some("mello".to_string());

    let result = dto.generate_auto_epg_url().unwrap();
    assert_eq!(result, "http://myprovider.com/xmltv.php?username=hello&password=mello");
}

#[test]
fn test_epg_url_from_enabled_alias_explicit_credentials() {
    let mut dto = create_test_dto();
    dto.url = "http://main.com".to_string();

    let alias = ConfigInputAliasDto {
        enabled: true,
        url: "http://alias.com".to_string(),
        username: Some("alias_user".to_string()),
        password: Some("alias_pass".to_string()),
        ..ConfigInputAliasDto::default()
    };

    dto.aliases = Some(vec![alias]);

    let result = dto.generate_auto_epg_url().unwrap();
    // Er muss die URL und die Credentials vom Alias nehmen
    assert_eq!(result, "http://alias.com/xmltv.php?username=alias_user&password=alias_pass");
}

#[test]
fn test_epg_url_skips_disabled_aliases() {
    let mut dto = create_test_dto();

    let alias = ConfigInputAliasDto {
        enabled: false, // Alias ist deaktiviert!
        url: "http://alias.com".to_string(),
        username: Some("alias_user".to_string()),
        password: Some("alias_pass".to_string()),
        ..ConfigInputAliasDto::default()
    };

    dto.aliases = Some(vec![alias]);

    let result = dto.generate_auto_epg_url();
    // Since the main DTO is empty and alias is disabled, an error must occur
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("no credentials could be extracted"));
}

#[test]
fn test_epg_url_fails_without_credentials() {
    let mut dto = create_test_dto();
    dto.url = "http://nocreds.com".to_string();

    let result = dto.generate_auto_epg_url();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("no credentials could be extracted"));
}

#[test]
fn test_epg_url_from_main_url_query_credentials() {
    let mut dto = create_test_dto();
    // Credentials stecken als Query-Parameter in der URL
    dto.url = "http://myprovider.com?username=hello&password=mello".to_string();

    let result = dto.generate_auto_epg_url().unwrap();

    // Durch unseren sauberen "clean_base" Fix sieht die URL jetzt richtig aus!
    assert_eq!(result, "http://myprovider.com/xmltv.php?username=hello&password=mello");
}

#[test]
fn test_epg_url_from_alias_url_query_credentials() {
    let mut dto = create_test_dto();
    dto.url = "http://main.com".to_string();

    let alias = ConfigInputAliasDto {
        enabled: true,
        // Credentials im Alias als Query-Parameter
        url: "http://alias.com?username=alias_user&password=alias_pass".to_string(),
        ..ConfigInputAliasDto::default()
    };

    dto.aliases = Some(vec![alias]);

    let result = dto.generate_auto_epg_url().unwrap();
    assert_eq!(result, "http://alias.com/xmltv.php?username=alias_user&password=alias_pass");
}

#[test]
fn test_epg_url_from_provider_scheme_url_query_credentials() {
    let mut dto = create_test_dto();
    dto.url = "provider://myprovider".to_string();
    dto.username = Some("test".to_string());
    dto.password = Some("secret".to_string());

    let result = dto.generate_auto_epg_url().unwrap();
    assert_eq!(result, "provider://myprovider/xmltv.php?username=test&password=secret");
}

#[test]
fn prepare_switches_xtream_to_xtream_batch_when_alias_exists() {
    let mut dto = ConfigInputDto {
        name: "input_alias".intern(),
        input_type: InputType::Xtream,
        url: "batch:///tmp/input_alias.csv".to_string(),
        aliases: Some(vec![ConfigInputAliasDto {
            id: 1,
            name: "alias_1".intern(),
            url: "http://provider.example/stream".to_string(),
            username: Some("u".to_string()),
            password: Some("p".to_string()),
            enabled: true,
            ..ConfigInputAliasDto::default()
        }]),
        ..ConfigInputDto::default()
    };

    dto.prepare_type().expect("prepare type should succeed");
    dto.prepare(0, true, &HashSet::new(), None).expect("prepare should succeed and infer batch type from batch:// URL");
    assert_eq!(dto.input_type, InputType::XtreamBatch);
}

#[test]
fn prepare_keeps_xtream_type_when_alias_exists_without_batch_url() {
    let mut dto = ConfigInputDto {
        name: "input_alias_http".intern(),
        input_type: InputType::XtreamBatch,
        url: "http://localhost:3001".to_string(),
        username: Some("root_user".to_string()),
        password: Some("root_pass".to_string()),
        aliases: Some(vec![ConfigInputAliasDto {
            id: 1,
            name: "alias_1".intern(),
            url: "http://provider.example/stream".to_string(),
            username: Some("u".to_string()),
            password: Some("p".to_string()),
            enabled: true,
            ..ConfigInputAliasDto::default()
        }]),
        ..ConfigInputDto::default()
    };

    dto.prepare_type().expect("prepare type should normalize non-batch URL to xtream");
    assert_eq!(dto.input_type, InputType::Xtream);
    dto.prepare(0, true, &HashSet::new(), None).expect("prepare should succeed for regular URL with aliases");
    assert_eq!(dto.input_type, InputType::Xtream);
}

#[test]
fn prepare_batch_url_does_not_require_xtream_credentials() {
    let mut dto = ConfigInputDto {
        name: "batch_no_creds".intern(),
        input_type: InputType::Xtream,
        url: "batch:///tmp/no-creds.csv".to_string(),
        username: None,
        password: None,
        ..ConfigInputDto::default()
    };

    dto.prepare(0, true, &HashSet::new(), None)
        .expect("batch:// input must be normalized before credential validation");
    assert_eq!(dto.input_type, InputType::XtreamBatch);
}

#[test]
fn prepare_provider_scheme_url_is_not_treated_as_batch_input() {
    let mut dto = ConfigInputDto {
        name: "batch_provider".intern(),
        input_type: InputType::XtreamBatch,
        url: "provider://myprovider".to_string(),
        username: Some("root_user".to_string()),
        password: Some("root_pass".to_string()),
        aliases: Some(vec![ConfigInputAliasDto {
            id: 1,
            name: "alias_1".intern(),
            url: "http://provider.example/stream".to_string(),
            username: Some("u".to_string()),
            password: Some("p".to_string()),
            enabled: true,
            ..ConfigInputAliasDto::default()
        }]),
        ..ConfigInputDto::default()
    };

    let err = dto
        .prepare(0, true, &HashSet::new(), None)
        .expect_err("prepare should treat provider:// URL as regular input (non-batch) and validate provider");
    assert!(err.to_string().contains("Provider name myprovider is not defined"), "Error: {err}");
}

#[test]
fn prepare_rejects_missing_input_url_even_with_aliases() {
    let mut dto = ConfigInputDto {
        name: "xtream_missing_root_url".intern(),
        input_type: InputType::Xtream,
        url: String::new(),
        username: Some("root_user".to_string()),
        password: Some("root_pass".to_string()),
        aliases: Some(vec![ConfigInputAliasDto {
            id: 1,
            name: "alias_1".intern(),
            url: "http://alias.example".to_string(),
            username: Some("alias_user".to_string()),
            password: Some("alias_pass".to_string()),
            enabled: true,
            ..ConfigInputAliasDto::default()
        }]),
        ..ConfigInputDto::default()
    };

    let err = dto
        .prepare(0, true, &HashSet::new(), None)
        .expect_err("prepare must require root input url even when aliases are present");
    assert!(err.to_string().contains("url for input is mandatory"), "Error: {err}");
    assert!(err.to_string().contains("xtream_missing_root_url"), "Error: {err}");
}

#[test]
fn prepare_rejects_missing_root_credentials_for_non_batch_url_even_with_aliases() {
    let mut dto = ConfigInputDto {
        name: "xtream_batch_missing_root_creds".intern(),
        input_type: InputType::XtreamBatch,
        url: "http://root.example".to_string(),
        aliases: Some(vec![ConfigInputAliasDto {
            id: 1,
            name: "alias_1".intern(),
            url: "http://alias.example".to_string(),
            username: Some("alias_user".to_string()),
            password: Some("alias_pass".to_string()),
            enabled: true,
            ..ConfigInputAliasDto::default()
        }]),
        ..ConfigInputDto::default()
    };

    let err = dto
        .prepare(0, true, &HashSet::new(), None)
        .expect_err("prepare must require root credentials for non-batch URL");
    assert!(err.to_string().contains("for input type xtream: username and password are mandatory"), "Error: {err}");
    assert!(err.to_string().contains("xtream_batch_missing_root_creds"), "Error: {err}");
}

#[test]
fn prepare_rejects_xtream_batch_batch_url_with_root_credentials_even_with_aliases() {
    let mut dto = ConfigInputDto {
        name: "xtream_batch_with_root_creds".intern(),
        input_type: InputType::XtreamBatch,
        url: "batch:///tmp/aliases.csv".to_string(),
        username: Some("root_user".to_string()),
        password: Some("root_pass".to_string()),
        aliases: Some(vec![ConfigInputAliasDto {
            id: 1,
            name: "alias_1".intern(),
            url: "http://alias.example".to_string(),
            username: Some("alias_user".to_string()),
            password: Some("alias_pass".to_string()),
            enabled: true,
            ..ConfigInputAliasDto::default()
        }]),
        ..ConfigInputDto::default()
    };

    let err = dto
        .prepare(0, true, &HashSet::new(), None)
        .expect_err("prepare must reject root credentials when using batch:// for xtream-batch");
    assert!(err.to_string().contains("with batch:// URL should not define username or password"), "Error: {err}");
    assert!(err.to_string().contains("xtream_batch_with_root_creds"), "Error: {err}");
}

#[test]
fn config_input_options_deserializes_legacy_prefixed_skip_aliases() {
    let dto: ConfigInputOptionsDto = serde_json::from_str(
        r#"{
                "xtream_skip_live": true,
                "stalker_skip_vod": true,
                "skip_series": true
            }"#,
    )
    .expect("legacy aliases should deserialize");

    assert!(dto.skip_live);
    assert!(dto.skip_vod);
    assert!(dto.skip_series);
}
