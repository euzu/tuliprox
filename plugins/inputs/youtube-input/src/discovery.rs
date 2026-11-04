use quick_xml::{events::Event, reader::Reader};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredVideo {
    pub video_id: String,
    pub title: String,
    pub channel_title: String,
    pub thumbnail_url: String,
}

/// Identifies the kind of YouTube input target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    ChannelId(String),
    PlaylistId(String),
    /// `@name`.
    Handle(String),
    /// A pre-handle custom URL, kept as its page path: `c/<name>` or `user/<name>`. The name is not a handle,
    /// so it is resolved through its own page.
    LegacyName(String),
    /// A channel's `/live` page (`@name/live`, `channel/UC…/live`, `c/<name>/live`): its running broadcast is
    /// fetched whatever the input's mode. Wraps a `ChannelId`, `Handle` or `LegacyName`.
    Live(Box<TargetKind>),
    VideoId(String),
    Invalid(String),
}

fn is_id_char(c: char) -> bool { c.is_ascii_alphanumeric() || c == '-' || c == '_' }

/// An 11-character video ID.
fn is_video_id(id: &str) -> bool { id.len() == 11 && id.chars().all(is_id_char) }

/// A 24-character channel ID `UC…`.
fn is_channel_id(id: &str) -> bool { id.len() == 24 && id.starts_with("UC") && id.chars().all(is_id_char) }

/// A playlist ID. Their lengths vary (`PL…`, `UU…`, `OLAK5uy_…`, `RD…`), so only the alphabet is checked.
fn is_playlist_id(id: &str) -> bool { !id.is_empty() && id.len() <= 64 && id.chars().all(is_id_char) }

fn video(id: &str) -> TargetKind {
    if is_video_id(id) {
        TargetKind::VideoId(id.to_string())
    } else {
        TargetKind::Invalid(format!("invalid video ID: {id}"))
    }
}

fn channel(id: &str) -> TargetKind {
    if is_channel_id(id) {
        TargetKind::ChannelId(id.to_string())
    } else {
        TargetKind::Invalid(format!("invalid channel ID: {id}"))
    }
}

fn playlist(id: &str) -> TargetKind {
    if is_playlist_id(id) {
        TargetKind::PlaylistId(id.to_string())
    } else {
        TargetKind::Invalid(format!("invalid playlist ID: {id}"))
    }
}

/// `kind`, or its `/live` page when the path continues with `live`.
fn with_live_suffix(kind: TargetKind, rest: &[&str]) -> TargetKind {
    match kind {
        TargetKind::Invalid(_) => kind,
        _ if rest.first() == Some(&"live") => TargetKind::Live(Box::new(kind)),
        _ => kind,
    }
}

