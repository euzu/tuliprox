use shared::{
    concat_string,
    defaults::{HLS_EXT, HLS_PREFIX},
    utils::{extract_extension_from_url, is_hls_url, open_hls_resource_url, seal_hls_resource_url, CONSTANTS},
};
use std::{borrow::Cow, str};
use tuliprox_core::model::ProxyUserCredentials;
use url::Url;

pub mod origin_manifest;

const TOKEN_SEPARATOR: char = '\x1F';
const TOKEN_SEPARATOR_STR: &str = "\x1F";

/// Explicit resource kind sealed into an HLS token, so the token route does not have to guess
/// manifest vs. media from the URL extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsResourceKind {
    Manifest(HlsManifestSource),
    Media,
}

/// Where a sealed manifest URL comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestSource {
    /// Provider entry URL; the alias rewrite can move it to another provider account.
    Entry,
    /// Child URL resolved from an upstream master, possibly on a CDN host.
    Child,
}

impl HlsResourceKind {
    const fn token_field(self) -> &'static str {
        match self {
            Self::Manifest(HlsManifestSource::Entry) => "me",
            Self::Manifest(HlsManifestSource::Child) => "mc",
            Self::Media => "m",
        }
    }

    fn from_token_field(value: &str) -> Option<Self> {
        match value {
            "me" => Some(Self::Manifest(HlsManifestSource::Entry)),
            "mc" => Some(Self::Manifest(HlsManifestSource::Child)),
            "m" => Some(Self::Media),
            _ => None,
        }
    }

    pub const fn is_manifest(self) -> bool { matches!(self, Self::Manifest(_)) }
}

/// Decoded content of a sealed HLS resource token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsResourceToken {
    pub session_token: Option<String>,
    pub url: String,
    /// `None` for legacy tokens and for references from unknown tags.
    pub kind: Option<HlsResourceKind>,
    /// Provider (account) whose credentials or resolution produced `url`.
    pub origin_provider: Option<String>,
}

impl HlsResourceToken {
    /// Manifest vs. media decision for the token route; falls back to the URL heuristic.
    pub fn is_manifest(&self) -> bool { self.kind.map_or_else(|| is_hls_url(&self.url), HlsResourceKind::is_manifest) }
}

/// Seals an HLS resource token and appends the path extension for the emitted URI.
/// Manifest tokens always end in `.m3u8`, so strict players treat them as playlists.
pub fn create_hls_resource_token(
    secret: &[u8; 16],
    session_token: Option<&str>,
    stream_url: &str,
    kind: Option<HlsResourceKind>,
    origin_provider: Option<&str>,
) -> String {
    let payload = concat_string!(
        session_token.unwrap_or_default(),
        TOKEN_SEPARATOR_STR,
        stream_url,
        TOKEN_SEPARATOR_STR,
        kind.map_or("", HlsResourceKind::token_field),
        TOKEN_SEPARATOR_STR,
        origin_provider.unwrap_or_default()
    );
    let token = seal_hls_resource_url(secret, &payload);
    let ext = if kind.is_some_and(HlsResourceKind::is_manifest) {
        Some(HLS_EXT)
    } else {
        extract_extension_from_url(stream_url)
    };
    match ext {
        Some(ext) => concat_string!(&token, ext),
        None => token,
    }
}

fn remove_any_ext(s: &str) -> &str {
    match s.rsplit_once('.') {
        Some((base, _)) => base,
        None => s,
    }
}

pub fn get_hls_session_token_and_url_from_token(secret: &[u8; 16], token: &str) -> Option<HlsResourceToken> {
    let decrypted = open_hls_resource_url(secret, remove_any_ext(token)).ok()?;
    let parts: Vec<&str> = decrypted.split(TOKEN_SEPARATOR).collect();
    match parts.as_slice() {
        [session, url, kind, origin] => Some(HlsResourceToken {
            session_token: (!session.is_empty()).then(|| (*session).to_string()),
            url: (*url).to_string(),
            kind: HlsResourceKind::from_token_field(kind),
            origin_provider: (!origin.is_empty()).then(|| (*origin).to_string()),
        }),
        [session, url] => Some(HlsResourceToken {
            session_token: Some((*session).to_string()),
            url: (*url).to_string(),
            kind: None,
            origin_provider: None,
        }),
        [url] => {
            Some(HlsResourceToken { session_token: None, url: (*url).to_string(), kind: None, origin_provider: None })
        }
        _ => None,
    }
}

