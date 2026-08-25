use url::Url;

pub const FLUSSONIC_HLS_LIVE_FILE: &str = "index.m3u8";
pub const FLUSSONIC_TS_LIVE_FILE: &str = "mpegts";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlussonicArchiveKind {
    Archive { start: String, duration: String, extension: &'static str },
    TimeshiftAbs { start: String, extension: &'static str },
    TimeshiftRel { ago: String, extension: &'static str },
}

impl FlussonicArchiveKind {
    pub fn discriminator(&self) -> String {
        match self {
            Self::Archive { start, duration, extension } => {
                format!("archive|{start}|{duration}|{extension}")
            }
            Self::TimeshiftAbs { start, extension } => format!("timeshift_abs|{start}|{extension}"),
            Self::TimeshiftRel { ago, extension } => format!("timeshift_rel|{ago}|{extension}"),
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            Self::Archive { extension, .. }
            | Self::TimeshiftAbs { extension, .. }
            | Self::TimeshiftRel { extension, .. } => extension,
        }
    }

    pub fn epg_reference_ts(&self) -> Option<i64> {
        match self {
            Self::Archive { start, .. } | Self::TimeshiftAbs { start, .. } => start.parse().ok(),
            Self::TimeshiftRel { .. } => None,
        }
    }
}

fn strip_media_suffix(value: &str) -> &str {
    value
        .strip_suffix(".m3u8")
        .or_else(|| value.strip_suffix(".M3U8"))
        .or_else(|| value.strip_suffix(".ts"))
        .or_else(|| value.strip_suffix(".TS"))
        .unwrap_or(value)
}

fn split_media_suffix(value: &str) -> (&str, &'static str) {
    if let Some(stem) = value.strip_suffix(".m3u8") {
        (stem, ".m3u8")
    } else if let Some(stem) = value.strip_suffix(".ts") {
        (stem, ".ts")
    } else {
        (value, ".m3u8")
    }
}

fn is_valid_start_duration(start: &str, duration: &str) -> bool {
    start.parse::<u64>().is_ok()
        && (duration.eq_ignore_ascii_case("now") || duration.parse::<u64>().is_ok())
}

fn split_start_duration(value: &str) -> Option<(&str, &str)> {
    let (start, duration) = value.split_once('-')?;
    if !is_valid_start_duration(start, duration) {
        return None;
    }
    Some((start, duration))
}

pub fn parse_flat_flussonic_archive(file: &str) -> Option<(u32, FlussonicArchiveKind)> {
    if file.contains('/')
        || !std::path::Path::new(file).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"))
    {
        return None;
    }
    let stem = strip_media_suffix(file.trim());
    let mut parts = stem.rsplitn(3, '-');
    let duration = parts.next()?;
    let start = parts.next()?;
    let virtual_id = parts.next()?.parse::<u32>().ok()?;
    if !is_valid_start_duration(start, duration) {
        return None;
    }
    Some((
        virtual_id,
        FlussonicArchiveKind::Archive {
            start: start.to_string(),
            duration: duration.to_string(),
            extension: ".m3u8",
        },
    ))
}

pub fn parse_flussonic_archive_file(file: &str) -> Option<FlussonicArchiveKind> {
    let file = file.trim();
    if file.is_empty() || file.contains('/') {
        return None;
    }
    let lower = file.to_ascii_lowercase();
    for prefix in ["archive-", "index-", "video-", "mono-"] {
        if let Some(value) = lower.strip_prefix(prefix) {
            let (value, extension) = split_media_suffix(value);
            let (start, duration) = split_start_duration(value)?;
            return Some(FlussonicArchiveKind::Archive {
                start: start.to_string(),
                duration: duration.to_string(),
                extension,
            });
        }
    }
    if let Some(value) = lower.strip_prefix("timeshift_abs-") {
        let (start, extension) = if let Some(start) = value.strip_suffix(".m3u8") {
            (start, ".m3u8")
        } else {
            (value.strip_suffix(".ts")?, ".ts")
        };
        start.parse::<u64>().ok()?;
        return Some(FlussonicArchiveKind::TimeshiftAbs { start: start.to_string(), extension });
    }
    if let Some(value) = lower.strip_prefix("timeshift_rel-") {
        let (ago, extension) = split_media_suffix(value);
        ago.parse::<u64>().ok()?;
        return Some(FlussonicArchiveKind::TimeshiftRel { ago: ago.to_string(), extension });
    }
    None
}

pub fn is_flussonic_live_file(file: &str) -> bool {
    ["index.m3u8", "video.m3u8", "mono.m3u8", "mpegts", "index.ts", "video.ts", "mono.ts"]
        .iter()
        .any(|supported_file| file.trim().eq_ignore_ascii_case(supported_file))
}

fn provider_playlist_stem(provider_url: &str) -> Option<&'static str> {
    let parsed = Url::parse(provider_url).ok()?;
    let file = parsed.path_segments()?.next_back()?;
    if file.eq_ignore_ascii_case("index.m3u8") || file.eq_ignore_ascii_case("index.ts") {
        Some("index")
    } else if file.eq_ignore_ascii_case("video.m3u8") || file.eq_ignore_ascii_case("video.ts") {
        Some("video")
    } else if file.eq_ignore_ascii_case("mono.m3u8") || file.eq_ignore_ascii_case("mono.ts") {
        Some("mono")
    } else {
        None
    }
}

pub fn build_provider_flussonic_archive_url(
    provider_url: &str,
    archive: &FlussonicArchiveKind,
) -> Option<String> {
    let new_file = match archive {
        FlussonicArchiveKind::Archive { start, duration, extension } => {
            format!("{}-{start}-{duration}{extension}", provider_playlist_stem(provider_url).unwrap_or("archive"))
        }
        FlussonicArchiveKind::TimeshiftAbs { start, extension } => {
            format!("timeshift_abs-{start}{extension}")
        }
        FlussonicArchiveKind::TimeshiftRel { ago, extension } => format!("timeshift_rel-{ago}{extension}"),
    };
    let mut parsed = Url::parse(provider_url).ok()?;
    parsed.path_segments_mut().ok()?.pop().push(&new_file);
    Some(parsed.into())
}

pub fn flussonic_proxy_live_file(player_mode: &str) -> &'static str {
    if player_mode.eq_ignore_ascii_case("flussonic-ts") {
        FLUSSONIC_TS_LIVE_FILE
    } else {
        FLUSSONIC_HLS_LIVE_FILE
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_provider_flussonic_archive_url, parse_flat_flussonic_archive, parse_flussonic_archive_file,
        FlussonicArchiveKind,
    };

    #[test]
    fn parses_flat_tivimate_hls_archive() -> Result<(), &'static str> {
        let (virtual_id, archive) = parse_flat_flussonic_archive("59-1784898000-3600.m3u8")
            .ok_or("flat archive path was not parsed")?;
        assert_eq!(virtual_id, 59);
        assert_eq!(
            archive,
            FlussonicArchiveKind::Archive {
                start: "1784898000".to_string(),
                duration: "3600".to_string(),
                extension: ".m3u8",
            }
        );
        assert!(parse_flat_flussonic_archive("59.m3u8").is_none());
        assert!(parse_flat_flussonic_archive("59-1784898000-3600.ts").is_none());

        let uppercase = parse_flat_flussonic_archive("59-1784898000-3600.M3U8")
            .ok_or("uppercase flat archive path was not parsed")?;
        assert_eq!(uppercase, (virtual_id, archive));
        Ok(())
    }

    #[test]
    fn archive_discriminator_includes_transport() {
        let hls = FlussonicArchiveKind::Archive {
            start: "1784898000".to_string(),
            duration: "3600".to_string(),
            extension: ".m3u8",
        };
        let ts = FlussonicArchiveKind::Archive {
            start: "1784898000".to_string(),
            duration: "3600".to_string(),
            extension: ".ts",
        };

        assert_ne!(hls.discriminator(), ts.discriminator());
    }

    #[test]
    fn parses_nested_hls_and_ts_archive_files() {
        for file in [
            "archive-1784898000-3600.m3u8",
            "index-1784898000-3600.m3u8",
            "video-1784898000-3600.m3u8",
            "mono-1784898000-now.m3u8",
        ] {
            assert!(matches!(
                parse_flussonic_archive_file(file),
                Some(FlussonicArchiveKind::Archive { .. })
            ));
        }
        assert_eq!(
            parse_flussonic_archive_file("timeshift_abs-1784898000.ts"),
            Some(FlussonicArchiveKind::TimeshiftAbs {
                start: "1784898000".to_string(),
                extension: ".ts",
            })
        );

        for file in ["archive-1784898000-3600.ts", "timeshift_rel-120.ts"] {
            let archive = parse_flussonic_archive_file(file).expect("TS archive path should parse");
            let url = build_provider_flussonic_archive_url("http://cdn.example/ch/index.ts", &archive)
                .expect("TS provider archive URL should build");
            assert!(std::path::Path::new(&url)
                 .extension()
                 .is_some_and(|ext| ext.eq_ignore_ascii_case("ts")), "requested TS extension was not preserved: {url}");
        }
    }

    #[test]
    fn rejects_malformed_native_archive_files() {
        for file in [
            "index.m3u8",
            "index-not-a-time-3600.m3u8",
            "archive-1784898000-invalid.m3u8",
            "timeshift_abs-x.ts",
            "timeshift_abs--1.ts",
            "timeshift_rel--120.m3u8",
            "../../archive-1784898000-3600.m3u8",
        ] {
            assert!(parse_flussonic_archive_file(file).is_none(), "accepted malformed file: {file}");
        }
    }

    #[test]
    fn identifies_only_supported_native_live_files() {
        for file in ["index.m3u8", "video.m3u8", "mono.m3u8", "mpegts", "index.ts"] {
            assert!(super::is_flussonic_live_file(file));
        }
        assert!(!super::is_flussonic_live_file("archive-1784898000-3600.m3u8"));
        assert!(!super::is_flussonic_live_file("segment.ts"));
    }

    #[test]
    fn builds_hls_and_ts_provider_archive_urls_without_losing_query() -> Result<(), &'static str> {
        let hls = build_provider_flussonic_archive_url(
            "http://cdn.example/ch/index.m3u8?token=abc",
            &FlussonicArchiveKind::Archive {
                start: "1784898000".to_string(),
                duration: "3600".to_string(),
                extension: ".m3u8",
            },
        )
        .ok_or("HLS provider archive URL was not built")?;
        assert_eq!(hls, "http://cdn.example/ch/index-1784898000-3600.m3u8?token=abc");

        let ts = build_provider_flussonic_archive_url(
            "http://cdn.example/ch/channel.ts?token=abc",
            &FlussonicArchiveKind::TimeshiftAbs {
                start: "1784898000".to_string(),
                extension: ".ts",
            },
        )
        .ok_or("TS provider archive URL was not built")?;
        assert_eq!(ts, "http://cdn.example/ch/timeshift_abs-1784898000.ts?token=abc");
        Ok(())
    }
}
