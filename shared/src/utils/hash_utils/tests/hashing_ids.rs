use super::*;

// ── extract_id_from_url ────────────────────────────────────────────

#[test]
fn numeric_id_with_ts_extension() {
    assert_eq!(extract_id_from_url("http://server.com/live/user/pass/950327.ts"), "950327");
}

#[test]
fn numeric_id_with_mkv_extension() {
    assert_eq!(extract_id_from_url("http://server.com/movie/user/pass/12345.mkv"), "12345");
}

#[test]
fn numeric_id_without_extension() {
    assert_eq!(extract_id_from_url("http://server.com/live/user/pass/950327"), "950327");
}

#[test]
fn numeric_id_with_query_string() {
    assert_eq!(
        extract_id_from_url("http://server.com/live/user/pass/1905905.ts?expires=1712345678&token=abc"),
        "1905905"
    );
}

#[test]
fn numeric_id_with_trailing_slash() {
    assert_eq!(extract_id_from_url("http://server.com/live/user/pass/42/"), "42");
}

#[test]
fn non_numeric_last_segment_returns_hash() {
    let result = extract_id_from_url("http://server.com/live/user/pass/hello.ts");
    assert!(!result.chars().all(|c| c.is_ascii_digit()), "should be a hash, not a numeric id");
    assert!(!result.is_empty());
}

#[test]
fn mixed_alphanumeric_segment_returns_hash() {
    let result = extract_id_from_url("http://server.com/live/user/pass/abc123.ts");
    assert!(!result.chars().all(|c| c.is_ascii_digit()), "mixed segment should not be treated as numeric id");
}

#[test]
fn no_slash_returns_hash() {
    let result = extract_id_from_url("12345");
    assert!(!result.chars().all(|c| c.is_ascii_digit()), "bare number without slash must not be parsed as id");
}

#[test]
fn empty_url_returns_hash() {
    let result = extract_id_from_url("");
    assert!(!result.is_empty());
}

#[test]
fn only_slashes_returns_hash() {
    let result = extract_id_from_url("///");
    assert!(!result.is_empty());
}

#[test]
fn numeric_id_after_scheme() {
    assert_eq!(extract_id_from_url("http://server.com/42"), "42");
}

#[test]
fn same_url_same_hash() {
    let a = extract_id_from_url("http://server.com/stream/hello.m3u8");
    let b = extract_id_from_url("http://server.com/stream/hello.m3u8");
    assert_eq!(a, b, "same non-numeric url should produce identical hash");
}

#[test]
fn different_url_different_hash() {
    let a = extract_id_from_url("http://a.com/hello.ts");
    let b = extract_id_from_url("http://b.com/world.ts");
    assert_ne!(a, b);
}

#[test]
fn query_string_ignored_for_numeric_extraction() {
    // The query string should not interfere with numeric extraction
    assert_eq!(extract_id_from_url("http://server.com/99999.ts?token=xyz"), "99999");
}

#[test]
fn fragment_after_numeric_id() {
    // Fragments don't contain '?' so they stay in the path — still numeric after '/'
    assert_eq!(extract_id_from_url("http://server.com/live/55555.ts#start"), "55555");
}

#[test]
fn non_numeric_query() {
    assert_eq!(
        extract_id_from_url("http://server.com/player_api.php?username=123443&password=1000"),
        "7D2F521803B23A43"
    );
}

// ── extract_numeric_id_from_url ────────────────────────────────────

#[test]
fn numeric_url_id_standard() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/live/user/pass/950327.ts"), Some(950327));
}

#[test]
fn numeric_url_id_no_extension() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/live/user/pass/42"), Some(42));
}

#[test]
fn numeric_url_id_with_query() {
    assert_eq!(
        extract_numeric_id_from_url("http://server.com/live/user/pass/1905905.ts?token=abc&expires=999"),
        Some(1905905)
    );
}

#[test]
fn numeric_url_id_trailing_slash() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/path/123/"), Some(123));
}

#[test]
fn numeric_url_id_zero() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/live/0.ts"), Some(0));
}

