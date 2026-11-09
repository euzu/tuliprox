use super::*;

#[test]
fn canonical_strips_path_and_query_credentials() {
    let url = "https://a.example/live/user1/pass1/abc.ts?type=m3u&username=user1&password=pass1";
    let creds = ["user1", "pass1"];
    let canonical = canonical_override_url(url, &creds).expect("canonical url");
    assert_eq!(canonical, "/live/abc.ts?type=m3u");
}

#[test]
fn canonical_host_port_scheme_independent() {
    let url1 = "https://a.example/live/user1/pass1/abc.ts?type=m3u";
    let url2 = "http://b.example:8080/live/user2/pass2/abc.ts?type=m3u";
    let creds = ["user1", "pass1", "user2", "pass2"];

    let canonical1 = canonical_override_url(url1, &creds).expect("canonical url1");
    let canonical2 = canonical_override_url(url2, &creds).expect("canonical url2");

    assert_eq!(canonical1, "/live/abc.ts?type=m3u");
    assert_eq!(canonical2, "/live/abc.ts?type=m3u");
    assert_eq!(canonical1, canonical2);
}

#[test]
fn canonical_encoded_credentials() {
    let url = "https://example.com/live/user1/p%40ss/abc.ts";
    let creds = ["user1", "p@ss"];
    let canonical = canonical_override_url(url, &creds).expect("canonical url");
    assert_eq!(canonical, "/live/abc.ts");

    let url_plain = "https://example.com/live/user1/p@ss/abc.ts";
    let creds_encoded = ["user1", "p%40ss"];
    let canonical_plain = canonical_override_url(url_plain, &creds_encoded).expect("canonical plain");
    assert_eq!(canonical_plain, "/live/abc.ts");
}

#[test]
fn canonical_keeps_segment_resembling_credential_substring() {
    let url = "https://example.com/live/pass1x/abc.ts";
    let creds = ["pass1"];
    let canonical = canonical_override_url(url, &creds).expect("canonical url");
    assert_eq!(canonical, "/live/pass1x/abc.ts");
}

#[test]
fn canonical_sorts_query() {
    let url = "https://example.com/live/abc.ts?z=1&a=2&m=3&a=1";
    let no_creds: &[&str] = &[];
    let canonical = canonical_override_url(url, no_creds).expect("canonical url");
    assert_eq!(canonical, "/live/abc.ts?a=1&a=2&m=3&z=1");
}

#[test]
fn hash_has_v1_prefix() {
    let url = "https://example.com/live/user1/pass1/abc.ts?type=m3u";
    let creds = ["user1", "pass1"];
    let hash = override_url_hash(url, &creds).expect("url hash");

    assert!(hash.starts_with("urlhash-v1:"));
    let hex_part = &hash["urlhash-v1:".len()..];
    assert_eq!(hex_part.len(), 64);
    assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn canonical_invalid_utf8_does_not_collapse() {
    let url1 = "https://example.com/live/%FF/abc.ts";
    let url2 = "https://example.com/live/%FE/abc.ts";
    let no_creds: &[&str] = &[];

    let canonical1 = canonical_override_url(url1, no_creds).expect("canonical url1");
    let canonical2 = canonical_override_url(url2, no_creds).expect("canonical url2");

    assert_eq!(canonical1, "/live/%FF/abc.ts");
    assert_eq!(canonical2, "/live/%FE/abc.ts");
    assert_ne!(canonical1, canonical2);

    let hash1 = override_url_hash(url1, no_creds).expect("hash1");
    let hash2 = override_url_hash(url2, no_creds).expect("hash2");
    assert_ne!(hash1, hash2);
}

#[test]
fn canonical_unparsable_url_returns_none() {
    let no_creds: &[&str] = &[];
    assert_eq!(canonical_override_url("not a url", no_creds), None);
    assert_eq!(override_url_hash("not a url", no_creds), None);
}

#[test]
fn query_param_value_equal_to_credential_dropped() {
    let url = "https://example.com/live/abc.ts?token=secret123&type=m3u";
    let creds = ["secret123"];
    let canonical = canonical_override_url(url, &creds).expect("canonical url");
    assert_eq!(canonical, "/live/abc.ts?type=m3u");
}

#[test]
fn query_param_encoded_credential_value_dropped() {
    let url = "https://example.com/live/abc.ts?user=p%40ss&type=m3u";
    let creds = ["p@ss"];
    let canonical = canonical_override_url(url, &creds).expect("canonical url");
    assert_eq!(canonical, "/live/abc.ts?type=m3u");
}
