use crate::api::source_yml_patch::resolve_provisioned_account_base_url;
use url::Url;

#[test]
fn resolve_base_url_updates_provider_query_credentials_when_present() {
    let result = resolve_provisioned_account_base_url(
        "provider://demo-provider/live?foo=bar&username=old&password=oldpw",
        Some("http://panel.example.com:8080/get.php?username=new&password=new"),
        "new-user",
        "new-pass",
    );

    let parsed = Url::parse(result.as_str()).expect("expected valid provider url");
    let pairs: Vec<(String, String)> = parsed.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    assert!(pairs.contains(&("foo".to_string(), "bar".to_string())));
    assert!(pairs.contains(&("username".to_string(), "new-user".to_string())));
    assert!(pairs.contains(&("password".to_string(), "new-pass".to_string())));
}

#[test]
fn resolve_base_url_falls_back_when_panel_response_is_literal_null() {
    let result =
        resolve_provisioned_account_base_url("http://input.example.org/path?x=1", Some("null"), "new", "secret");

    assert_eq!(result, "http://input.example.org/path?x=1");
}