#[test]
fn numeric_url_id_non_numeric_segment() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/live/hello.ts"), None);
}

#[test]
fn numeric_url_id_mixed_alphanumeric() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/live/abc123.ts"), None);
}

#[test]
fn numeric_url_id_no_slash() {
    assert_eq!(extract_numeric_id_from_url("12345"), None);
}

#[test]
fn numeric_url_id_empty() {
    assert_eq!(extract_numeric_id_from_url(""), None);
}

#[test]
fn numeric_url_id_query_digits_not_matched() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/player_api.php?username=123"), None);
}

#[test]
fn numeric_url_id_overflow_returns_none() {
    // u32::MAX = 4294967295, this exceeds it
    assert_eq!(extract_numeric_id_from_url("http://server.com/live/99999999999.ts"), None);
}

#[test]
fn numeric_url_id_fragment() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/live/55555.ts#start"), Some(55555));
}

#[test]
fn numeric_url_id_fragment_no_extension() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/live/55555#start"), Some(55555));
}

#[test]
fn numeric_url_id_trailing_slash_before_query() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/123/?foo=bar"), Some(123));
}

#[test]
fn numeric_url_id_trailing_slash_before_fragment() {
    assert_eq!(extract_numeric_id_from_url("http://server.com/456/#section"), Some(456));
}

// ── hash_bytes / hash_string ─────────────────────────────────────

#[test]
fn hash_bytes_deterministic() {
    let a = hash_bytes(b"hello");
    let b = hash_bytes(b"hello");
    assert_eq!(a, b);
}

#[test]
fn hash_bytes_different_input_different_output() {
    assert_ne!(hash_bytes(b"hello"), hash_bytes(b"world"));
}

#[test]
fn hash_string_matches_hash_bytes() {
    assert_eq!(hash_string("test"), hash_bytes(b"test"));
}

// ── short_hash ───────────────────────────────────────────────────

#[test]
fn short_hash_deterministic() {
    assert_eq!(short_hash("abc"), short_hash("abc"));
}

#[test]
fn short_hash_length_is_16_hex_chars() {
    // 8 bytes -> 16 hex characters
    assert_eq!(short_hash("anything").len(), 16);
}

#[test]
fn short_hash_different_input() {
    assert_ne!(short_hash("abc"), short_hash("xyz"));
}

// ── hex_encode / hex_decode ──────────────────────────────────────

#[test]
fn hex_encode_known_value() {
    assert_eq!(hex_encode(&[0xDE, 0xAD, 0xBE, 0xEF]), "DEADBEEF");
}

#[test]
fn hex_encode_empty() {
    assert_eq!(hex_encode(&[]), "");
}

