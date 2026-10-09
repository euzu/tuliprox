use super::*;

#[test]
fn test_provider_dns_defaults() {
    let dns = ProviderDnsDto::default();
    assert!(!dns.enabled);
    assert_eq!(dns.refresh_secs, 300);
    assert_eq!(dns.prefer, DnsPrefer::System);
    assert_eq!(dns.on_resolve_error, OnResolveErrorPolicy::KeepLastGood);
    assert_eq!(dns.on_connect_error, OnConnectErrorPolicy::TryNextIp);
    assert!(dns.schemes.is_none());
}

#[test]
fn test_provider_url_selection_policy_defaults_to_resume_last_working() {
    let provider = ConfigProviderDto {
        name: "provider-a".intern(),
        urls: vec!["http://primary.example.com".intern()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
        dns: None,
    };

    assert_eq!(provider.provider_url_selection_policy, ProviderUrlSelectionPolicy::ResumeLastWorking);
}

#[test]
fn test_provider_url_selection_policy_can_be_set_to_restart_from_first() {
    let provider = ConfigProviderDto {
        name: "provider-a".intern(),
        urls: vec!["http://primary.example.com".intern()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    };

    assert_eq!(provider.provider_url_selection_policy, ProviderUrlSelectionPolicy::RestartFromFirst);
}

#[test]
fn test_provider_url_selection_policy_deserializes_default_when_omitted() {
    let provider: ConfigProviderDto =
        serde_json::from_str(r#"{"name":"provider-a","urls":["http://primary.example.com"]}"#)
            .expect("provider dto should deserialize");

    assert_eq!(provider.provider_url_selection_policy, ProviderUrlSelectionPolicy::ResumeLastWorking);
}

#[test]
fn test_provider_url_selection_policy_deserializes_restart_from_first() {
    let provider: ConfigProviderDto = serde_json::from_str(
            r#"{"name":"provider-a","urls":["http://primary.example.com"],"provider_url_selection_policy":"restart_from_first"}"#,
        )
            .expect("provider dto should deserialize");

    assert_eq!(provider.provider_url_selection_policy, ProviderUrlSelectionPolicy::RestartFromFirst);
}

#[test]
fn test_provider_url_selection_policy_default_is_omitted_on_serialize() {
    let provider = ConfigProviderDto {
        name: "provider-a".intern(),
        urls: vec!["http://primary.example.com".intern()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    };

    let json = serde_json::to_string(&provider).expect("provider dto should serialize");
    let value: serde_json::Value = serde_json::from_str(&json).expect("serialized provider should be valid json");

    assert!(value.get("provider_url_selection_policy").is_none());
}

#[test]
fn test_provider_dns_prepare_normalizes_overrides_and_clamps_refresh() {
    let mut dns = ProviderDnsDto {
        refresh_secs: 1,
        schemes: Some(vec![DnsScheme::Http, DnsScheme::Http, DnsScheme::Https]),
        overrides: Some(HashMap::from([(
            "  EXAMPLE.COM ".to_string(),
            vec![
                "203.0.113.10".parse::<IpAddr>().expect("valid ip"),
                "203.0.113.10".parse::<IpAddr>().expect("valid ip"),
            ],
        )])),
        ..ProviderDnsDto::default()
    };

    dns.prepare().expect("dns prepare should succeed");

    assert_eq!(dns.refresh_secs, 1);
    assert_eq!(dns.schemes, Some(vec![DnsScheme::Http, DnsScheme::Https]));
    let overrides = dns.overrides.expect("overrides should exist");
    assert_eq!(overrides.len(), 1);
    assert!(overrides.contains_key("example.com"));
    assert_eq!(overrides["example.com"].len(), 1);
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
fn test_config_input_options_dto_filter_prepare_parses_valid_filter() {
    let mut dto = ConfigInputOptionsDto {
        resolve_filter: Some(r#"name ~ "test""#.to_string()),
        ..ConfigInputOptionsDto::default()
    };
    dto.prepare(None).expect("valid filter should parse");
    assert!(dto.t_resolve_filter.is_some());
}

#[test]
fn test_config_input_options_dto_filter_prepare_rejects_invalid_filter() {
    let mut dto = ConfigInputOptionsDto {
        resolve_filter: Some(r#"name ~ "["#.to_string()), // invalid regex
        ..ConfigInputOptionsDto::default()
    };
    let result = dto.prepare(None);
    assert!(result.is_err());
}

#[test]
fn test_config_input_options_dto_filter_prepare_with_unknown_template_placeholder() {
    let mut dto = ConfigInputOptionsDto {
        resolve_filter: Some(r#"name ~ "!UNKNOWN!""#.to_string()),
        ..ConfigInputOptionsDto::default()
    };
    let result = dto.prepare(None);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Unknown template placeholder"));
}

#[test]
fn test_config_input_options_dto_filter_none_prepares_successfully() {
    let mut dto = ConfigInputOptionsDto { resolve_filter: None, ..ConfigInputOptionsDto::default() };
    dto.prepare(None).expect("None filter should prepare successfully");
    assert!(dto.t_resolve_filter.is_none());
}
