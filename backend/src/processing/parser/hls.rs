use crate::model::ProxyUserCredentials;
use shared::concat_string;
use shared::{
    utils::{deobfuscate_text, extract_extension_from_url, obfuscate_text, CONSTANTS},
    defaults::{HLS_EXT, HLS_PREFIX}
};
use std::borrow::Cow;
use std::str;
use url::Url;

pub mod origin_manifest;
pub mod initial_strip;
pub mod transient_manifest;

const TOKEN_SEPARATOR: char = '\x1F';
const TOKEN_SEPARATOR_STR: &str = "\x1F";

fn create_hls_session_token_and_url(secret: &[u8], session_token: &str, stream_url: &str) -> String {
    let cookie_value = obfuscate_text(secret, &concat_string!(session_token, TOKEN_SEPARATOR_STR, stream_url));
    if let Some(ext) = extract_extension_from_url(stream_url) {
        return concat_string!(&cookie_value, ext);
    }
    cookie_value
}

fn create_hls_url_without_session_token(secret: &[u8], stream_url: &str) -> String {
    let token = obfuscate_text(secret, stream_url);
    if let Some(ext) = extract_extension_from_url(stream_url) {
        return concat_string!(&token, ext);
    }
    token
}

fn remove_any_ext(s: &str) -> &str {
    match s.rsplit_once('.') {
        Some((base, _)) => base,
        None => s,
    }
}
pub fn get_hls_session_token_and_url_from_token(secret: &[u8], token: &str) -> Option<(Option<String>, String)> {
    if let Ok(decrypted) = deobfuscate_text(secret, remove_any_ext(token)) {
        let parts: Vec<&str> = decrypted.split(TOKEN_SEPARATOR).collect();
        if parts.len() == 2 {
            let session_token: String = parts[0].to_string();
            let stream_url: String = parts[1].to_string();
            return Some((Some(session_token), stream_url));
        }
        if parts.len() == 1 {
            return Some((None, decrypted));
        }
    }
    None
}

pub struct RewriteHlsProps<'a> {
    pub secret: &'a [u8; 16],
    pub base_url: &'a str,
    pub content: &'a str,
    pub hls_url: String,
    pub target_id: u16,
    pub virtual_id: u32,
    pub input_id: u16,
    pub user_token: Option<&'a str>,
}

fn is_direct_archive_start_query_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("utc") || key.eq_ignore_ascii_case("utcstart")
}

fn is_contextual_archive_start_query_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("start") || key.eq_ignore_ascii_case("timestamp")
}

fn is_archive_start_context_query_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("end")
        || key.eq_ignore_ascii_case("duration")
        || key.eq_ignore_ascii_case("lutc")
        || key.eq_ignore_ascii_case("offset")
}

fn archive_start_query(url: &Url) -> Option<(Cow<'_, str>, Cow<'_, str>)> {
    let has_context = url.query_pairs().any(|(key, _)| is_archive_start_context_query_key(&key));
    url.query_pairs().find(|(key, _)| {
        is_direct_archive_start_query_key(key) || (has_context && is_contextual_archive_start_query_key(key))
    })
}

fn preserve_archive_start_query(base: &Url, mut target: Url) -> Url {
    if archive_start_query(&target).is_some() {
        return target;
    }
    if let Some((key, value)) = archive_start_query(base) {
        target.query_pairs_mut().append_pair(&key, &value);
    }
    target
}

fn has_same_origin(left: &Url, right: &Url) -> bool {
    matches!(left.scheme(), "ftp" | "http" | "https" | "ws" | "wss")
        && left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}

/// Rewrites an HLS URI relative to a base playlist URL.
/// Absolute URIs are returned unchanged.
pub fn rewrite_hls_url<'a>(base: &'a str, reference: &'a str) -> Cow<'a, str> {
    if Url::parse(reference).is_ok() {
        return Cow::Borrowed(reference);
    }

    let Ok(base_url) = Url::parse(base) else {
        return Cow::Borrowed(reference);
    };

    base_url.join(reference).map_or_else(
        |_| Cow::Borrowed(reference),
        |target| {
            let is_same_origin_child_playlist = has_same_origin(&target, &base_url)
                && extract_extension_from_url(target.as_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case(HLS_EXT));
            Cow::Owned(if is_same_origin_child_playlist {
                preserve_archive_start_query(&base_url, target).into()
            } else {
                target.into()
            })
        },
    )
}

