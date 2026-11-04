use std::time::Duration;
use tuliprox_plugin_sdk::{clock, http, prelude::*};

mod cache;
mod discovery;
mod resolver;

use discovery::{extract_channel_id_from_html, extract_live_info_from_html, parse_atom_feed, parse_target, TargetKind};
use resolver::ResolvedStream;

const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[tuliprox_plugin(manifest = "manifest.yml")]
mod plugin {
    use super::*;

    /// Defaults for anything an input leaves unset. The plugin declares no settings schema: the
    /// configuration belongs to each input (its `ui.input_form`), so this object is `{}` unless a
    /// config written before that move still carries a `settings:` block, which stays honoured.
    #[tuliprox_plugin_settings]
    pub struct Settings {
        #[serde(default)]
        pub channels: Vec<String>,
        #[serde(default = "default_mode")]
        pub mode: String,
        #[serde(default = "default_max_uploads")]
        pub max_uploads: u32,
        #[serde(default)]
        pub invidious_instances: Vec<String>,
        #[serde(default = "default_cache_ttl_minutes")]
        pub cache_ttl_minutes: u32,
    }

    fn default_mode() -> String { "recent_uploads".to_string() }
    fn default_max_uploads() -> u32 { 10 }
    fn default_cache_ttl_minutes() -> u32 { 120 }

    /// The `plugin_config` of one input, as the host hands it over in
    /// `input-request.options`: per-input overrides of the plugin's settings.
    /// Every field the input leaves unset keeps the plugin's own setting.
    #[derive(Debug, Default, PartialEq)]
    pub(super) struct InputOverrides {
        pub(super) mode: Option<String>,
        pub(super) max_uploads: Option<u32>,
        pub(super) cache_ttl_minutes: Option<u32>,
        pub(super) invidious_instances: Option<Vec<String>>,
        /// Targets this input names in its own list, next to (or instead of) its `url`.
        pub(super) channels: Option<Vec<String>>,
    }

    /// A list option arrives as its JSON text (`["https://a"]`), because the ABI
    /// hands options over as strings. A plain comma-separated list is accepted
    /// too, so a hand-written `plugin_config: "https://a, https://b"` works.
    fn parse_instance_list(value: &str) -> Vec<String> {
        let entries: Vec<String> = match serde_json::from_str::<Vec<String>>(value) {
            Ok(list) => list,
            Err(_) => value.split(',').map(str::to_string).collect(),
        };
        entries.into_iter().map(|entry| entry.trim().to_string()).filter(|entry| !entry.is_empty()).collect()
    }

    pub(super) fn input_overrides(req: &InputRequest) -> InputOverrides {
        let mut overrides = InputOverrides::default();
        for (key, value) in &req.options {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.as_str() {
                "mode" => overrides.mode = Some(value.to_string()),
                "max_uploads" => match value.parse::<u32>() {
                    Ok(limit) => overrides.max_uploads = Some(limit),
                    Err(_) => warn!("ignoring input override max_uploads='{value}': not a number"),
                },
                "cache_ttl_minutes" => match value.parse::<u32>() {
                    Ok(minutes) => overrides.cache_ttl_minutes = Some(minutes),
                    Err(_) => warn!("ignoring input override cache_ttl_minutes='{value}': not a number"),
                },
                "invidious_instances" => {
                    let instances = parse_instance_list(value);
                    if instances.is_empty() {
                        warn!("ignoring input override invidious_instances: no instance URL in '{value}'");
                    } else {
                        overrides.invidious_instances = Some(instances);
                    }
                }
                "channels" => {
                    let targets = parse_instance_list(value);
                    if targets.is_empty() {
                        warn!("ignoring input override channels: no target in '{value}'");
                    } else {
                        overrides.channels = Some(targets);
                    }
                }
                other => debug!("ignoring unknown input option '{other}'"),
            }
        }
        overrides
    }

