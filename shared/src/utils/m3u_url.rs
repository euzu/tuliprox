use url::Url;

/// Returns whether a query key commonly carries account-specific credentials.
pub fn is_account_query_key(key: &str) -> bool {
    const ACCOUNT_QUERY_KEYS: &[&str] = &[
        "token",
        "access_token",
        "auth_token",
        "key",
        "apikey",
        "api_key",
        "accesskey",
        "access_key",
        "auth",
        "authorization",
        "user",
        "username",
        "usr",
        "login",
        "pass",
        "password",
        "pwd",
        "session",
        "session_id",
        "device_key",
        "mac",
        "sig",
        "signature",
    ];

    let ends_with_ignore_ascii_case = |suffix: &str| {
        key.get(key.len().saturating_sub(suffix.len())..).is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
    };

    ACCOUNT_QUERY_KEYS.iter().any(|candidate| key.eq_ignore_ascii_case(candidate))
        || ends_with_ignore_ascii_case("_token")
        || ends_with_ignore_ascii_case("_key")
}

/// Live playlist file names a Flussonic channel directory can expose next to its archive files.
pub const FLUSSONIC_LIVE_FILES: [&str; 7] =
    ["index.m3u8", "video.m3u8", "mono.m3u8", "mpegts", "index.ts", "video.ts", "mono.ts"];

/// Builds the account-independent identity used to correlate equivalent M3U stream URLs.
///
/// Account credentials are removed from the URL authority and query while all other URL
/// components remain part of the identity. Query pairs are sorted so provider accounts may
/// return the same channel parameters in a different order. The path stays exact, so
/// case-sensitive provider paths remain distinct.
pub fn m3u_stream_url_identity(stream_url: &str) -> Option<String> {
    account_independent_identity(Url::parse(stream_url).ok()?)
}

/// Prefix of the case-folded Flussonic live file keys, disjoint from URL identities.
const FLUSSONIC_LIVE_FILE_KEY_PREFIX: &str = "flussonic-live|";

/// Additional index key of a Flussonic live file whose name is not the canonical lowercase one.
///
/// Stored next to the exact identity, so a Flussonic archive sibling lookup probing
/// `mono.m3u8` still finds an alias entry stored as `MONO.M3U8`. Exact lookups never use it.
pub fn m3u_flussonic_live_file_index_key(stream_url: &str) -> Option<String> {
    let (url, canonical) = flussonic_live_file_url(stream_url)?;
    (!canonical).then(|| flussonic_live_file_key(url)).flatten()
}

/// Case-folded key a Flussonic archive sibling candidate is looked up under after its exact
/// identity; see [`m3u_flussonic_live_file_index_key`].
pub fn m3u_flussonic_live_file_lookup_key(stream_url: &str) -> Option<String> {
    flussonic_live_file_key(flussonic_live_file_url(stream_url)?.0)
}

/// Parses a URL whose file name is a Flussonic live file in any case, returning it with the
/// canonical lowercase name and whether the original name was already canonical.
fn flussonic_live_file_url(stream_url: &str) -> Option<(Url, bool)> {
    let mut url = Url::parse(stream_url).ok()?;
    let file = url.path_segments()?.next_back()?;
    let live_file = FLUSSONIC_LIVE_FILES.iter().find(|live_file| live_file.eq_ignore_ascii_case(file))?;
    let canonical = *live_file == file;
    if !canonical {
        url.path_segments_mut().ok()?.pop().push(live_file);
    }
    Some((url, canonical))
}

fn flussonic_live_file_key(url: Url) -> Option<String> {
    account_independent_identity(url).map(|identity| format!("{FLUSSONIC_LIVE_FILE_KEY_PREFIX}{identity}"))
}

fn account_independent_identity(mut url: Url) -> Option<String> {
    if url.cannot_be_a_base() || url.host_str().is_none() {
        return None;
    }

    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_fragment(None);

    let mut query_pairs: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| !is_account_query_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    query_pairs.sort_unstable();

    url.set_query(None);
    if !query_pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())));
    }

    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_keeps_path_case_and_flussonic_key_folds_only_live_files() {
        assert_ne!(
            m3u_stream_url_identity("http://stream.example:4000/323/MONO.M3U8?token=a"),
            m3u_stream_url_identity("http://stream.example:4000/323/mono.m3u8?token=b")
        );
        assert_eq!(
            m3u_flussonic_live_file_index_key("http://stream.example:4000/323/MONO.M3U8?token=a").as_deref(),
            Some("flussonic-live|http://stream.example:4000/323/mono.m3u8")
        );
        assert_eq!(
            m3u_flussonic_live_file_lookup_key("http://stream.example:4000/323/mono.m3u8?token=b"),
            m3u_flussonic_live_file_index_key("http://stream.example:4000/323/MONO.M3U8?token=a")
        );
        // Canonical names need no extra index entry, other file names never get one.
        assert_eq!(m3u_flussonic_live_file_index_key("http://stream.example:4000/323/mono.m3u8?token=a"), None);
        assert_eq!(m3u_flussonic_live_file_index_key("http://stream.example:4000/323/Playlist.M3U8"), None);
        assert_eq!(m3u_flussonic_live_file_lookup_key("http://stream.example:4000/323/Playlist.m3u8"), None);
    }

    #[test]
    fn identity_ignores_account_tokens_but_preserves_channel_parameters() {
        let primary = m3u_stream_url_identity(
            "http://stream.example:4000/323/mono.m3u8?token=primary-stream-token&quality=hd&lang=en",
        );
        let alias = m3u_stream_url_identity(
            "http://stream.example:4000/323/mono.m3u8?lang=en&access_key=alias-stream-token&quality=hd",
        );

        assert_eq!(primary, alias);
        assert_eq!(primary.as_deref(), Some("http://stream.example:4000/323/mono.m3u8?lang=en&quality=hd"));
    }

    #[test]
    fn identity_keeps_authority_and_path_distinctions() {
        let first = m3u_stream_url_identity("http://stream.example:4000/323/mono.m3u8?token=one");
        let other_port = m3u_stream_url_identity("http://stream.example:5000/323/mono.m3u8?token=two");
        let other_path = m3u_stream_url_identity("http://stream.example:4000/324/mono.m3u8?token=two");

        assert_ne!(first, other_port);
        assert_ne!(first, other_path);
    }

    #[test]
    fn account_query_key_matching_is_case_insensitive_and_utf8_safe() {
        assert!(is_account_query_key("UserName"));
        assert!(is_account_query_key("PASSWORD"));
        assert!(is_account_query_key("provider_ToKeN"));
        assert!(is_account_query_key("device_KEY"));
        assert!(is_account_query_key("ä_key"));
        assert!(!is_account_query_key("€AB"));
        assert!(!is_account_query_key("monokey"));
    }

    #[test]
    fn identity_ignores_query_username_and_password() {
        let primary = m3u_stream_url_identity(
            "http://stream.example/323/mono.m3u8?username=primary-user&password=primary-pass&quality=hd",
        );
        let alias = m3u_stream_url_identity(
            "http://stream.example/323/mono.m3u8?PASSWORD=alias-pass&quality=hd&UserName=alias-user",
        );

        assert_eq!(primary, alias);
        assert_eq!(primary.as_deref(), Some("http://stream.example/323/mono.m3u8?quality=hd"));
    }
}