fn rewrite_uri_attrib<'a>(line: &'a str, props: &RewriteHlsProps, user: &ProxyUserCredentials) -> Cow<'a, str> {
    let Some(caps) = CONSTANTS.re_hls_uri.captures(line) else {
        return Cow::Borrowed(line);
    };

    let uri = &caps[1];
    let rewritten = rewrite_hls_url(&props.hls_url, uri);

    let token = if let Some(user_token) = &props.user_token {
        create_hls_session_token_and_url(props.secret, user_token, &rewritten)
    } else {
        create_hls_url_without_session_token(props.secret, &rewritten)
    };

        let final_uri = format!(
        "{}/{HLS_PREFIX}/{}/{}/{}/{}/{}/{}",
        props.base_url,
        user.username,
        user.password,
        props.target_id,
        props.input_id,
        props.virtual_id,
        token
    );

    Cow::Owned(CONSTANTS
        .re_hls_uri
        .replace(line, format!(r#"URI="{final_uri}""#))
        .to_string())
}

pub fn rewrite_hls(user: &ProxyUserCredentials, props: &RewriteHlsProps) -> String {
    let username = &user.username;
    let password = &user.password;
    let mut result = Vec::new();
    for line in props.content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // skip comments
        if line.starts_with('#') {
            let rewritten = rewrite_uri_attrib(line, props, user);
            result.push(rewritten.to_string());
            continue;
        }

        // target url
        let target_url = rewrite_hls_url(&props.hls_url, line);
        let token = if let Some(user_token) = &props.user_token {
            create_hls_session_token_and_url(props.secret, user_token, &target_url)
        } else {
            create_hls_url_without_session_token(props.secret, &target_url)
        };
        let url = format!(
            "{}/{HLS_PREFIX}/{}/{}/{}/{}/{}/{}",
            props.base_url,
            username,
            password,
            props.target_id,
            props.input_id,
            props.virtual_id,
            token
        );
        result.push(url);
    }
    result.push("\r\n".to_string());
    result.join("\r\n")
}

#[cfg(test)]
mod test {
    use crate::model::ProxyUserCredentials;
    use crate::processing::parser::hls::{
        get_hls_session_token_and_url_from_token, rewrite_hls, rewrite_hls_url, RewriteHlsProps,
    };
    use rand::RngCore;
    use shared::utils::{u32_to_base64};
    use shared::defaults::{HLS_PREFIX};

    #[test]
    fn test_token_size() {
        for _i in 0..10_000 {
            let session_token = rand::rng().next_u32();
            assert_eq!(u32_to_base64(session_token).len(), 6);
        }
    }

    #[test]
    fn rewrite_http_relative_segment() {
        let base = "http://example.com/hls/playlist.m3u8";
        let uri = "seg001.ts";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(out, "http://example.com/hls/seg001.ts");
    }

    #[test]
    fn rewrite_http_root_relative_segment() {
        let base = "http://example.com/hls/playlist.m3u8";
        let uri = "/media/seg001.ts";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(out, "http://example.com/media/seg001.ts");
    }

    #[test]
    fn rewrite_http_parent_directory() {
        let base = "http://example.com/hls/level1/playlist.m3u8";
        let uri = "../seg001.ts";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(out, "http://example.com/hls/seg001.ts");
    }

    #[test]
    fn rewrite_relative_variant_preserves_archive_start_query() {
        let base = "https://cdn.example/hls/channel/index.m3u8?offset=-10752&utcstart=1785072000&useseq=t";
        let uri = "variant/playlist.m3u8?offset=-10752&useseq=t";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(
            out,
            "https://cdn.example/hls/channel/variant/playlist.m3u8?offset=-10752&useseq=t&utcstart=1785072000"
        );
    }

    #[test]
    fn rewrite_keeps_child_archive_start_query() {
        let base = "https://cdn.example/hls/channel/index.m3u8?utcstart=1785072000";
        let uri = "variant/playlist.m3u8?utc=1785071000";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(out, "https://cdn.example/hls/channel/variant/playlist.m3u8?utc=1785071000");
    }