/// Builds the proxied `/hls/...` URI for a sealed resource token.
pub fn build_hls_resource_uri(
    base_url: &str,
    user: &ProxyUserCredentials,
    target_id: u16,
    input_id: u16,
    virtual_id: u32,
    token: &str,
) -> String {
    format!("{base_url}/{HLS_PREFIX}/{}/{}/{target_id}/{input_id}/{virtual_id}/{token}", user.username, user.password)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsPlaylistKind {
    Master,
    Media,
    Invalid,
}

/// Strips a leading UTF-8 BOM; classification and rewriting must see the same input.
pub fn normalize_hls_playlist(content: &str) -> &str { content.strip_prefix('\u{FEFF}').unwrap_or(content) }

/// Classifies playlist content line by line. Tags are matched with `starts_with` on trimmed
/// lines only, so tag names inside titles never change the kind.
pub fn classify_hls_playlist(content: &str) -> HlsPlaylistKind {
    let mut lines = normalize_hls_playlist(content).lines().map(str::trim).filter(|line| !line.is_empty());
    if !lines.next().is_some_and(|line| line.starts_with("#EXTM3U")) {
        return HlsPlaylistKind::Invalid;
    }
    let mut is_media = false;
    for line in lines {
        // A URI line that references another playlist makes this a master, even without
        // `#EXT-X-STREAM-INF` (some providers list chunklists under `#EXTINF`).
        if line.starts_with("#EXT-X-STREAM-INF")
            || line.starts_with("#EXT-X-I-FRAME-STREAM-INF")
            || (!line.starts_with('#') && is_hls_url(line))
        {
            return HlsPlaylistKind::Master;
        }
        is_media |= line.starts_with("#EXT-X-TARGETDURATION") || line.starts_with("#EXTINF");
    }
    if is_media {
        HlsPlaylistKind::Media
    } else {
        HlsPlaylistKind::Invalid
    }
}

fn tag_uri_resource_kind(playlist_kind: HlsPlaylistKind, line: &str) -> Option<HlsResourceKind> {
    let tag = line.split_once(':').map_or(line, |(tag, _)| tag);
    match (playlist_kind, tag) {
        (HlsPlaylistKind::Master, "#EXT-X-MEDIA" | "#EXT-X-I-FRAME-STREAM-INF") => {
            Some(HlsResourceKind::Manifest(HlsManifestSource::Child))
        }
        (HlsPlaylistKind::Master, "#EXT-X-SESSION-KEY")
        | (HlsPlaylistKind::Media, "#EXT-X-MAP" | "#EXT-X-KEY" | "#EXT-X-PART" | "#EXT-X-PRELOAD-HINT") => {
            Some(HlsResourceKind::Media)
        }
        _ => None,
    }
}

const fn uri_line_resource_kind(playlist_kind: HlsPlaylistKind) -> Option<HlsResourceKind> {
    match playlist_kind {
        HlsPlaylistKind::Master => Some(HlsResourceKind::Manifest(HlsManifestSource::Child)),
        HlsPlaylistKind::Media => Some(HlsResourceKind::Media),
        HlsPlaylistKind::Invalid => None,
    }
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
    /// Provider (account) that served the playlist being rewritten.
    pub origin_provider: Option<&'a str>,
    /// Kind of `content` when the caller has classified it already; classified here otherwise.
    pub playlist_kind: Option<HlsPlaylistKind>,
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

fn has_archive_start_query(url: &Url) -> bool {
    let has_context = url.query_pairs().any(|(key, _)| is_archive_start_context_query_key(&key));
    url.query_pairs().any(|(key, _)| {
        is_direct_archive_start_query_key(&key) || (has_context && is_contextual_archive_start_query_key(&key))
    })
}

fn preserve_archive_start_query(base: &Url, mut target: Url) -> Url {
    let has_context = base.query_pairs().any(|(key, _)| is_archive_start_context_query_key(&key));
    if !has_archive_start_query(base) {
        return target;
    }

    for (key, value) in base.query_pairs() {
        let is_start =
            is_direct_archive_start_query_key(&key) || (has_context && is_contextual_archive_start_query_key(&key));
        let already_present = if is_start {
            has_archive_start_query(&target)
        } else {
            target.query_pairs().any(|(target_key, _)| target_key.eq_ignore_ascii_case(&key))
        };
        if (is_start || is_archive_start_context_query_key(&key)) && !already_present {
            target.query_pairs_mut().append_pair(&key, &value);
        }
    }
    target
}

fn is_bounded_flussonic_hls_url(url: &Url) -> bool {
    url.path_segments().and_then(Iterator::last).is_some_and(|file| {
        matches!(crate::m3u_format::parse_flussonic_archive_file(file),
            Some(crate::m3u_format::FlussonicArchiveKind::Archive { duration, extension: ".m3u8", .. })
                if duration.parse::<u64>().is_ok_and(|duration| duration > 0))
    })
}

fn preserve_bounded_archive_query(base: &Url, mut target: Url) -> Url {
    // Child URIs can supply their own credentials; inherit only missing query keys.
    for (key, value) in base.query_pairs() {
        if !target.query_pairs().any(|(target_key, _)| target_key.eq_ignore_ascii_case(&key)) {
            target.query_pairs_mut().append_pair(&key, &value);
        }
    }
    target
}

fn preserve_token_query(base: &Url, mut target: Url) -> Url {
    let target_has_token = target.query_pairs().any(|(key, _)| key.eq_ignore_ascii_case("token"));
    if target_has_token {
        return target;
    }

    if let Some((key, value)) = base.query_pairs().find(|(key, _)| key.eq_ignore_ascii_case("token")) {
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
/// Bounded Flussonic archives inherit source queries for resources on the same origin.
/// Other absolute URIs are returned unchanged.
pub fn rewrite_hls_url<'a>(base: &'a str, reference: &'a str) -> Cow<'a, str> {
    rewrite_hls_url_with_archive_context(base, reference, false)
}

fn rewrite_hls_url_with_archive_context<'a>(base: &'a str, reference: &'a str, bounded_archive: bool) -> Cow<'a, str> {
    if let Ok(target) = Url::parse(reference) {
        if let Ok(base_url) = Url::parse(base) {
            if (bounded_archive || is_bounded_flussonic_hls_url(&base_url)) && has_same_origin(&base_url, &target) {
                return Cow::Owned(preserve_bounded_archive_query(&base_url, target).into());
            }
        }
        return Cow::Borrowed(reference);
    }

    let Ok(base_url) = Url::parse(base) else {
        return Cow::Borrowed(reference);
    };

    base_url.join(reference).map_or_else(
        |_| Cow::Borrowed(reference),
        |mut target| {
            let is_same_origin = has_same_origin(&target, &base_url);
            if is_same_origin {
                target = if bounded_archive || is_bounded_flussonic_hls_url(&base_url) {
                    preserve_bounded_archive_query(&base_url, target)
                } else {
                    preserve_token_query(&base_url, target)
                };
            }
            let is_same_origin_child_playlist = is_same_origin
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

fn bounded_archive_session_virtual_id(token: &str, username: &str) -> Option<u32> {
    let payload = token.strip_prefix("m3u-catchup|").or_else(|| token.strip_prefix("catchup|"))?;
    let (fingerprint, subject) = payload.split_once('|')?;
    // Match the authenticated username before parsing fields; usernames may contain separators.
    let archive = subject.strip_prefix(username)?.strip_prefix('|')?;
    let mut fields = archive.split('|');
    let virtual_id = fields.next()?.parse::<u32>().ok()?;
    if fingerprint.is_empty() || username.is_empty() || fields.next()? != "archive" {
        return None;
    }
    fields.next()?.parse::<u64>().ok()?;
    if fields.next()?.parse::<u64>().ok()? == 0 {
        return None;
    }
    match (fields.next(), fields.next()) {
        (None | Some(".m3u8" | ".ts"), None) => Some(virtual_id),
        _ => None,
    }
}

fn rewrite_hls_resource_url<'a>(props: &'a RewriteHlsProps, reference: &'a str, username: &str) -> Cow<'a, str> {
    // The range identity survives child playlists whose filenames no longer identify the archive.
    let bounded_archive = props.user_token.and_then(|token| bounded_archive_session_virtual_id(token, username))
        == Some(props.virtual_id);
    rewrite_hls_url_with_archive_context(&props.hls_url, reference, bounded_archive)
}

fn rewrite_uri_attrib<'a>(
    line: &'a str,
    props: &RewriteHlsProps,
    user: &ProxyUserCredentials,
    playlist_kind: HlsPlaylistKind,
) -> Cow<'a, str> {
    let Some(caps) = CONSTANTS.re_hls_uri.captures(line) else {
        return Cow::Borrowed(line);
    };

    let uri = &caps[1];
    let rewritten = rewrite_hls_resource_url(props, uri, &user.username);
    let final_uri = rewrite_resource_uri(props, user, &rewritten, tag_uri_resource_kind(playlist_kind, line));

    Cow::Owned(CONSTANTS.re_hls_uri.replace(line, format!(r#"URI="{final_uri}""#)).to_string())
}

fn rewrite_resource_uri(
    props: &RewriteHlsProps,
    user: &ProxyUserCredentials,
    resource_url: &str,
    kind: Option<HlsResourceKind>,
) -> String {
    let token = create_hls_resource_token(props.secret, props.user_token, resource_url, kind, props.origin_provider);
    build_hls_resource_uri(props.base_url, user, props.target_id, props.input_id, props.virtual_id, &token)
}

pub fn rewrite_hls(user: &ProxyUserCredentials, props: &RewriteHlsProps) -> String {
    let content = normalize_hls_playlist(props.content);
    let playlist_kind = props.playlist_kind.unwrap_or_else(|| classify_hls_playlist(content));
    let mut result = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // skip comments
        if line.starts_with('#') {
            let rewritten = rewrite_uri_attrib(line, props, user, playlist_kind);
            result.push(rewritten.to_string());
            continue;
        }

        // target url
        let target_url = rewrite_hls_resource_url(props, line, &user.username);
        result.push(rewrite_resource_uri(props, user, &target_url, uri_line_resource_kind(playlist_kind)));
    }
    result.push("\r\n".to_string());
    result.join("\r\n")
}

#[cfg(test)]
mod test {
    use super::{
        classify_hls_playlist, create_hls_resource_token, get_hls_session_token_and_url_from_token, rewrite_hls,
        rewrite_hls_resource_url, rewrite_hls_url, HlsManifestSource, HlsPlaylistKind, HlsResourceKind,
        HlsResourceToken, RewriteHlsProps, TOKEN_SEPARATOR_STR,
    };
    use rand::RngCore;
    use shared::{
        defaults::HLS_PREFIX,
        utils::{seal_hls_resource_url, u32_to_base64},
    };
    use tuliprox_core::model::ProxyUserCredentials;

    #[test]
    fn test_token_size() {
        for _i in 0..10_000 {
            let session_token = rand::rng().next_u32();
            assert_eq!(u32_to_base64(session_token).len(), 6);
        }
    }

    #[test]
    fn rewrite_http_relative_segments() {
        let cases = [
            ("http://example.com/hls/playlist.m3u8", "seg001.ts", "http://example.com/hls/seg001.ts"),
            ("http://example.com/hls/playlist.m3u8", "/media/seg001.ts", "http://example.com/media/seg001.ts"),
            ("http://example.com/hls/level1/playlist.m3u8", "../seg001.ts", "http://example.com/hls/seg001.ts"),
        ];
        for (base, uri, expected) in cases {
            assert_eq!(rewrite_hls_url(base, uri), expected);
        }
    }

    #[test]
    fn rewrite_relative_hls_resources_inherit_token() {
        let base = "https://cdn.example/hls/channel/timeshift_abs-1785136500.m3u8?token=secret";

        for (reference, expected) in [
            (
                "tracks-v1a1/timeshift_abs-1785136500.m3u8",
                "https://cdn.example/hls/channel/tracks-v1a1/timeshift_abs-1785136500.m3u8?token=secret",
            ),
            (
                "tracks-v1a1/dvr-2026/07/27/12/00/00-06000.ts",
                "https://cdn.example/hls/channel/tracks-v1a1/dvr-2026/07/27/12/00/00-06000.ts?token=secret",
            ),
            ("key.bin?version=2", "https://cdn.example/hls/channel/key.bin?version=2&token=secret"),
            ("init.mp4", "https://cdn.example/hls/channel/init.mp4?token=secret"),
        ] {
            assert_eq!(rewrite_hls_url(base, reference), expected);
        }
    }

    #[test]
    fn bounded_archive_token_survives_master_variant_and_segment_rewrites() {
        let master = "https://provider.example/channel/archive-1717200000-3600.m3u8?token=a%2Fb&auth=secret";
        let variant = rewrite_hls_url(master, "tracks-v1a1/archive-1717200000-3600.m3u8");
        assert_eq!(
            variant,
            "https://provider.example/channel/tracks-v1a1/archive-1717200000-3600.m3u8?token=a%2Fb&auth=secret"
        );
        let segment = rewrite_hls_url(&variant, "dvr/segment.ts");
        assert_eq!(segment, "https://provider.example/channel/tracks-v1a1/dvr/segment.ts?token=a%2Fb&auth=secret");
        assert_eq!(rewrite_hls_url(&variant, "//elsewhere.example/segment.ts"), "https://elsewhere.example/segment.ts");
    }

    #[test]
    fn bounded_archive_session_preserves_queries_for_generic_variant_names() {
        let props = RewriteHlsProps {
            secret: &[7; 16],
            base_url: "http://proxy",
            content: "",
            hls_url: "https://provider.example/channel/variant.m3u8?token=secret&auth=value".to_string(),
            target_id: 1,
            virtual_id: 42,
            input_id: 1,
            user_token: Some("m3u-catchup|fp|alice|42|archive|1717200000|3600"),
            origin_provider: None,
            playlist_kind: None,
        };
        assert_eq!(
            rewrite_hls_resource_url(&props, "segment.ts", "alice"),
            "https://provider.example/channel/segment.ts?token=secret&auth=value"
        );
        assert_eq!(
            rewrite_hls_resource_url(&props, "https://other.example/segment.ts", "alice"),
            "https://other.example/segment.ts"
        );
        let live = RewriteHlsProps { user_token: None, ..props };
        assert_eq!(
            rewrite_hls_resource_url(&live, "segment.ts", "alice"),
            "https://provider.example/channel/segment.ts?token=secret"
        );
    }

    #[test]
    fn archive_text_in_username_does_not_enable_bounded_archive_queries() {
        let mut props = RewriteHlsProps {
            secret: &[7; 16],
            base_url: "http://proxy",
            content: "",
            hls_url: "https://provider.example/channel/variant.m3u8?token=secret&auth=value".to_string(),
            target_id: 1,
            virtual_id: 42,
            input_id: 1,
            user_token: None,
            origin_provider: None,
            playlist_kind: None,
        };
        for (token, username) in [
            ("m3u-catchup|fp|alice|42|archive|42|3600", "alice|42|archive"),
            ("fp|alice|archive|name|42", "alice|archive|name"),
            ("m3u-catchup|fp|alice|archive|name|42|live", "alice|archive|name"),
            ("m3u-catchup|fp|alice|archive|name|42|opaque", "alice|archive|name"),
            ("m3u-catchup|fp|alice|42|archive|invalid|3600", "alice"),
            ("m3u-catchup|fp|alice|42|archive|1717200000|0", "alice"),
            ("m3u-catchup|fp|alice|43|archive|1717200000|3600", "alice"),
            ("m3u-catchup|fp|alice|42|archive|1717200000|3600|unexpected", "alice"),
        ] {
            props.user_token = Some(token);
            assert_eq!(
                rewrite_hls_resource_url(&props, "segment.ts", username),
                "https://provider.example/channel/segment.ts?token=secret",
                "{token}"
            );
        }
        for (token, username) in [
            ("m3u-catchup|fp|alice|archive|name|42|archive|1717200000|3600", "alice|archive|name"),
            ("m3u-catchup|fp|alice|42|archive|1717200000|3600|.m3u8", "alice"),
            ("catchup|fp|alice|42|archive|1717200000|3600", "alice"),
        ] {
            props.user_token = Some(token);
            assert_eq!(
                rewrite_hls_resource_url(&props, "segment.ts", username),
                "https://provider.example/channel/segment.ts?token=secret&auth=value",
                "{token}"
            );
        }
    }

    #[test]
    fn bounded_archive_inherits_query_only_on_same_origin_and_keeps_child_credentials() {
        let base = "https://provider.example/channel/archive-1717200000-3600.m3u8?token=parent&auth=secret";
        assert_eq!(
            rewrite_hls_url(base, "https://provider.example/channel/segment.ts?token=child"),
            "https://provider.example/channel/segment.ts?token=child&auth=secret"
        );
        assert_eq!(rewrite_hls_url(base, "https://other.example/segment.ts"), "https://other.example/segment.ts");
        assert_eq!(
            rewrite_hls_url(base, "https://provider.example:444/segment.ts"),
            "https://provider.example:444/segment.ts"
        );
        assert_eq!(
            rewrite_hls_url(base, "key.bin?AUTH=child"),
            "https://provider.example/channel/key.bin?AUTH=child&token=parent"
        );
        assert_eq!(
            rewrite_hls_url(
                "https://provider.example/channel/archive-1717200000-3600.ts?token=parent&auth=secret",
                "seg.ts"
            ),
            "https://provider.example/channel/seg.ts?token=parent"
        );
    }

    #[test]
    fn rewrite_relative_hls_resource_keeps_its_own_token() {
        let base = "https://cdn.example/hls/channel/index.m3u8?TOKEN=base";

        assert_eq!(
            rewrite_hls_url(base, "child.m3u8?token=child"),
            "https://cdn.example/hls/channel/child.m3u8?token=child"
        );
    }

    #[test]
    fn rewrite_does_not_leak_token_to_cross_origin_resource() {
        let base = "https://cdn.example/hls/channel/index.m3u8?token=secret";

        assert_eq!(rewrite_hls_url(base, "https://media.example/child.m3u8"), "https://media.example/child.m3u8");
        assert_eq!(rewrite_hls_url(base, "//media.example/child.m3u8"), "https://media.example/child.m3u8");
    }

    #[test]
    fn rewrite_hls_url_cases() {
        let cases = [
            (
                "https://cdn.example/hls/channel/index.m3u8?offset=-10752&utcstart=1785072000&useseq=t",
                "variant/playlist.m3u8?offset=-10752&useseq=t",
                "https://cdn.example/hls/channel/variant/playlist.m3u8?offset=-10752&useseq=t&utcstart=1785072000",
            ),
            (
                "https://cdn.example/hls/channel/index.m3u8?utcstart=1785072000&offset=-3600&end=1785075600&duration=3600",
                "variant/playlist.m3u8?offset=-1800",
                "https://cdn.example/hls/channel/variant/playlist.m3u8?offset=-1800&utcstart=1785072000&end=1785075600&duration=3600",
            ),
            (
                "https://cdn.example/hls/channel/index.m3u8?utcstart=1785072000",
                "variant/playlist.m3u8?utc=1785071000",
                "https://cdn.example/hls/channel/variant/playlist.m3u8?utc=1785071000",
            ),
            (
                "https://cdn.example/hls/channel/index.m3u8?start=1785072000",
                "variant/playlist.m3u8",
                "https://cdn.example/hls/channel/variant/playlist.m3u8",
            ),
            (
                "https://cdn.example/hls/channel/index.m3u8?utcstart=1785072000&offset=-3600",
                "segment.ts?sig=abc",
                "https://cdn.example/hls/channel/segment.ts?sig=abc",
            ),
            (
                "https://cdn.example/hls/channel/index.m3u8?utcstart=1785072000&offset=-3600",
                "key.bin?sig=def",
                "https://cdn.example/hls/channel/key.bin?sig=def",
            ),
            (
                "https://cdn.example/hls/channel/index.m3u8?utcstart=1785072000&offset=-3600",
                "init.mp4?sig=ghi",
                "https://cdn.example/hls/channel/init.mp4?sig=ghi",
            ),
            (
                "http://example.com/hls/playlist.m3u8",
                "https://cdn.example.org/video/seg.ts",
                "https://cdn.example.org/video/seg.ts",
            ),
            ("file:///mnt/media/hls/playlist.m3u8", "seg001.ts", "file:///mnt/media/hls/seg001.ts"),
            ("file:///mnt/media/hls/level1/playlist.m3u8", "../seg001.ts", "file:///mnt/media/hls/seg001.ts"),
            (
                "file:///mnt/media/hls/playlist.m3u8?utc=1785072000",
                "child.m3u8",
                "file:///mnt/media/hls/child.m3u8",
            ),
            ("file:///mnt/media/hls/playlist.m3u8", "file:///mnt/other/seg.ts", "file:///mnt/other/seg.ts"),
            ("http://example.com/hls/playlist.m3u8", "seg.ts#t=10", "http://example.com/hls/seg.ts#t=10"),
        ];

        for (base, uri, expected) in cases {
            assert_eq!(rewrite_hls_url(base, uri), expected, "failed for base: {base}, uri: {uri}");
        }
    }

    #[test]
    fn rewrite_absolute_same_origin_url_remains_exact_passthrough() {
        let base = "https://cdn.example/hls/channel/index.m3u8?utcstart=1785072000&offset=-3600";
        let reference = "https://cdn.example/hls/channel/segment.ts?sig=a%2Fb&token=x+y";

        assert!(matches!(rewrite_hls_url(base, reference), std::borrow::Cow::Borrowed(_)));
        assert_eq!(rewrite_hls_url(base, reference), reference);
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
            origin_provider: None,
            playlist_kind: None,
        };

        let rewritten = rewrite_hls(&user, &props);
        let segment_line = rewritten
            .lines()
            .find(|line| line.contains(&format!("/{HLS_PREFIX}/")))
            .expect("rewritten playlist should contain a segment URL");
        assert!(segment_line.contains("/hls/u/p/9/11/101/"));
        let token = segment_line.rsplit('/').next().expect("rewritten hls segment URL should include token");
        let decoded =
            get_hls_session_token_and_url_from_token(&secret, token).expect("rewritten hls token should decode");

        assert!(decoded.session_token.is_none());
        assert_eq!(decoded.url, "http://origin/live/segment.ts");
    }

    const SECRET: [u8; 16] = [7u8; 16];

    fn decode(token: &str) -> HlsResourceToken {
        get_hls_session_token_and_url_from_token(&SECRET, token).expect("token should decode")
    }

    fn test_user() -> ProxyUserCredentials {
        let mut user = ProxyUserCredentials::default();
        user.username = "u".to_string();
        user.password = "p".to_string();
        user
    }

    fn rewrite_props<'a>(content: &'a str, user_token: Option<&'a str>) -> RewriteHlsProps<'a> {
        RewriteHlsProps {
            secret: &SECRET,
            base_url: "http://proxy",
            content,
            hls_url: "http://origin/live/main.m3u8".to_string(),
            target_id: 9,
            virtual_id: 101,
            input_id: 11,
            user_token,
            origin_provider: Some("provider_a"),
            playlist_kind: None,
        }
    }

    fn token_of_uri(uri: &str) -> HlsResourceToken {
        decode(uri.rsplit('/').next().expect("uri should contain a token"))
    }

    fn uri_attribute(line: &str) -> &str {
        let start = line.find("URI=\"").expect("line should contain URI attribute") + 5;
        let end = line[start..].find('"').expect("URI attribute should be closed") + start;
        &line[start..end]
    }

    fn has_extension(path: &str, extension: &str) -> bool {
        std::path::Path::new(path).extension().is_some_and(|ext| ext == extension)
    }

    #[test]
    fn resource_token_round_trips_four_fields() {
        let manifest = HlsResourceKind::Manifest(HlsManifestSource::Entry);
        let token = create_hls_resource_token(&SECRET, Some("session"), "http://a/x.m3u8", Some(manifest), Some("p1"));
        assert_eq!(
            decode(&token),
            HlsResourceToken {
                session_token: Some("session".to_string()),
                url: "http://a/x.m3u8".to_string(),
                kind: Some(manifest),
                origin_provider: Some("p1".to_string()),
            }
        );

        let token = create_hls_resource_token(&SECRET, None, "http://a/seg.ts", Some(HlsResourceKind::Media), None);
        let decoded = decode(&token);
        assert_eq!(decoded.session_token, None);
        assert_eq!(decoded.origin_provider, None);
        assert_eq!(decoded.kind, Some(HlsResourceKind::Media));
        assert_eq!(decoded.url, "http://a/seg.ts");

        let token = create_hls_resource_token(
            &SECRET,
            Some("s"),
            "http://a/c.m3u8",
            Some(HlsResourceKind::Manifest(HlsManifestSource::Child)),
            None,
        );
        assert_eq!(decode(&token).kind, Some(HlsResourceKind::Manifest(HlsManifestSource::Child)));
    }

    #[test]
    fn resource_token_decodes_legacy_and_rejects_three_fields() {
        let sep = TOKEN_SEPARATOR_STR;
        let two = seal_hls_resource_url(&SECRET, &format!("session{sep}http://a/seg.ts"));
        let decoded = decode(&format!("{two}.ts"));
        assert_eq!(decoded.session_token.as_deref(), Some("session"));
        assert_eq!(decoded.url, "http://a/seg.ts");
        assert_eq!(decoded.kind, None);

        let one = seal_hls_resource_url(&SECRET, "http://a/seg.ts");
        let decoded = decode(&one);
        assert_eq!(decoded.session_token, None);
        assert_eq!(decoded.url, "http://a/seg.ts");

        let unknown = seal_hls_resource_url(&SECRET, &format!("s{sep}http://a/x{sep}zz{sep}"));
        assert_eq!(decode(&unknown).kind, None);

        let three = seal_hls_resource_url(&SECRET, &format!("s{sep}http://a/x{sep}m"));
        assert!(get_hls_session_token_and_url_from_token(&SECRET, &three).is_none());
    }

    #[test]
    fn manifest_tokens_always_end_in_m3u8() {
        let manifest = Some(HlsResourceKind::Manifest(HlsManifestSource::Entry));
        let ts = create_hls_resource_token(&SECRET, Some("s"), "http://a/timeshift_abs-1.ts?utc=1", manifest, None);
        assert!(has_extension(&ts, "m3u8"), "{ts}");
        let bare = create_hls_resource_token(&SECRET, Some("s"), "http://a/play/1234", manifest, None);
        assert!(has_extension(&bare, "m3u8"), "{bare}");
        assert!(decode(&bare).is_manifest());
        let media =
            create_hls_resource_token(&SECRET, Some("s"), "http://a/seg.ts", Some(HlsResourceKind::Media), None);
        assert!(has_extension(&media, "ts"), "{media}");
        let media_m3u8 =
            create_hls_resource_token(&SECRET, Some("s"), "http://a/x.m3u8", Some(HlsResourceKind::Media), None);
        assert!(!decode(&media_m3u8).is_manifest());
        let legacy = seal_hls_resource_url(&SECRET, "http://a/x.m3u8");
        assert!(decode(&legacy).is_manifest());
    }

    #[test]
    fn classify_hls_playlist_kinds() {
        assert_eq!(classify_hls_playlist("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8"), HlsPlaylistKind::Master);
        assert_eq!(classify_hls_playlist("#EXTM3U\n#EXT-X-I-FRAME-STREAM-INF:URI=\"i.m3u8\""), HlsPlaylistKind::Master);
        assert_eq!(
            classify_hls_playlist("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nseg.ts"),
            HlsPlaylistKind::Media
        );
        assert_eq!(
            classify_hls_playlist("#EXTM3U\r\n#EXT-X-TARGETDURATION:6\r\n#EXTINF:6,\r\nseg.ts\r\n"),
            HlsPlaylistKind::Media
        );
        assert_eq!(classify_hls_playlist("\u{FEFF}#EXTM3U\n#EXTINF:6,\nseg.ts"), HlsPlaylistKind::Media);
        assert_eq!(classify_hls_playlist("\n\n  \n#EXTM3U\n#EXTINF:6,\nseg.ts"), HlsPlaylistKind::Media);
        assert_eq!(
            classify_hls_playlist("#EXTM3U\n#EXTINF:-1,Channel #EXT-X-STREAM-INF Test\nseg.ts"),
            HlsPlaylistKind::Media
        );
        assert_eq!(classify_hls_playlist("#EXTM3U x-tvg-url=\"a\"\n#EXTINF:6,\nseg.ts"), HlsPlaylistKind::Media);
        assert_eq!(
            classify_hls_playlist("#EXTM3U\n#EXTINF:-1,\nhttp://cdn.example/chunklist.m3u8?token=a"),
            HlsPlaylistKind::Master
        );
        assert_eq!(classify_hls_playlist(""), HlsPlaylistKind::Invalid);
        assert_eq!(classify_hls_playlist("<html><body>error</body></html>"), HlsPlaylistKind::Invalid);
        assert_eq!(classify_hls_playlist("{\"error\":1}"), HlsPlaylistKind::Invalid);
        assert_eq!(classify_hls_playlist("#EXTM3U\n#EXT-X-VERSION:3"), HlsPlaylistKind::Invalid);
    }

    #[test]
    fn rewrite_hls_strips_bom_before_rewriting() {
        let user = test_user();
        let rewritten = rewrite_hls(&user, &rewrite_props("\u{FEFF}#EXTM3U\n#EXTINF:6,\nseg.ts", Some("s")));
        assert!(rewritten.starts_with("#EXTM3U"), "{rewritten}");
        let tag_lines_rewritten =
            rewritten.lines().filter(|line| line.contains("#EXT") && line.contains("/hls/")).count();
        assert_eq!(tag_lines_rewritten, 0);
    }

    #[test]
    fn rewrite_hls_master_assigns_manifest_child_kinds() {
        let user = test_user();
        let content = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",URI=\"tracks-a1/index.fmp4.m3u8\"\n\
            #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1,URI=\"iframe.m3u8\"\n\
            #EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
            #EXT-X-UNKNOWN-TAG:URI=\"other.bin\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"aud\"\n\
            tracks-v1/index.fmp4.m3u8";
        let rewritten = rewrite_hls(&user, &rewrite_props(content, Some("s")));
        let child = Some(HlsResourceKind::Manifest(HlsManifestSource::Child));
        let line = |prefix: &str| rewritten.lines().find(|line| line.starts_with(prefix)).expect(prefix).to_string();
        assert_eq!(token_of_uri(uri_attribute(&line("#EXT-X-MEDIA"))).kind, child);
        assert_eq!(token_of_uri(uri_attribute(&line("#EXT-X-I-FRAME-STREAM-INF"))).kind, child);
        assert_eq!(token_of_uri(uri_attribute(&line("#EXT-X-SESSION-KEY"))).kind, Some(HlsResourceKind::Media));
        assert_eq!(token_of_uri(uri_attribute(&line("#EXT-X-UNKNOWN-TAG"))).kind, None);
        let variant = token_of_uri(&line("http://proxy/hls/"));
        assert_eq!(variant.kind, child);
        assert_eq!(variant.origin_provider.as_deref(), Some("provider_a"));
        assert_eq!(variant.session_token.as_deref(), Some("s"));
        assert!(line("#EXT-X-MEDIA").contains(".m3u8\""));
    }

    #[test]
    fn rewrite_hls_media_assigns_media_kinds() {
        let user = test_user();
        let content = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n\
            #EXT-X-MAP:URI=\"init-1.hls.mp4\"\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
            #EXT-X-PART:DURATION=1,URI=\"part1.m4s\"\n\
            #EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part2.m4s\"\n\
            #EXTINF:6,\nseg-1.m4s";
        let rewritten = rewrite_hls(&user, &rewrite_props(content, Some("s")));
        for prefix in ["#EXT-X-MAP", "#EXT-X-KEY", "#EXT-X-PART", "#EXT-X-PRELOAD-HINT"] {
            let line = rewritten.lines().find(|line| line.starts_with(prefix)).expect(prefix);
            assert_eq!(token_of_uri(uri_attribute(line)).kind, Some(HlsResourceKind::Media), "{prefix}");
        }
        let segment = rewritten.lines().find(|line| line.starts_with("http://proxy/hls/")).expect("segment");
        assert_eq!(token_of_uri(segment).kind, Some(HlsResourceKind::Media));
    }

    #[test]
    fn rewrite_hls_nested_chunklist_is_manifest_child() {
        let rewritten = rewrite_hls(&test_user(), &rewrite_props("#EXTM3U\n#EXTINF:-1,\nchunklist.m3u8", Some("s")));
        let line = rewritten.lines().find(|line| line.starts_with("http://proxy/hls/")).expect("chunklist");
        assert_eq!(token_of_uri(line).kind, Some(HlsResourceKind::Manifest(HlsManifestSource::Child)));
    }

    #[test]
    fn build_hls_resource_uri_format() {
        assert_eq!(
            super::build_hls_resource_uri("http://proxy", &test_user(), 9, 11, 101, "tok.m3u8"),
            "http://proxy/hls/u/p/9/11/101/tok.m3u8"
        );
    }
}
