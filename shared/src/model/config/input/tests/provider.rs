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
fn test_provider_dns_prepare_normalizes_overrides_and_preserves_low_refresh() {
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

    // Explicit low values are honored (only warned about), not clamped.
    assert_eq!(dns.refresh_secs, 1);
    assert_eq!(dns.schemes, Some(vec![DnsScheme::Http, DnsScheme::Https]));
    let overrides = dns.overrides.expect("overrides should exist");
    assert_eq!(overrides.len(), 1);
    assert!(overrides.contains_key("example.com"));
    assert_eq!(overrides["example.com"].len(), 1);
}