    #[test]
    fn rewrite_does_not_propagate_plain_start_query() {
        let base = "https://cdn.example/hls/channel/index.m3u8?start=1785072000";
        let uri = "variant/playlist.m3u8";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(out, "https://cdn.example/hls/channel/variant/playlist.m3u8");
    }

    #[test]
    fn rewrite_archive_playlist_does_not_modify_signed_media_urls() {
        let base = "https://cdn.example/hls/channel/index.m3u8?utcstart=1785072000&offset=-3600";

        assert_eq!(
            rewrite_hls_url(base, "segment.ts?sig=abc"),
            "https://cdn.example/hls/channel/segment.ts?sig=abc"
        );
        assert_eq!(
            rewrite_hls_url(base, "key.bin?sig=def"),
            "https://cdn.example/hls/channel/key.bin?sig=def"
        );
        assert_eq!(
            rewrite_hls_url(base, "init.mp4?sig=ghi"),
            "https://cdn.example/hls/channel/init.mp4?sig=ghi"
        );
    }

    #[test]
    fn rewrite_absolute_same_origin_url_remains_exact_passthrough() {
        let base = "https://cdn.example/hls/channel/index.m3u8?utcstart=1785072000&offset=-3600";
        let reference = "https://cdn.example/hls/channel/segment.ts?sig=a%2Fb&token=x+y";

        assert!(matches!(rewrite_hls_url(base, reference), std::borrow::Cow::Borrowed(_)));
        assert_eq!(rewrite_hls_url(base, reference), reference);
    }

    #[test]
    fn rewrite_https_absolute_passthrough() {
        let base = "http://example.com/hls/playlist.m3u8";
        let uri = "https://cdn.example.org/video/seg.ts";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(out, uri);
    }

    #[test]
    fn rewrite_file_relative_segment() {
        let base = "file:///mnt/media/hls/playlist.m3u8";
        let uri = "seg001.ts";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(out, "file:///mnt/media/hls/seg001.ts");
    }

    #[test]
    fn rewrite_file_parent_directory() {
        let base = "file:///mnt/media/hls/level1/playlist.m3u8";
        let uri = "../seg001.ts";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(out, "file:///mnt/media/hls/seg001.ts");
    }

    #[test]
    fn rewrite_file_child_playlist_does_not_inherit_archive_query() {
        let base = "file:///mnt/media/hls/playlist.m3u8?utc=1785072000";

        assert_eq!(rewrite_hls_url(base, "child.m3u8"), "file:///mnt/media/hls/child.m3u8");
    }

    #[test]
    fn rewrite_file_absolute_passthrough() {
        let base = "file:///mnt/media/hls/playlist.m3u8";
        let uri = "file:///mnt/other/seg.ts";

        let out = rewrite_hls_url(base, uri);
        assert_eq!(out, uri);
    }

    #[test]
    fn rewrite_hls_fragment() {
        let base = "http://example.com/hls/playlist.m3u8";
        let fragment = "seg.ts#t=10";

        let out = rewrite_hls_url(base, fragment);
        assert_eq!(out, "http://example.com/hls/seg.ts#t=10");
    }

    #[test]
    fn rewrite_hls_without_user_token_keeps_segment_urls() {
        let mut user = ProxyUserCredentials::default();
        user.username = "u".to_string();
        user.password = "p".to_string();
        let secret = [7u8; 16];
        let props = RewriteHlsProps {
            secret: &secret,
            base_url: "http://proxy",
            content: "#EXTM3U\nsegment.ts",
            hls_url: "http://origin/live/main.m3u8".to_string(),
            target_id: 9,
            virtual_id: 101,
            input_id: 11,
            user_token: None,
        };

        let rewritten = rewrite_hls(&user, &props);
        let segment_line = rewritten
            .lines()
            .find(|line| line.contains(&format!("/{HLS_PREFIX}/")))
            .expect("rewritten playlist should contain a segment URL");
        assert!(segment_line.contains("/hls/u/p/9/11/101/"));
        let token = segment_line
            .rsplit('/')
            .next()
            .expect("rewritten hls segment URL should include token");
        let decoded = get_hls_session_token_and_url_from_token(&secret, token)
            .expect("rewritten hls token should decode");

        assert!(decoded.0.is_none());
        assert_eq!(decoded.1, "http://origin/live/segment.ts");
    }
}