    struct FetchCtx<'a> {
        instances: &'a [String],
        active_cursor: &'a mut usize,
        deadline: i64,
        cache_ttl_minutes: u32,
        input_name: &'a str,
    }

    pub struct YoutubeInputPlugin;

    impl InputProvider for YoutubeInputPlugin {
        fn fetch_playlist(req: &InputRequest) -> Result<Vec<Channel>, String> {
            let settings = Settings::current().map_err(|e| e.to_string())?;

            // 1. Validate the mode setting; an input's own `plugin_config` overrides it.
            let overrides = input_overrides(req);
            let mode = overrides.mode.as_deref().unwrap_or(settings.mode.as_str());
            if mode != "recent_uploads" && mode != "live_only" {
                return Err(format!(
                    "invalid mode '{mode}' in YouTube plugin settings; expected 'recent_uploads' or 'live_only'"
                ));
            }

            // Every setting can be overridden per input; the plugin's own value is the default.
            let instances = overrides.invidious_instances.as_deref().unwrap_or(&settings.invidious_instances);
            let cache_ttl_minutes = overrides.cache_ttl_minutes.unwrap_or(settings.cache_ttl_minutes).clamp(10, 360);

            if instances.is_empty() {
                return Err(format!(
                    "input '{}' has no Invidious instance: set at least one URL in its Invidious Instances field",
                    req.input_name
                ));
            }

            // 2. Resolve the targets: the input's `url` (one entry, or several comma-separated ones) and the
            // input's own `channels` list, in that order and without duplicates. A config written before the
            // settings moved per input still falls back to the plugin's `channels` list.
            let mut targets: Vec<String> = Vec::new();
            let mut push = |entry: &str| {
                let entry = entry.trim();
                if !entry.is_empty() && !targets.iter().any(|known| known == entry) {
                    targets.push(entry.to_string());
                }
            };
            for entry in req.url.split(',') {
                push(entry);
            }
            match overrides.channels.as_deref() {
                Some(channels) => channels.iter().for_each(|entry| push(entry)),
                None if req.url.trim().is_empty() => settings.channels.iter().for_each(|entry| {
                    for part in entry.split(',') {
                        push(part);
                    }
                }),
                None => {}
            }

            if targets.is_empty() {
                return Err(
                    format!(
                        "input '{}' has no target: set the input's Url or its Channels list to a channel, playlist or video",
                        req.input_name
                    ),
                );
            }

            info!(
                "fetching YouTube playlist for input '{}' ({} target(s), mode '{}', {} instance(s))",
                req.input_name,
                targets.len(),
                mode,
                instances.len()
            );

            // 3. Wall-clock deadline budget: 25 seconds (fits within any host max_exec >= 2.5s)
            let start_time = clock::now_unix();
            let deadline = start_time + 25;
            let mut active_cursor = 0;
            let max_uploads = overrides.max_uploads.unwrap_or(settings.max_uploads).clamp(1, 15);

            let mut ctx = FetchCtx {
                instances,
                active_cursor: &mut active_cursor,
                deadline,
                cache_ttl_minutes,
                input_name: &req.input_name,
            };

            let mut channels = Vec::new();
            let mut had_hard_error = false;

            for target_str in &targets {
                let now = clock::now_unix();
                if now >= deadline - 2 {
                    warn!("time budget nearly exhausted ({}s left), stopping discovery early", deadline - now);
                    break;
                }

                let target = parse_target(target_str);
                debug!("processing YouTube target: {target_str} (parsed: {target:?})");

                match target {
                    TargetKind::Invalid(reason) => {
                        warn!("skipping invalid target '{target_str}': {reason}");
                        had_hard_error = true;
                    }
                    TargetKind::VideoId(video_id) => {
                        match resolver::resolve_stream(
                            &video_id,
                            ctx.instances,
                            ctx.active_cursor,
                            ctx.cache_ttl_minutes,
                            ctx.deadline,
                        ) {
                            Ok(resolved) => {
                                if mode == "live_only" && !resolved.is_live {
                                    continue;
                                }
                                channels.push(make_channel(
                                    &video_id,
                                    &format!("YouTube Video {video_id}"),
                                    "YouTube",
                                    &format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"),
                                    &resolved,
                                    ctx.input_name,
                                ));
                            }
                            Err(err) => {
                                warn!("failed to resolve single video {video_id}: {err}");
                                had_hard_error = true;
                            }
                        }
                    }
                    TargetKind::ChannelId(ref ucid) => {
                        if mode == "live_only" {
                            // In live_only mode, query channel /live directly (discovers active 24/7 streams)
                            let live_url = format!("https://www.youtube.com/channel/{ucid}/live");
                            if let Err(e) = discover_and_resolve_live(
                                &live_url,
                                &format!("Channel {ucid}"),
                                &mut ctx,
                                &mut channels,
                            ) {
                                warn!("live discovery error for channel {ucid}: {e}");
                                had_hard_error = true;
                            }
                        } else if let Some(feed_url) = discovery::feed_url_for_target(&target) {
                            fetch_and_append_feed(
                                &feed_url,
                                mode,
                                max_uploads,
                                &mut ctx,
                                &mut channels,
                                &mut had_hard_error,
                            );
                        }
                    }
                    TargetKind::PlaylistId(_) => {
                        if let Some(feed_url) = discovery::feed_url_for_target(&target) {
                            fetch_and_append_feed(
                                &feed_url,
                                mode,
                                max_uploads,
                                &mut ctx,
                                &mut channels,
                                &mut had_hard_error,
                            );
                        }
                    }
                    TargetKind::Handle(ref handle) => {
                        let encoded_handle = url_encode_handle(handle);
                        if mode == "live_only" {
                            // Query @handle/live directly (discovers ongoing 24/7 streams like @DWNews or @France24_en)
                            let live_url = format!("https://www.youtube.com/{encoded_handle}/live");
                            if let Err(e) = discover_and_resolve_live(&live_url, handle, &mut ctx, &mut channels) {
                                warn!("live discovery error for {handle}: {e}");
                                had_hard_error = true;
                            }
                        } else {
                            // In recent_uploads mode, resolve handle to channel ID and fetch RSS feed
                            let resolved_channel_id = resolve_handle_via_youtube_html(&encoded_handle)
                                .or_else(|| resolve_handle_via_invidious(&encoded_handle, ctx.instances));

                            match resolved_channel_id {
                                Some(channel_id) => {
                                    let feed_target = TargetKind::ChannelId(channel_id);
                                    if let Some(feed_url) = discovery::feed_url_for_target(&feed_target) {
                                        fetch_and_append_feed(
                                            &feed_url,
                                            mode,
                                            max_uploads,
                                            &mut ctx,
                                            &mut channels,
                                            &mut had_hard_error,
                                        );
                                    }
                                }
                                None => {
                                    warn!("could not resolve channel ID for YouTube handle {handle}");
                                    had_hard_error = true;
                                }
                            }
                        }
                    }
                }
            }

            // 4. Distinguish between hard errors (all targets failed or invalid) vs legitimate empty (0 channels live)
            if !targets.is_empty() && channels.is_empty() && had_hard_error {
                return Err(format!(
                    "configured YouTube targets ({}) failed to resolve: invalid target configuration or Invidious instances unreachable",
                    targets.len()
                ));
            }

            info!("YouTube input '{}' finished successfully: {} channel(s) available", req.input_name, channels.len());
            Ok(channels)
        }

        fn fetch_epg(_req: &InputRequest) -> Result<Vec<(EpgChannel, Vec<EpgProgramme>)>, String> { Ok(Vec::new()) }
    }

    fn make_channel(
        video_id: &str,
        title: &str,
        group: &str,
        logo: &str,
        resolved: &ResolvedStream,
        input_name: &str,
    ) -> Channel {
        let item_type = if resolved.is_live { ItemType::Live } else { ItemType::Video };
        let cluster = if resolved.is_live { Cluster::Live } else { Cluster::Video };

        Channel {
            handle: 0,
            name: title.to_string(),
            title: title.to_string(),
            group: group.to_string(),
            url: resolved.url.clone(),
            epg_id: None,
            logo: logo.to_string(),
            chno: None,
            item_type,
            cluster,
            input_name: input_name.to_string(),
            input_stream_id: video_id.to_string(),
            extra: Vec::new(),
        }
    }

    fn discover_and_resolve_live(
        live_page_url: &str,
        group_fallback: &str,
        ctx: &mut FetchCtx<'_>,
        channels: &mut Vec<Channel>,
    ) -> Result<(), String> {
        let now = clock::now_unix();
        if now >= ctx.deadline - 2 {
            return Ok(());
        }

        // The page request must leave room for the stream resolution that follows it.
        let remaining_secs = (ctx.deadline - now).max(1) as u64;
        let resp = http::Request::get(live_page_url)
            .header("User-Agent", BROWSER_UA)
            .header("Accept-Language", "en-US,en;q=0.9")
            .timeout(Duration::from_secs(remaining_secs.clamp(2, 4)))
            .send()
            .map_err(|e| format!("request error: {e}"))?;

        if !resp.is_success() {
            debug!("{live_page_url} returned status {}", resp.status());
            return Ok(());
        }

        let html = resp.text().map_err(|e| format!("text error: {e}"))?;
        let Some((video_id, live_title_opt)) = extract_live_info_from_html(&html) else {
            debug!("no active live video ID found on {live_page_url}");
            return Ok(());
        };

        match resolver::resolve_stream(&video_id, ctx.instances, ctx.active_cursor, ctx.cache_ttl_minutes, ctx.deadline)
        {
            Ok(resolved) => {
                if resolved.is_live {
                    let fallback_title = format!("{group_fallback} Live");
                    let channel_title = live_title_opt.as_deref().unwrap_or(&fallback_title);
                    channels.push(make_channel(
                        &video_id,
                        channel_title,
                        group_fallback,
                        &format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"),
                        &resolved,
                        ctx.input_name,
                    ));
                }
            }
            Err(e) => {
                warn!("failed to resolve live video {video_id}: {e}");
                return Err(e);
            }
        }

        Ok(())
    }

    fn fetch_and_append_feed(
        feed_url: &str,
        mode: &str,
        max_uploads: u32,
        ctx: &mut FetchCtx<'_>,
        channels: &mut Vec<Channel>,
        had_hard_error: &mut bool,
    ) {
        let now = clock::now_unix();
        if now >= ctx.deadline - 2 {
            return;
        }

        debug!("fetching YouTube feed from {feed_url}");
        let remaining_secs = (ctx.deadline - now).max(1) as u64;
        let resp = match http::Request::get(feed_url)
            .header("User-Agent", BROWSER_UA)
            .timeout(Duration::from_secs(remaining_secs.clamp(2, 4)))
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                warn!("HTTP error fetching feed {feed_url}: {e}");
                *had_hard_error = true;
                return;
            }
        };

        if !resp.is_success() {
            warn!("feed {feed_url} returned status {}", resp.status());
            *had_hard_error = true;
            return;
        }

        let xml_text = match resp.text() {
            Ok(t) => t,
            Err(e) => {
                warn!("could not read feed text from {feed_url}: {e}");
                *had_hard_error = true;
                return;
            }
        };

        let (feed_title, videos) = match parse_atom_feed(&xml_text, max_uploads as usize) {
            Ok(res) => res,
            Err(e) => {
                warn!("failed to parse Atom feed from {feed_url}: {e}");
                *had_hard_error = true;
                return;
            }
        };

        for video in videos {
            if clock::now_unix() >= ctx.deadline - 2 {
                warn!("deadline budget limit reached, stopping feed resolution early");
                break;
            }

            match resolver::resolve_stream(
                &video.video_id,
                ctx.instances,
                ctx.active_cursor,
                ctx.cache_ttl_minutes,
                ctx.deadline,
            ) {
                Ok(resolved) => {
                    if mode == "live_only" && !resolved.is_live {
                        continue;
                    }

                    let group_name = if !video.channel_title.is_empty() {
                        &video.channel_title
                    } else if !feed_title.is_empty() {
                        &feed_title
                    } else {
                        "YouTube"
                    };

                    channels.push(make_channel(
                        &video.video_id,
                        &video.title,
                        group_name,
                        &video.thumbnail_url,
                        &resolved,
                        ctx.input_name,
                    ));
                }
                Err(err) => {
                    warn!("failed to resolve stream for video {}: {err}", video.video_id);
                    *had_hard_error = true;
                }
            }
        }
    }

    fn resolve_handle_via_youtube_html(encoded_handle: &str) -> Option<String> {
        let url = format!("https://www.youtube.com/{encoded_handle}");

        let resp = http::Request::get(&url)
            .header("User-Agent", BROWSER_UA)
            .header("Accept-Language", "en-US,en;q=0.9")
            .timeout(Duration::from_secs(4))
            .send()
            .ok()?;

        if !resp.is_success() {
            return None;
        }

        let html = resp.text().ok()?;
        extract_channel_id_from_html(&html)
    }

    fn resolve_handle_via_invidious(encoded_handle: &str, instances: &[String]) -> Option<String> {
        for instance in instances {
            let clean_instance = instance.trim_end_matches('/');
            let endpoint = format!("{clean_instance}/api/v1/channels/{encoded_handle}");

            let resp = match http::Request::get(&endpoint)
                .header("User-Agent", BROWSER_UA)
                .timeout(Duration::from_secs(3))
                .send()
            {
                Ok(r) => r,
                Err(_) => continue,
            };

            if !resp.is_success() {
                continue;
            }

            if let Ok(json_val) = resp.json::<serde_json::Value>() {
                if let Some(author_id) = json_val.get("authorId").and_then(|v| v.as_str()) {
                    return Some(author_id.to_string());
                }
            }
        }
        None
    }

    fn url_encode_handle(handle: &str) -> String {
        let mut encoded = String::with_capacity(handle.len() + 4);
        for b in handle.bytes() {
            match b {
                b'@' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    encoded.push(b as char);
                }
                _ => {
                    encoded.push_str(&format!("%{b:02X}"));
                }
            }
        }
        encoded
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use tuliprox_plugin_sdk::{http::Method, mock};

    // `Settings` is kept in one process-wide slot, so tests that install settings must not overlap; the mock host
    // itself is thread-local and needs no guard.
    static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

    fn lock_settings() -> MutexGuard<'static, ()> { SETTINGS_LOCK.lock().unwrap_or_else(PoisonError::into_inner) }

    fn request(options: &[(&str, &str)]) -> tuliprox_plugin_sdk::wit::input_provider::InputRequest {
        tuliprox_plugin_sdk::wit::input_provider::InputRequest {
            input_name: "feed".to_string(),
            url: String::new(),
            username: None,
            password: None,
            options: options.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
        }
    }

    #[test]
    fn input_options_override_every_setting() {
        let overrides = plugin::input_overrides(&request(&[
            ("mode", "live_only"),
            ("max_uploads", "5"),
            ("cache_ttl_minutes", "30"),
            ("invidious_instances", r#"["https://a.test","https://b.test"]"#),
            ("unknown", "ignored"),
        ]));
        assert_eq!(overrides.mode.as_deref(), Some("live_only"));
        assert_eq!(overrides.max_uploads, Some(5));
        assert_eq!(overrides.cache_ttl_minutes, Some(30));
        assert_eq!(
            overrides.invidious_instances.as_deref(),
            Some(["https://a.test".to_string(), "https://b.test".to_string()].as_slice())
        );
    }

    #[test]
    fn no_options_means_no_overrides_and_a_plain_list_is_accepted() {
        assert_eq!(plugin::input_overrides(&request(&[])), plugin::InputOverrides::default(), "settings win");

        let overrides =
            plugin::input_overrides(&request(&[("invidious_instances", " https://a.test , https://b.test ")]));
        assert_eq!(
            overrides.invidious_instances.as_deref(),
            Some(["https://a.test".to_string(), "https://b.test".to_string()].as_slice())
        );

        // An empty or unusable value must not silently disable the plugin's own list.
        let empty = plugin::input_overrides(&request(&[("invidious_instances", "  "), ("mode", "")]));
        assert_eq!(empty, plugin::InputOverrides::default());

        let bad_number = plugin::input_overrides(&request(&[("max_uploads", "many")]));
        assert_eq!(bad_number.max_uploads, None, "a typo falls back to the plugin setting");
    }

    #[test]
    fn fetch_playlist_live_only_filters_out_vod_from_playlist() {
        let _settings = lock_settings();
        mock::reset();
        mock::set_now(1_000);

        mock::set_config(
            r#"{
            "mode": "live_only",
            "invidious_instances": ["https://inv.test"]
        }"#,
        );
        assert!(plugin::Settings::reload().is_ok());

        let feed_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:yt="http://www.youtube.com/xml/schemas/2015">
  <title>News Live &amp; VOD Playlist</title>
  <entry>
    <yt:videoId>vod12345678</yt:videoId>
    <title>Yesterday News VOD</title>
  </entry>
  <entry>
    <yt:videoId>live1234567</yt:videoId>
    <title>24/7 Live Stream</title>
  </entry>
</feed>"#;

        mock::http_respond(
            Method::Get,
            "https://www.youtube.com/feeds/videos.xml?playlist_id=PLnews12345678",
            200,
            feed_xml,
        );

        mock::http_respond(
            Method::Get,
            "https://inv.test/api/v1/videos/vod12345678?local=true",
            200,
            r#"{"liveNow":false,"hlsUrl":"","formatStreams":[{"url":"https://inv.test/vod.mp4","container":"mp4"}]}"#,
        );

        mock::http_respond(
            Method::Get,
            "https://inv.test/api/v1/videos/live1234567?local=true",
            200,
            r#"{"liveNow":true,"hlsUrl":"/manifest/live.m3u8"}"#,
        );

        let req = InputRequest {
            input_name: "test-playlist".to_string(),
            url: "https://www.youtube.com/playlist?list=PLnews12345678".to_string(),
            username: None,
            password: None,
            options: Vec::new(),
        };

        let channels = plugin::YoutubeInputPlugin::fetch_playlist(&req).expect("playlist fetch should succeed");

        assert_eq!(channels.len(), 1, "VOD video must be filtered out in live_only mode");
        let live_ch = &channels[0];
        assert_eq!(live_ch.title, "24/7 Live Stream");
        assert_eq!(live_ch.item_type, ItemType::Live);
        assert_eq!(live_ch.cluster, Cluster::Live);
        assert_eq!(live_ch.url, "https://inv.test/manifest/live.m3u8?local=true");
    }

    #[test]
    fn an_input_uses_its_own_invidious_instances() {
        let _settings = lock_settings();
        mock::reset();
        mock::set_now(1_000);

        // The plugin setting names a mirror this input must not use.
        mock::set_config(r#"{"invidious_instances": ["https://settings.test"]}"#);
        assert!(plugin::Settings::reload().is_ok());

        let video = "dQw4w9WgXcQ";
        mock::http_respond(
            Method::Get,
            "https://www.youtube.com/feeds/videos.xml?channel_id=UC5NOEUbkLheQcaaRldYW5GA",
            200,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:yt="http://www.youtube.com/xml/schemas/2015">
  <title>Kanal</title>
  <entry><yt:videoId>{video}</yt:videoId><title>Eine Folge</title></entry>
</feed>"#
            ),
        );
        mock::http_respond(
            Method::Get,
            &format!("https://input.test/api/v1/videos/{video}?local=true"),
            200,
            r#"{"liveNow":false,"hlsUrl":"","formatStreams":[{"url":"https://input.test/v.mp4","container":"mp4"}]}"#,
        );

        let mut req = InputRequest {
            input_name: "with-overrides".to_string(),
            url: "UC5NOEUbkLheQcaaRldYW5GA".to_string(),
            username: None,
            password: None,
            options: vec![("invidious_instances".to_string(), r#"["https://input.test"]"#.to_string())],
        };
        let channels = plugin::YoutubeInputPlugin::fetch_playlist(&req).expect("fetch");
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].url, "https://input.test/v.mp4", "the input's mirror resolved it");

        // Without the option the plugin setting is used again.
        req.options.clear();
        let err = plugin::YoutubeInputPlugin::fetch_playlist(&req).expect_err("settings.test is not mocked");
        assert!(err.contains("settings.test") || err.contains("instances"), "{err}");
    }

    #[test]
    fn an_input_lists_its_targets_and_its_instances_in_its_own_config() {
        let _settings = lock_settings();
        mock::reset();
        mock::set_now(1_000);

        // No settings at all: everything the fetch needs comes from the input.
        mock::set_config("{}");
        assert!(plugin::Settings::reload().is_ok());

        let feed = |id: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:yt="http://www.youtube.com/xml/schemas/2015">
  <title>Kanal</title>
  <entry><yt:videoId>{id}</yt:videoId><title>Folge {id}</title></entry>
</feed>"#
            )
        };
        mock::http_respond(
            Method::Get,
            "https://www.youtube.com/feeds/videos.xml?channel_id=UC5NOEUbkLheQcaaRldYW5GA",
            200,
            feed("aaaaaaaaaaa"),
        );
        mock::http_respond(
            Method::Get,
            "https://www.youtube.com/feeds/videos.xml?channel_id=UC1111111111111111111111",
            200,
            feed("bbbbbbbbbbb"),
        );
        for (id, name) in [("aaaaaaaaaaa", "a"), ("bbbbbbbbbbb", "b")] {
            mock::http_respond(
                Method::Get,
                &format!("https://input.test/api/v1/videos/{id}?local=true"),
                200,
                format!(
                    r#"{{"liveNow":false,"hlsUrl":"","formatStreams":[{{"url":"https://input.test/{name}.mp4","container":"mp4"}}]}}"#
                ),
            );
        }

        // One target in `url`, one in the input's own `channels` list; the second is a duplicate.
        let req = InputRequest {
            input_name: "two-targets".to_string(),
            url: "UC5NOEUbkLheQcaaRldYW5GA".to_string(),
            username: None,
            password: None,
            options: vec![
                (
                    "channels".to_string(),
                    r#"["UC1111111111111111111111","https://www.youtube.com/channel/UC5NOEUbkLheQcaaRldYW5GA"]"#
                        .to_string(),
                ),
                ("invidious_instances".to_string(), r#"["https://input.test"]"#.to_string()),
            ],
        };

        let channels = plugin::YoutubeInputPlugin::fetch_playlist(&req).expect("both targets fetch");
        assert_eq!(channels.len(), 2, "one channel per target, the duplicate dropped once");
        assert_eq!(channels[0].url, "https://input.test/a.mp4");
        assert_eq!(channels[1].url, "https://input.test/b.mp4");
    }

    #[test]
    fn an_input_without_instances_names_the_missing_field() {
        let _settings = lock_settings();
        mock::reset();
        mock::set_config("{}");
        assert!(plugin::Settings::reload().is_ok());

        let req = InputRequest {
            input_name: "no-instance".to_string(),
            url: "@tagesschau".to_string(),
            username: None,
            password: None,
            options: Vec::new(),
        };
        let err = plugin::YoutubeInputPlugin::fetch_playlist(&req).unwrap_err();
        assert!(err.contains("Invidious Instances"), "{err}");
        assert!(err.contains("no-instance"), "{err}");
    }

    #[test]
    fn fetch_playlist_without_targets_is_an_error() {
        let _settings = lock_settings();
        mock::reset();
        mock::set_now(1_000);

        mock::set_config(r#"{"invidious_instances": ["https://inv.test"]}"#);
        assert!(plugin::Settings::reload().is_ok());

        let req = InputRequest {
            input_name: "test-empty".to_string(),
            url: String::new(),
            username: None,
            password: None,
            options: Vec::new(),
        };

        let err = plugin::YoutubeInputPlugin::fetch_playlist(&req).unwrap_err();
        assert!(err.contains("has no target"), "the error points at the input: {err}");
        assert!(err.contains("test-empty"), "and names it: {err}");
    }

    #[test]
    fn fetch_playlist_live_only_channel_id_path() {
        let _settings = lock_settings();
        mock::reset();
        mock::set_now(1_000);

        mock::set_config(
            r#"{
            "mode": "live_only",
            "invidious_instances": ["https://inv.test"]
        }"#,
        );
        assert!(plugin::Settings::reload().is_ok());

        let live_html = r#"<html><head><title>DW News livestream - YouTube</title><link rel="canonical" href="https://www.youtube.com/watch?v=LuKwFajn37U"></head></html>"#;
        mock::http_respond(
            Method::Get,
            "https://www.youtube.com/channel/UCknLrEdhRCp1aegoMqRaCZg/live",
            200,
            live_html,
        );
        mock::http_respond(
            Method::Get,
            "https://inv.test/api/v1/videos/LuKwFajn37U?local=true",
            200,
            r#"{"liveNow":true,"hlsUrl":"/manifest/live.m3u8"}"#,
        );

        let req = InputRequest {
            input_name: "test-channel".to_string(),
            url: "https://www.youtube.com/channel/UCknLrEdhRCp1aegoMqRaCZg".to_string(),
            username: None,
            password: None,
            options: Vec::new(),
        };

        let channels = plugin::YoutubeInputPlugin::fetch_playlist(&req).expect("channel live fetch should succeed");
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].title, "DW News livestream");
        assert_eq!(channels[0].group, "Channel UCknLrEdhRCp1aegoMqRaCZg");
        assert_eq!(channels[0].item_type, ItemType::Live);
    }

    #[test]
    fn settings_channels_accept_comma_separated_targets() {
        let _settings = lock_settings();
        mock::reset();
        mock::set_now(1_000);

        mock::set_config(
            r#"{
            "mode": "live_only",
            "invidious_instances": ["https://inv.test"],
            "channels": ["@AAA,@BBB"]
        }"#,
        );
        assert!(plugin::Settings::reload().is_ok());

        let live_html = r#"<html><head><title>Channel AAA Live - YouTube</title><link rel="canonical" href="https://www.youtube.com/watch?v=aaaaaaaaaaa"></head></html>"#;
        mock::http_respond(Method::Get, "https://www.youtube.com/@AAA/live", 200, live_html);
        mock::http_respond(Method::Get, "https://www.youtube.com/@BBB/live", 200, "<html>not live</html>");
        mock::http_respond(
            Method::Get,
            "https://inv.test/api/v1/videos/aaaaaaaaaaa?local=true",
            200,
            r#"{"liveNow":true,"hlsUrl":"/manifest/live.m3u8"}"#,
        );

        let req = InputRequest {
            input_name: "test-comma".to_string(),
            url: String::new(),
            username: None,
            password: None,
            options: Vec::new(),
        };

        let channels = plugin::YoutubeInputPlugin::fetch_playlist(&req).expect("both targets should be consulted");
        assert_eq!(channels.len(), 1, "only the live channel is kept");
        assert_eq!(channels[0].title, "Channel AAA Live");
    }

    #[test]
    fn fetch_playlist_invalid_targets_yield_hard_error() {
        let _settings = lock_settings();
        mock::reset();
        mock::set_now(1_000);

        mock::set_config(
            r#"{
            "invidious_instances": ["https://inv.test"]
        }"#,
        );
        assert!(plugin::Settings::reload().is_ok());

        let req = InputRequest {
            input_name: "test-invalid".to_string(),
            url: "https://invalid-domain.com/something".to_string(),
            username: None,
            password: None,
            options: Vec::new(),
        };

        let result = plugin::YoutubeInputPlugin::fetch_playlist(&req);
        assert!(result.is_err(), "all-invalid targets must return Err");
        let err = result.unwrap_err();
        assert!(err.contains("failed to resolve"), "error message should mention failure to resolve: {err}");
    }

    #[test]
    fn fetch_playlist_live_only_uses_live_title_from_html() {
        let _settings = lock_settings();
        mock::reset();
        mock::set_now(1_000);

        mock::set_config(
            r#"{
            "mode": "live_only",
            "invidious_instances": ["https://inv.test"]
        }"#,
        );
        assert!(plugin::Settings::reload().is_ok());

        let live_html = r#"<html><head><title>Sky News Live - YouTube</title><link rel="canonical" href="https://www.youtube.com/watch?v=LuKwFajn37U"></head></html>"#;
        mock::http_respond(Method::Get, "https://www.youtube.com/@SkyNews/live", 200, live_html);

        mock::http_respond(
            Method::Get,
            "https://inv.test/api/v1/videos/LuKwFajn37U?local=true",
            200,
            r#"{"liveNow":true,"hlsUrl":"/manifest/live.m3u8"}"#,
        );

        let req = InputRequest {
            input_name: "test-live".to_string(),
            url: "@SkyNews/live".to_string(),
            username: None,
            password: None,
            options: Vec::new(),
        };

        let channels = plugin::YoutubeInputPlugin::fetch_playlist(&req).expect("live fetch should succeed");
        assert_eq!(channels.len(), 1);
        let ch = &channels[0];
        assert_eq!(ch.title, "Sky News Live");
        assert_eq!(ch.item_type, ItemType::Live);
        assert_eq!(ch.cluster, Cluster::Live);
    }
}