/// Parses a user-supplied string into a recognized YouTube target using structured URL decomposition.
#[must_use]
pub fn parse_target(raw: &str) -> TargetKind {
    let trimmed = raw.trim();
    let input = trimmed.split_once('#').map_or(trimmed, |(before, _)| before).trim();

    if input.is_empty() {
        return TargetKind::Invalid("empty target".to_string());
    }

    // 1. Bare IDs and handles
    if is_channel_id(input) {
        return TargetKind::ChannelId(input.to_string());
    }

    // 11-char video ID check MUST precede playlist prefix check so IDs like `PLbA1cD2eFg` are parsed as VideoId
    if is_video_id(input) {
        return TargetKind::VideoId(input.to_string());
    }

    // Check known YouTube playlist prefixes (PL, UU, OLAK, RD, LL, FL)
    if ["PL", "UU", "OLAK", "RD", "LL", "FL"].iter().any(|prefix| input.starts_with(prefix)) && is_playlist_id(input) {
        return TargetKind::PlaylistId(input.to_string());
    }

    // Bare handles: `@name` or `@name/live`
    if let Some(stripped) = input.strip_prefix('@') {
        let mut parts = stripped.split('/');
        let handle = parts.next().unwrap_or(stripped);
        if !handle.is_empty() && !handle.contains('.') {
            let rest: Vec<&str> = parts.filter(|part| !part.is_empty()).collect();
            return with_live_suffix(TargetKind::Handle(format!("@{handle}")), &rest);
        }
    }

    // 2. Normalize URL (strip protocol) and split it into host, path and query
    let url_without_proto = input.strip_prefix("https://").or_else(|| input.strip_prefix("http://")).unwrap_or(input);

    let (host_and_path, query_str) = match url_without_proto.split_once('?') {
        Some((hp, q)) => (hp, q),
        None => (url_without_proto, ""),
    };

    let (host, path) = match host_and_path.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (host_and_path, "/".to_string()),
    };

    // 3. Only YouTube hosts are accepted. A bare ID or handle returned above never reaches this point,
    // so anything host-shaped that is left must name a supported domain.
    let bare_host = host.split(':').next().unwrap_or(host);
    let lower_host = bare_host.to_ascii_lowercase();
    let is_youtube_host = lower_host == "youtu.be"
        || lower_host.ends_with(".youtu.be")
        || lower_host == "youtube.com"
        || lower_host.ends_with(".youtube.com");
    if !is_youtube_host && (bare_host.contains('.') || input.contains('/')) {
        return TargetKind::Invalid(format!("unsupported domain: {bare_host}"));
    }

    // Helper to extract a query parameter value
    let get_query_param = |name: &str| -> Option<String> {
        for part in query_str.split('&') {
            if let Some((k, v)) = part.split_once('=') {
                if k == name && !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        None
    };

    // 4. youtu.be shortlinks: youtu.be/<id>
    if lower_host == "youtu.be" || lower_host.ends_with(".youtu.be") {
        let seg = path.trim_start_matches('/');
        let id = seg.split('/').next().unwrap_or(seg);
        if !id.is_empty() {
            return video(id);
        }
    }

    // 5. youtube.com domains
    let segments: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();

    if let Some((&first, rest)) = segments.split_first() {
        // /watch?v=... (check v parameter before playlist list parameter)
        if first == "watch" {
            if let Some(v) = get_query_param("v") {
                return video(&v);
            }
            if let Some(list) = get_query_param("list") {
                return playlist(&list);
            }
        }

        // /playlist?list=...
        if first == "playlist" {
            if let Some(list) = get_query_param("list") {
                return playlist(&list);
            }
        }

        // /live/<id>, /shorts/<id>, /embed/<id>
        if matches!(first, "live" | "shorts" | "embed") {
            if let Some(id) = rest.first() {
                return video(id);
            }
        }

        // /channel/<channel_id> or /channel/<channel_id>/live
        if first == "channel" {
            if let Some((id, after)) = rest.split_first() {
                return with_live_suffix(channel(id), after);
            }
        }

        // /@<handle> or /@<handle>/live
        if first.len() > 1 && first.starts_with('@') {
            return with_live_suffix(TargetKind::Handle(first.to_string()), rest);
        }

        // /user/<name> or /c/<name>, optionally followed by /live
        if first == "user" || first == "c" {
            if let Some((name, after)) = rest.split_first() {
                return with_live_suffix(TargetKind::LegacyName(format!("{first}/{name}")), after);
            }
        }
    }

    // Fallback query parameter checks
    if let Some(v) = get_query_param("v") {
        return video(&v);
    }
    if let Some(list) = get_query_param("list") {
        return playlist(&list);
    }

    TargetKind::Invalid(format!("unrecognized YouTube target: {input}"))
}

/// Builds the RSS feed URL for a given target.
#[must_use]
pub fn feed_url_for_target(target: &TargetKind) -> Option<String> {
    match target {
        TargetKind::ChannelId(id) => Some(format!("https://www.youtube.com/feeds/videos.xml?channel_id={id}")),
        TargetKind::PlaylistId(id) => Some(format!("https://www.youtube.com/feeds/videos.xml?playlist_id={id}")),
        TargetKind::Handle(_)
        | TargetKind::LegacyName(_)
        | TargetKind::Live(_)
        | TargetKind::VideoId(_)
        | TargetKind::Invalid(_) => None,
    }
}

/// The 24-character channel ID starting at `start`, or `None` when the bytes there are not one.
///
/// The slice is taken by characters, so a multi-byte codepoint at the boundary cannot split.
fn channel_id_at(html: &str, start: usize) -> Option<String> {
    let candidate: String = html.get(start..)?.chars().take(24).collect();
    let is_id = candidate.len() == 24
        && candidate.starts_with("UC")
        && candidate.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    is_id.then_some(candidate)
}

/// Extracts the channel ID `UC...` that names the page itself.
///
/// Only the canonical link and `og:url` are read: they carry the page's own ID. The rest of the document links
/// to *recommended* channels (`youtube.com/channel/UC...`) and names their `"channelId"` entries, and on a page
/// whose canonical URL is a watch link that body text belongs to somebody else. Returning `None` lets the caller
/// fall back to Invidious instead of silently resolving the handle to the wrong channel.
#[must_use]
pub fn extract_channel_id_from_html(html: &str) -> Option<String> {
    const HEAD_MARKERS: [&str; 2] = [
        r#"rel="canonical" href="https://www.youtube.com/channel/"#,
        r#"property="og:url" content="https://www.youtube.com/channel/"#,
    ];

    HEAD_MARKERS
        .iter()
        .filter_map(|marker| html.find(marker).and_then(|pos| channel_id_at(html, pos + marker.len())))
        .next()
}

/// Extracts an active live stream video ID and the page title from a YouTube `/live` page.
///
/// Only the canonical link and `og:url` are read: on a channel that is live they name the running broadcast's watch
/// page. A channel that is not live answers with its channel page, whose canonical URL is the channel and whose body
/// links to *recommended* videos and other channels' streams; none of those is the answer, so `None` is returned.
#[must_use]
pub fn extract_live_info_from_html(html: &str) -> Option<(String, Option<String>)> {
    const HEAD_MARKERS: [&str; 2] = [
        r#"rel="canonical" href="https://www.youtube.com/watch?v="#,
        r#"property="og:url" content="https://www.youtube.com/watch?v="#,
    ];

    let video_id = HEAD_MARKERS.iter().find_map(|marker| {
        let pos = html.find(marker)?;
        let candidate: String = html.get(pos + marker.len()..)?.chars().take(11).collect();
        is_video_id(&candidate).then_some(candidate)
    })?;

    // The page title names the running broadcast; it is HTML, not XML, so its entities are decoded here.
    let title = html.find("<title>").and_then(|start| {
        let after = html.get(start + 7..)?;
        let end = after.find("</title>")?;
        let raw = after.get(..end)?;
        let clean = unescape_html(raw).trim().trim_end_matches(" - YouTube").trim().to_string();
        (!clean.is_empty()).then_some(clean)
    });

    Some((video_id, title))
}

/// Decodes the HTML character references that occur in a page title.
///
/// The five named references plus `&nbsp;` and the numeric forms are covered; anything else is kept
/// verbatim, so unknown markup survives unchanged instead of disappearing.
fn unescape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;

    while idx < input.len() {
        let rest = &input[idx..];
        let mut chars = rest.char_indices();
        let Some((_, first)) = chars.next() else { break };
        if first != '&' {
            out.push(first);
            idx += first.len_utf8();
            continue;
        }

        let end = chars.take(10).find(|(_, c)| *c == ';').map(|(off, _)| off);
        match end.map(|end| (end, decode_entity(&rest[1..end]))) {
            Some((end, Some(decoded))) => {
                out.push(decoded);
                idx += end + 1;
            }
            _ => {
                out.push('&');
                idx += 1;
            }
        }
    }

    out
}