#[test]
fn hex_decode_known_value() {
    assert_eq!(hex_decode("DEADBEEF").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn hex_decode_lowercase() {
    assert_eq!(hex_decode("deadbeef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn hex_decode_empty() {
    assert_eq!(hex_decode("").unwrap(), Vec::<u8>::new());
}

#[test]
fn hex_decode_odd_length_error() {
    assert!(hex_decode("ABC").is_err());
}

#[test]
fn hex_decode_invalid_char_error() {
    assert!(hex_decode("ZZZZ").is_err());
}

#[test]
fn hex_roundtrip() {
    let original = vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];
    assert_eq!(hex_decode(&hex_encode(&original)).unwrap(), original);
}

// ── hash_string_as_hex ───────────────────────────────────────────

#[test]
fn hash_string_as_hex_deterministic() {
    assert_eq!(hash_string_as_hex("url"), hash_string_as_hex("url"));
}

#[test]
fn hash_string_as_hex_length() {
    // UUIDType is 32 bytes (blake3) -> 64 hex chars
    assert_eq!(hash_string_as_hex("test").len(), 64);
}

// ── get_provider_id ──────────────────────────────────────────────

#[test]
fn get_provider_id_parses_numeric_string() {
    assert_eq!(get_provider_id("12345", "http://x.com/other.ts"), Some(12345));
}

#[test]
fn get_provider_id_falls_back_to_url() {
    assert_eq!(get_provider_id("not_a_number", "http://x.com/live/999.ts"), Some(999));
}

#[test]
fn get_provider_id_none_when_both_fail() {
    assert_eq!(get_provider_id("abc", "http://x.com/live/hello.ts"), None);
}

#[test]
fn get_provider_id_empty_string_falls_back() {
    assert_eq!(get_provider_id("", "http://x.com/42.ts"), Some(42));
}

#[test]
fn get_provider_id_prefers_parsed_string() {
    // provider_id string wins even if URL has a different numeric id
    assert_eq!(get_provider_id("100", "http://x.com/live/200.ts"), Some(100));
}

// ── provider/local playlist uuid ────────────────────────────────

#[test]
fn generate_provider_playlist_uuid_deterministic() {
    let a = generate_provider_playlist_uuid("k", "1", PlaylistItemType::Live);
    let b = generate_provider_playlist_uuid("k", "1", PlaylistItemType::Live);
    assert_eq!(a, b);
}

#[test]
fn generate_provider_playlist_uuid_different_key() {
    let a = generate_provider_playlist_uuid("k1", "1", PlaylistItemType::Live);
    let b = generate_provider_playlist_uuid("k2", "1", PlaylistItemType::Live);
    assert_ne!(a, b);
}

#[test]
fn generate_local_playlist_uuid_uses_url_path() {
    let a = generate_local_playlist_uuid("k", PlaylistItemType::LocalSeries, "http://x.com/path/to/stream");
    let b = generate_local_playlist_uuid("k", PlaylistItemType::LocalSeries, "http://y.com/path/to/stream");
    assert_eq!(a, b);
}

#[test]
fn generate_local_playlist_uuid_is_deterministic() {
    let a = generate_local_playlist_uuid("k", PlaylistItemType::LocalSeries, "http://x.com/stream");
    let b = generate_local_playlist_uuid("k", PlaylistItemType::LocalSeries, "http://x.com/stream");
    assert_eq!(a, b);
}

#[test]
fn generate_provider_playlist_uuid_different_type() {
    let a = generate_provider_playlist_uuid("k", "1", PlaylistItemType::Live);
    let b = generate_provider_playlist_uuid("k", "1", PlaylistItemType::Video);
    assert_ne!(a, b);
}

#[test]
fn generate_runtime_playlist_uuid_dispatches_by_item_type() {
    let provider = generate_runtime_playlist_uuid("input", "100", PlaylistItemType::Series, "file:///ignored.mkv");
    let local = generate_runtime_playlist_uuid("input", "100", PlaylistItemType::LocalSeries, "file:///ignored.mkv");

    assert_ne!(provider, local);
}

// ── u32_to_base64 / base64_to_u32 ───────────────────────────────

#[test]
fn base64_roundtrip_zero() {
    assert_eq!(base64_to_u32(&u32_to_base64(0)), Some(0));
}

#[test]
fn base64_roundtrip_max() {
    assert_eq!(base64_to_u32(&u32_to_base64(u32::MAX)), Some(u32::MAX));
}

#[test]
fn base64_roundtrip_arbitrary() {
    assert_eq!(base64_to_u32(&u32_to_base64(950327)), Some(950327));
}

#[test]
fn base64_to_u32_invalid_input() {
    assert_eq!(base64_to_u32("!!!"), None);
}

#[test]
fn base64_to_u32_wrong_length() {
    assert_eq!(base64_to_u32("AAAAAAA"), None);
}

#[test]
fn base64_to_u32_empty() {
    assert_eq!(base64_to_u32(""), None);
}

// ── parse_uuid_hex ───────────────────────────────────────────────

#[test]
fn parse_uuid_hex_valid() {
    let result = parse_uuid_hex("550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(
        result,
        Some([0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00, 0x00])
    );
}

#[test]
fn parse_uuid_hex_wrong_length() {
    assert_eq!(parse_uuid_hex("550e8400-e29b-41d4-a716"), None);
}

#[test]
fn parse_uuid_hex_no_hyphens_wrong_format() {
    // 32 hex chars without hyphens -> length 32 ≠ 36
    assert_eq!(parse_uuid_hex("550e8400e29b41d4a716446655440000"), None);
}

#[test]
fn parse_uuid_hex_invalid_hex_char() {
    assert_eq!(parse_uuid_hex("ZZZZZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZZZZZZZZZ"), None);
}

#[test]
fn parse_uuid_hex_empty() {
    assert_eq!(parse_uuid_hex(""), None);
}

// ── create_alias_uuid ────────────────────────────────────────────

#[test]
fn create_alias_uuid_deterministic() {
    let base = hash_string("base");
    let a = create_alias_uuid(&base, "mapping1");
    let b = create_alias_uuid(&base, "mapping1");
    assert_eq!(a, b);
}

#[test]
fn create_alias_uuid_different_mapping() {
    let base = hash_string("base");
    let a = create_alias_uuid(&base, "mapping1");
    let b = create_alias_uuid(&base, "mapping2");
    assert_ne!(a, b);
}

#[test]
fn create_alias_uuid_different_base() {
    let a = create_alias_uuid(&hash_string("base1"), "m");
    let b = create_alias_uuid(&hash_string("base2"), "m");
    assert_ne!(a, b);
}

#[test]
fn create_alias_uuid_differs_from_base() {
    let base = hash_string("base");
    let alias = create_alias_uuid(&base, "mapping");
    assert_ne!(base, alias);
}

/// Swapping `to_string()` for `as_str()` in the UUID hashers must not change a
/// single byte — every persisted playlist UUID depends on it. Guards against a
/// future `Display` impl that stops delegating to `as_str()`.
#[test]
fn item_type_label_is_hash_stable_against_display() {
    for item_type in [
        PlaylistItemType::Live,
        PlaylistItemType::LiveHls,
        PlaylistItemType::Video,
        PlaylistItemType::LocalVideo,
        PlaylistItemType::LocalSeries,
        PlaylistItemType::LocalSeriesInfo,
        PlaylistItemType::Series,
        PlaylistItemType::SeriesInfo,
        PlaylistItemType::Catchup,
    ] {
        assert_eq!(item_type.as_str(), item_type.to_string(), "{item_type:?}");

        let mut expected = blake3::Hasher::new();
        expected.update(b"key");
        expected.update(b"provider");
        expected.update(item_type.to_string().as_bytes());
        assert_eq!(
            generate_provider_playlist_uuid("key", "provider", item_type),
            UUIDType(expected.finalize().into()),
            "{item_type:?}"
        );
    }
}

#[test]
fn fnv1a_32_is_deterministic_and_nonzero() {
    let a = fnv1a_32("input_a:Show Name:https://example.test/series/ep1");
    let b = fnv1a_32("input_a:Show Name:https://example.test/series/ep1");
    assert_eq!(a, b);
    assert!(fnv1a_32("") > 0);
    assert!(fnv1a_32("0") > 0);
    assert_ne!(a, fnv1a_32("input_a:Show Name:https://example.test/series/ep2"));
}

#[test]
fn fnv1a_32_matches_known_vector() {
    // FNV-1a 32-bit of "" is 0x811c9dc5 (offset basis). Our implementation
    // forces the result to `>= 1` so the sentinel-zero guard kicks in.
    assert_eq!(fnv1a_32(""), 0x811c9dc5);
}

#[test]
fn fnv1a_32_parts_matches_joined_string() {
    let joined = fnv1a_32("a:b:c");
    let parts = fnv1a_32_parts(&["a", "b", "c"]);
    assert_eq!(joined, parts);
    // Empty parts list still produces the offset-basis sentinel.
    assert_eq!(fnv1a_32_parts(&[]), fnv1a_32(""));
    // Single part produces no separator byte.
    assert_eq!(fnv1a_32_parts(&["only"]), fnv1a_32("only"));
}
