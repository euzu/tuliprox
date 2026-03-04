use crate::model::ProxyUserCredentials;
use shared::concat_string;
use shared::utils::{deobfuscate_text, extract_extension_from_url, obfuscate_text, CONSTANTS, HLS_PREFIX};
use std::borrow::Cow;
use std::str;
use url::Url;

const TOKEN_SEPARATOR: char = '\x1F';
const TOKEN_SEPARATOR_STR: &str = "\x1F";

fn create_hls_session_token_and_url(secret: &[u8], session_token: &str, stream_url: &str) -> String {
    let cookie_value = obfuscate_text(secret, &concat_string!(session_token, TOKEN_SEPARATOR_STR, stream_url));
    if let Some(ext) = extract_extension_from_url(stream_url) {
        return concat_string!(&cookie_value, &ext);
    }
    cookie_value
}

fn create_hls_url_without_session_token(secret: &[u8], stream_url: &str) -> String {
    let token = obfuscate_text(secret, stream_url);
    if let Some(ext) = extract_extension_from_url(stream_url) {
        return concat_string!(&token, &ext);
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
    pub virtual_id: u32,
    pub input_id: u16,
    pub user_token: Option<&'a str>,
}

/// Rewrites an HLS URI relative to a base playlist URL.
/// Absolute URIs are returned unchanged.
pub fn rewrite_hls_url<'a>(base: &'a str, reference: &'a str) -> Cow<'a, str> {
    // absolute URI → passthrough
    if Url::parse(reference).is_ok() {
        return Cow::Borrowed(reference);
    }

    let Ok(base_url) = Url::parse(base) else {
        return Cow::Borrowed(reference);
    };

    base_url.join(reference).map_or_else(|_| Cow::Borrowed(reference), |u| Cow::Owned(u.to_string()))
}

fn rewrite_uri_attrib<'a>(line: &'a str, props: &RewriteHlsProps) -> Cow<'a, str> {
    let Some(caps) = CONSTANTS.re_hls_uri.captures(line) else {
        return Cow::Borrowed(line);
    };

    let uri = &caps[1];
    let rewritten = rewrite_hls_url(&props.hls_url, uri);

    let final_uri = if let Some(user_token) = &props.user_token {
        Cow::Owned(create_hls_session_token_and_url(
            props.secret,
            user_token,
            &rewritten,
        ))
    } else {
        rewritten
    };

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
            let rewritten = rewrite_uri_attrib(line, props);
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
            "{}/{HLS_PREFIX}/{}/{}/{}/{}/{}",
            props.base_url,
            username,
            password,
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
    use shared::utils::{u32_to_base64, HLS_PREFIX};

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
            virtual_id: 101,
            input_id: 11,
            user_token: None,
        };

        let rewritten = rewrite_hls(&user, &props);
        let segment_line = rewritten
            .lines()
            .find(|line| line.contains(&format!("/{HLS_PREFIX}/")))
            .expect("rewritten playlist should contain a segment URL");
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