fn decode_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let code = name.strip_prefix('#')?;
            let value = match code.strip_prefix('x').or_else(|| code.strip_prefix('X')) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => code.parse::<u32>().ok()?,
            };
            char::from_u32(value)
        }
    }
}

/// Parses a YouTube Atom feed XML into a list of discovered videos.
/// YouTube Atom feeds are delivered sorted descending (newest first).
pub fn parse_atom_feed(xml: &str, limit: usize) -> Result<(String, Vec<DiscoveredVideo>), String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut feed_title = String::new();
    let mut videos = Vec::new();

    let mut in_feed = false;
    let mut in_entry = false;
    let mut current_tag = String::new();

    let mut cur_video_id = String::new();
    let mut cur_title = String::new();
    let mut cur_author = String::new();
    let mut cur_thumbnail = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "feed" {
                    in_feed = true;
                } else if name == "entry" {
                    in_entry = true;
                    cur_video_id.clear();
                    cur_title.clear();
                    cur_author.clear();
                    cur_thumbnail.clear();
                }
                current_tag = name;
            }
            Ok(Event::Empty(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if in_entry && (name == "media:thumbnail" || name.ends_with(":thumbnail")) {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"url" {
                            cur_thumbnail = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                }
            }
            Ok(Event::Text(ref e)) => {
                let text = match e.unescape() {
                    Ok(t) => t.into_owned(),
                    Err(_) => String::from_utf8_lossy(e.as_ref()).to_string(),
                };
                if in_entry {
                    if current_tag == "yt:videoId" || current_tag.ends_with(":videoId") {
                        cur_video_id = text;
                    } else if current_tag == "title" && cur_title.is_empty() {
                        cur_title = text;
                    } else if current_tag == "name" && cur_author.is_empty() {
                        cur_author = text;
                    }
                } else if in_feed && current_tag == "title" && feed_title.is_empty() {
                    feed_title = text;
                }
            }
            Ok(Event::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "entry" {
                    if !cur_video_id.is_empty() {
                        let channel_name = if !cur_author.is_empty() {
                            cur_author.clone()
                        } else if !feed_title.is_empty() {
                            feed_title.clone()
                        } else {
                            "YouTube".to_string()
                        };

                        videos.push(DiscoveredVideo {
                            video_id: cur_video_id.clone(),
                            title: if cur_title.is_empty() { "Untitled".to_string() } else { cur_title.clone() },
                            channel_title: channel_name,
                            thumbnail_url: cur_thumbnail.clone(),
                        });

                        if videos.len() >= limit {
                            break;
                        }
                    }
                    in_entry = false;
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(format!("error parsing feed XML: {err}")),
            _ => {}
        }
    }

    Ok((feed_title, videos))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn parse_targets_comprehensive() {
        let cases = [
            ("https://www.youtube.com/watch?v=dQw4w9WgXcQ", TargetKind::VideoId("dQw4w9WgXcQ".into())),
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL1234567890123456",
                TargetKind::VideoId("dQw4w9WgXcQ".into()),
            ),
            (
                "https://www.youtube.com/playlist?list=PL1234567890123456",
                TargetKind::PlaylistId("PL1234567890123456".into()),
            ),
            ("https://www.youtube.com/playlist?list=PLshort13", TargetKind::PlaylistId("PLshort13".into())),
            ("PLshort13", TargetKind::PlaylistId("PLshort13".into())),
            // 11-char video ID starting with PL must parse as VideoId, not PlaylistId
            ("PLbA1cD2eFg", TargetKind::VideoId("PLbA1cD2eFg".into())),
            ("RDdQw4w9WgX", TargetKind::VideoId("RDdQw4w9WgX".into())),
            ("UU1234567890", TargetKind::PlaylistId("UU1234567890".into())),
            ("RD1234567890", TargetKind::PlaylistId("RD1234567890".into())),
            ("https://youtu.be/dQw4w9WgXcQ?t=42", TargetKind::VideoId("dQw4w9WgXcQ".into())),
            ("https://www.youtube.com/live/_QMBCjq5xCg", TargetKind::VideoId("_QMBCjq5xCg".into())),
            ("https://www.youtube.com/shorts/dQw4w9WgXcQ", TargetKind::VideoId("dQw4w9WgXcQ".into())),
            ("https://www.youtube.com/embed/dQw4w9WgXcQ", TargetKind::VideoId("dQw4w9WgXcQ".into())),
            (
                "https://www.youtube.com/channel/UC1234567890123456789012",
                TargetKind::ChannelId("UC1234567890123456789012".into()),
            ),
            (
                "https://www.youtube.com/channel/UC1234567890123456789012/live",
                TargetKind::Live(Box::new(TargetKind::ChannelId("UC1234567890123456789012".into()))),
            ),
            (
                "https://www.youtube.com/@SkyNews/live",
                TargetKind::Live(Box::new(TargetKind::Handle("@SkyNews".into()))),
            ),
            ("@tagesschau/live", TargetKind::Live(Box::new(TargetKind::Handle("@tagesschau".into())))),
            ("@tagesschau/videos", TargetKind::Handle("@tagesschau".into())),
            ("https://www.youtube.com/@tagesschau", TargetKind::Handle("@tagesschau".into())),
            ("https://www.youtube.com/c/tagesschau", TargetKind::LegacyName("c/tagesschau".into())),
            ("https://www.youtube.com/user/tagesschau", TargetKind::LegacyName("user/tagesschau".into())),
            (
                "https://www.youtube.com/c/tagesschau/live",
                TargetKind::Live(Box::new(TargetKind::LegacyName("c/tagesschau".into()))),
            ),
            ("@tagesschau", TargetKind::Handle("@tagesschau".into())),
            ("UC1234567890123456789012", TargetKind::ChannelId("UC1234567890123456789012".into())),
            ("dQw4w9WgXcQ", TargetKind::VideoId("dQw4w9WgXcQ".into())),
            ("", TargetKind::Invalid("empty target".into())),
            ("   ", TargetKind::Invalid("empty target".into())),
            ("https://example.com/foo", TargetKind::Invalid("unsupported domain: example.com".into())),
            // Schemeless foreign hosts must not be read as a path on youtube.com
            ("example.com/watch?v=dQw4w9WgXcQ", TargetKind::Invalid("unsupported domain: example.com".into())),
            ("example.com/@tagesschau", TargetKind::Invalid("unsupported domain: example.com".into())),
            // A bare word is neither an ID nor a handle
            ("tagesschau", TargetKind::Invalid("unrecognized YouTube target: tagesschau".into())),
            ("youtu.be/dQw4w9WgXcQ", TargetKind::VideoId("dQw4w9WgXcQ".into())),
            ("WWW.YOUTUBE.COM/watch?v=dQw4w9WgXcQ", TargetKind::VideoId("dQw4w9WgXcQ".into())),
            ("https://www.youtube.com/watch?v=dQw4w9WgXcQ#t=42", TargetKind::VideoId("dQw4w9WgXcQ".into())),
            ("https://www.youtube.com/playlist?list=OLAK5uy_abc-123", TargetKind::PlaylistId("OLAK5uy_abc-123".into())),
            // Malformed IDs are rejected instead of being sent to Invidious or the feed endpoint
            ("https://www.youtube.com/watch?v=short", TargetKind::Invalid("invalid video ID: short".into())),
            ("https://youtu.be/dQw4w9WgXcQ123", TargetKind::Invalid("invalid video ID: dQw4w9WgXcQ123".into())),
            ("https://www.youtube.com/channel/UCshort", TargetKind::Invalid("invalid channel ID: UCshort".into())),
            (
                "https://www.youtube.com/playlist?list=PL%3Cscript%3E",
                TargetKind::Invalid("invalid playlist ID: PL%3Cscript%3E".into()),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(parse_target(input), expected, "failed for input: {input}");
        }
    }

    #[test]
    fn extract_channel_id_multibyte_safety() {
        let test_cases = [
            "<link rel=\"canonical\" href=\"https://www.youtube.com/channel/UC12345678901234567890äöü\">",
            "<link rel=\"canonical\" href=\"https://www.youtube.com/channel/UC12345678901234567890123€\">",
            "{\"channelId\":\"UC12345678901234567890äöü\"}",
            "{\"channelId\":\"UC12345678901234567890123🙂\"}",
            "<link rel=\"canonical\" href=\"https://www.youtube.com/channel/UCBR8-60-44284mFTZt7W9Aw\">",
        ];

        for case in test_cases {
            let _ = extract_channel_id_from_html(case);
        }

        let valid_html = r#"<link rel="canonical" href="https://www.youtube.com/channel/UCBR8-60-44284mFTZt7W9Aw">"#;
        assert_eq!(extract_channel_id_from_html(valid_html), Some("UCBR8-60-44284mFTZt7W9Aw".to_string()));
    }

    #[test]
    fn extract_channel_id_ignores_recommended_channels() {
        // The page body names recommended channels; only the canonical link names this page's channel.
        let with_recommendations = r#"<html><head><meta property="og:url" content="https://www.youtube.com/channel/UCoMdktPbSTixAyNGwb-UYkQ"></head>
<body data-first="{\"channelId\":\"UCbbS1GE942k3UVqpLklyhIA\"}">
<a href="https://www.youtube.com/channel/UCbbS1GE942k3UVqpLklyhIA">Other</a></body></html>"#;
        assert_eq!(extract_channel_id_from_html(with_recommendations), Some("UCoMdktPbSTixAyNGwb-UYkQ".to_string()));

        // Without a canonical link or og:url the JSON entries of recommended channels are not trusted.
        let recommendations_only = r#"<html><body>{"channelId":"UCbbS1GE942k3UVqpLklyhIA"}</body></html>"#;
        assert_eq!(extract_channel_id_from_html(recommendations_only), None);

        // A watch page (canonical is the video) links other channels in its body; none of them is the answer.
        let watch_page = r#"<html><head><link rel="canonical" href="https://www.youtube.com/watch?v=zC5s9yaaAE0"></head>
<body><a href="https://www.youtube.com/channel/UCzG5BnqHO8oNlrPDW9CYJog">Recommended</a></body></html>"#;
        assert_eq!(extract_channel_id_from_html(watch_page), None);
    }

    #[test]
    fn extract_live_info_unescapes_title() {
        let live_html = r#"<html><head><title>FRANCE 24 English &#8211; LIVE &#38; Top stories &amp; more - YouTube</title><link rel="canonical" href="https://www.youtube.com/watch?v=HvZt-nh9sGg"></head></html>"#;
        assert_eq!(
            extract_live_info_from_html(live_html),
            Some(("HvZt-nh9sGg".to_string(), Some("FRANCE 24 English – LIVE & Top stories & more".to_string())))
        );

        // An unknown reference is kept verbatim instead of being dropped
        assert_eq!(unescape_html("a &unknown; b && c"), "a &unknown; b && c");
    }

    #[test]
    fn extract_live_info() {
        let live_html = r#"<html><head><title>Sky News Live - YouTube</title><link rel="canonical" href="https://www.youtube.com/watch?v=LuKwFajn37U"></head></html>"#;
        assert_eq!(
            extract_live_info_from_html(live_html),
            Some(("LuKwFajn37U".to_string(), Some("Sky News Live".to_string())))
        );
        assert_eq!(extract_live_info_from_html("no live stream here"), None);
    }

    #[test]
    fn extract_live_info_ignores_streams_linked_from_the_page_body() {
        // A channel that is not live: its page links other channels' broadcasts, none of which is this channel's.
        let not_live = r#"<html><head><title>Kanal - YouTube</title>
<link rel="canonical" href="https://www.youtube.com/channel/UCoMdktPbSTixAyNGwb-UYkQ"></head>
<body><a href="https://www.youtube.com/watch?v=zC5s9yaaAE0">Recommended live</a></body></html>"#;
        assert_eq!(extract_live_info_from_html(not_live), None);

        // og:url is accepted when the canonical link is missing.
        let og_only = r#"<html><head><title>Live</title><meta property="og:url" content="https://www.youtube.com/watch?v=LuKwFajn37U"></head></html>"#;
        assert_eq!(extract_live_info_from_html(og_only), Some(("LuKwFajn37U".to_string(), Some("Live".to_string()))));
    }
}
