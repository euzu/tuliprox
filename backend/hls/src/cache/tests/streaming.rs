use super::*;

#[test]
fn segment_cache_key_contains_no_origin_data() {
    let key = cache_key();

    assert_eq!(key.stable_value(), "hls:proxy_session:00000000000000000123");
}
