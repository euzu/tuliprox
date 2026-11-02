use super::*;
use serde_json::json;

fn release(version: &str) -> Value {
    json!({"version": version, "package_url": "https://example.com/p.tlpx", "plugin_api": 1})
}

fn plugin(id: &str, releases: &[Value]) -> Value { json!({"id": id, "name": "Demo", "releases": releases}) }

fn catalog(plugins: &[Value]) -> Vec<u8> {
    serde_json::to_vec(&json!({"format_version": 1, "plugins": plugins})).unwrap()
}

fn host() -> Version { Version::new(3, 3, 175) }

#[test]
fn a_minimal_catalog_parses_and_unknown_fields_are_ignored() {
    let bytes = serde_json::to_vec(&json!({
        "format_version": 1, "future": true,
        "plugins": [{"id": "demo", "name": "Demo", "sha256": "ignored", "releases": [
            {"version": "1.0.0", "package_url": "https://example.com/p.tlpx", "plugin_api": 1, "checksum": "x"}
        ]}]
    }))
    .unwrap();
    let (parsed, warnings) = parse(&bytes, false).unwrap();
    assert_eq!(warnings.len(), 0);
    assert_eq!(parsed.plugins.len(), 1);
    assert_eq!(parsed.plugins[0].releases[0].version, "1.0.0");
}

#[test]
fn catalog_level_problems_fail_the_parse() {
    assert!(parse(b"not json", false).is_err());
    assert!(parse(b"[]", false).is_err());
    for bad in [json!({"plugins": []}), json!({"format_version": 0, "plugins": []})] {
        assert!(parse(&serde_json::to_vec(&bad).unwrap(), false).unwrap_err().contains("format_version"));
    }
    let newer = serde_json::to_vec(&json!({"format_version": 2, "plugins": []})).unwrap();
    assert_eq!(parse(&newer, false).unwrap_err(), "format_version 2 is newer than this Tuliprox understands (1)");
    let duplicate = catalog(&[plugin("demo", &[release("1.0.0")]), plugin("demo", &[release("1.1.0")])]);
    assert!(parse(&duplicate, false).unwrap_err().contains("duplicate plugin id"));
}

#[test]
fn an_invalid_plugin_is_skipped_with_a_warning_and_the_rest_survive() {
    let bytes = catalog(&[
        plugin("Bad_Id", &[release("1.0.0")]),
        plugin("staging", &[release("1.0.0")]),
        json!({"id": "noname", "releases": [release("1.0.0")]}),
        plugin("norelease", &[]),
        plugin("good", &[release("1.0.0")]),
    ]);
    let (parsed, warnings) = parse(&bytes, false).unwrap();
    assert_eq!(parsed.plugins.iter().map(|plugin| plugin.id.as_str()).collect::<Vec<_>>(), ["good"]);
    assert_eq!(warnings.len(), 4, "{warnings:?}");
}

#[test]
fn an_invalid_release_is_skipped_with_a_warning() {
    let mut http = release("1.1.0");
    http["package_url"] = json!("http://example.com/p.tlpx");
    let mut file = release("1.2.0");
    file["package_url"] = json!("file:///etc/passwd");
    let mut credentials = release("1.3.0");
    credentials["package_url"] = json!("https://user:pw@example.com/p.tlpx");
    let mut bad_min = release("1.4.0");
    bad_min["min_tuliprox_version"] = json!("three");
    let mut no_api = release("1.5.0");
    no_api.as_object_mut().unwrap().remove("plugin_api");
    let bytes = catalog(&[plugin(
        "demo",
        &[release("1.0.0"), release("1.0.0"), release("latest"), http, file, credentials, bad_min, no_api],
    )]);
    let (parsed, warnings) = parse(&bytes, false).unwrap();
    assert_eq!(parsed.plugins[0].releases.len(), 1);
    assert_eq!(warnings.len(), 7, "{warnings:?}");
}

#[test]
fn http_package_urls_pass_only_under_the_knob() {
    let mut http = release("1.0.0");
    http["package_url"] = json!("http://127.0.0.1:8080/p.tlpx");
    let bytes = catalog(&[plugin("demo", &[http])]);
    assert_eq!(parse(&bytes, false).unwrap().0.plugins.len(), 0);
    assert_eq!(parse(&bytes, true).unwrap().0.plugins.len(), 1);
}

#[test]
fn texts_are_sanitized_and_cut() {
    let mut entry = plugin("demo", &[release("1.0.0")]);
    entry["name"] = json!(format!("Na\u{202E}me\n{}", "x".repeat(200)));
    entry["homepage"] = json!("javascript:alert(1)");
    let (parsed, _) = parse(&catalog(&[entry]), false).unwrap();
    let plugin = &parsed.plugins[0];
    assert!(!plugin.name.contains('\u{202E}') && !plugin.name.contains('\n'));
    assert_eq!(plugin.name.chars().count(), 80);
    assert!(plugin.homepage.is_none());
}

#[test]
fn compatibility_checks_the_plugin_api_and_the_minimum_version() {
    let make = |min: Option<&str>, api: u32| Release {
        version: "1.0.0".into(),
        package_url: "https://example.com/p".into(),
        min_tuliprox_version: min.map(str::to_string),
        plugin_api: api,
        capabilities: Vec::new(),
        changelog: None,
        published: None,
    };
    assert!(compatibility(&make(None, 1), &host(), 1).is_ok());
    assert!(compatibility(&make(Some("3.3.175"), 1), &host(), 1).is_ok());
    assert!(compatibility(&make(Some("3.0.0"), 1), &host(), 1).is_ok());
    assert!(compatibility(&make(Some("3.4.0"), 1), &host(), 1).unwrap_err().contains("3.4.0"));
    assert!(compatibility(&make(None, 2), &host(), 1).unwrap_err().contains("plugin API 2"));
}

#[test]
fn releases_sort_newest_first_with_compatible_ones_before_incompatible() {
    let make = |version: &str, api: u32| Release {
        version: version.into(),
        package_url: "https://example.com/p".into(),
        min_tuliprox_version: None,
        plugin_api: api,
        capabilities: Vec::new(),
        changelog: None,
        published: None,
    };
    let rows = release_dtos(&[make("1.2.0", 1), make("2.0.0", 2), make("1.10.0", 1), make("1.9.0", 1)], &host(), 1);
    let order: Vec<_> = rows.iter().map(|row| (row.version.as_str(), row.compatible)).collect();
    assert_eq!(order, [("1.10.0", true), ("1.9.0", true), ("1.2.0", true), ("2.0.0", false)]);
    assert!(rows[3].incompatible_reason.is_some());
}
