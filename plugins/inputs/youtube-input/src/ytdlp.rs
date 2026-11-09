use crate::{
    cache::{self, CachedStream},
    resolver::ResolvedStream,
};
use std::collections::HashMap;
use tuliprox_plugin_sdk::{clock, prelude::*, process::Command};

pub const DEFAULT_PATHS: [&str; 2] = ["/app/bin/yt-dlp", "yt-dlp"];

/// yt-dlp stream URLs are signed and expire after about six hours; a cached one must be renewed before that.
const MAX_URL_CACHE_MINUTES: u32 = 300;

/// Formats the error message when yt-dlp cannot be found across all candidate paths.
pub fn not_found_error(tried: &[String]) -> String {
    format!(
        "yt-dlp not found (tried {}); install yt-dlp or mount it into /app/bin/yt-dlp; see the youtube-input documentation",
        tried.join(", ")
    )
}

/// Discovers a runnable yt-dlp binary by testing candidates with `--version`.
///
/// If `custom_path` is provided (non-empty), only that path is tested.
/// Otherwise, `/app/bin/yt-dlp` and `yt-dlp` on PATH are tested in order.
pub fn find_ytdlp(custom_path: Option<&str>) -> Result<(String, String), Vec<String>> {
    let candidates: Vec<String> = match custom_path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => vec![path.to_string()],
        None => DEFAULT_PATHS.iter().map(|&s| s.to_string()).collect(),
    };

    for candidate in &candidates {
        let output = match Command::new(candidate).arg("--version").timeout_secs(2).run() {
            Ok(out) => out,
            Err(err) => {
                debug!("yt-dlp candidate '{candidate}' failed to start: {err}");
                continue;
            }
        };

        if output.success() {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !version.is_empty() {
                debug!("found yt-dlp at '{candidate}', version {version}");
                return Ok((candidate.clone(), version));
            }
        }
    }

    Err(candidates)
}

/// Resolves playable stream URLs for a batch of videos using `yt-dlp`.
///
/// Cache hits are retrieved from the KV store first. Missing videos are resolved in
/// a single batch execution with `--no-warnings --no-playlist --print`.
pub fn resolve_streams(
    program: &str,
    video_ids: &[&str],
    ttl_minutes: u32,
    deadline: i64,
) -> Result<HashMap<String, ResolvedStream>, String> {
    let valid_instances = ["yt-dlp".to_string()];
    let mut results = HashMap::new();
    let mut missing_ids = Vec::new();

    for &id in video_ids {
        if let Some(cached) = cache::get_stream(id, &valid_instances) {
            debug!("cache hit for yt-dlp video {id}");
            results.insert(id.to_string(), ResolvedStream { url: cached.url, is_live: cached.is_live });
        } else if !missing_ids.contains(&id) {
            missing_ids.push(id);
        }
    }

    if missing_ids.is_empty() {
        return Ok(results);
    }

    let now = clock::now_unix();
    if now >= deadline - 2 {
        return Err("time budget exhausted before running yt-dlp".to_string());
    }
    let timeout_secs = u32::try_from((deadline - now).saturating_sub(2).clamp(1, 300)).unwrap_or(5);

    let mut cmd = Command::new(program)
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("-f")
        // YouTube often offers only separate video and audio HLS renditions. Any HLS format carries the master
        // playlist (`manifest_url`) that holds both, so an HLS video-only format is the last resort.
        .arg("best[protocol^=m3u8]/best[ext=mp4][protocol^=http]/best/bestvideo*[protocol^=m3u8]")
        .arg("--print")
        .arg("%(id)s\t%(is_live)s\t%(url)s\t%(manifest_url)s")
        .timeout_secs(timeout_secs);

    for id in &missing_ids {
        cmd = cmd.arg(format!("https://www.youtube.com/watch?v={id}"));
    }

    let output = cmd.run().map_err(|e| format!("failed to execute yt-dlp: {e}"))?;
    // yt-dlp exits non-zero when any one video of the batch fails (private, removed, members only), so the
    // status alone says nothing about the others: read stdout first and fail only when nothing resolved.
    let first_error = || {
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        stderr_str.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("yt-dlp process failed").to_string()
    };
    let resolved_before = results.len();

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    for line in stdout_str.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let video_id = parts[0].trim();
        let is_live_str = parts[1].trim();
        // The HLS master playlist when there is one (audio and video together), else the format URL.
        let url = parts
            .get(3)
            .map(|manifest| manifest.trim())
            .filter(|manifest| manifest.starts_with("http"))
            .unwrap_or_else(|| parts[2].trim());
        if video_id.is_empty() || url.is_empty() {
            continue;
        }
        let is_live = is_live_str.eq_ignore_ascii_case("true");
        let stream = ResolvedStream { url: url.to_string(), is_live };
        let cached = CachedStream { url: stream.url.clone(), is_live: stream.is_live, instance: "yt-dlp".to_string() };
        if let Err(err) = cache::set_stream(video_id, &cached, ttl_minutes.min(MAX_URL_CACHE_MINUTES)) {
            warn!("failed to cache stream URL for {video_id}: {err}");
        }
        results.insert(video_id.to_string(), stream);
    }

    if !output.success() {
        if results.len() == resolved_before {
            return Err(first_error());
        }
        warn!("yt-dlp resolved only part of the batch: {}", first_error());
    }
    Ok(results)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::{mock, process};

    fn ok_process(stdout: &str) -> Result<process::Output, &'static str> {
        Ok(process::Output { status: Some(0), stdout: stdout.as_bytes().to_vec(), stderr: Vec::new() })
    }

    fn err_process(status: i32, stderr: &str) -> Result<process::Output, &'static str> {
        Ok(process::Output { status: Some(status), stdout: Vec::new(), stderr: stderr.as_bytes().to_vec() })
    }

    #[test]
    fn path_search_order_finds_default_app_bin() {
        mock::reset();
        mock::process_respond("/app/bin/yt-dlp", ok_process("2024.10.07\n"));

        let (path, version) = find_ytdlp(None).expect("finds yt-dlp");
        assert_eq!(path, "/app/bin/yt-dlp");
        assert_eq!(version, "2024.10.07");
    }

    #[test]
    fn path_search_order_falls_back_to_path() {
        mock::reset();
        // First candidate fails, second succeeds
        mock::process_respond("/app/bin/yt-dlp", Err("not found"));
        mock::process_respond("yt-dlp", ok_process("2024.11.01\n"));

        let (path, version) = find_ytdlp(None).expect("finds yt-dlp on PATH");
        assert_eq!(path, "yt-dlp");
        assert_eq!(version, "2024.11.01");
    }

    #[test]
    fn custom_ytdlp_path_is_tried_exclusively() {
        mock::reset();
        mock::process_respond("/custom/yt-dlp", ok_process("2024.12.01\n"));

        let (path, version) = find_ytdlp(Some("/custom/yt-dlp")).expect("finds custom yt-dlp");
        assert_eq!(path, "/custom/yt-dlp");
        assert_eq!(version, "2024.12.01");

        let runs = mock::process_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].program, "/custom/yt-dlp");
    }

    #[test]
    fn not_found_reports_tried_paths_and_docs() {
        mock::reset();
        mock::process_respond("/app/bin/yt-dlp", Err("no such file"));
        mock::process_respond("yt-dlp", Err("not found"));

        let tried = find_ytdlp(None).expect_err("should fail");
        assert_eq!(tried, vec!["/app/bin/yt-dlp".to_string(), "yt-dlp".to_string()]);
        let msg = not_found_error(&tried);
        assert!(msg.contains("yt-dlp not found (tried /app/bin/yt-dlp, yt-dlp)"), "{msg}");
        assert!(msg.contains("see the youtube-input documentation"), "{msg}");
    }

    #[test]
    fn batch_parse_with_valid_and_malformed_lines() {
        mock::reset();
        mock::set_now(1_000);

        let stdout = "\
vid1\tFalse\thttps://stream.example/vid1.mp4\n\
malformed line with no tabs\n\
vid2\tTrue\thttps://stream.example/live.m3u8\n\
incomplete\tonly_two_parts\n\
\tFalse\thttps://missing_id.com\n\
";
        mock::process_respond("/app/bin/yt-dlp", ok_process(stdout));

        let results = resolve_streams("/app/bin/yt-dlp", &["vid1", "vid2"], 120, 1_030).expect("resolves");
        assert_eq!(results.len(), 2);
        assert_eq!(
            results.get("vid1"),
            Some(&ResolvedStream { url: "https://stream.example/vid1.mp4".to_string(), is_live: false })
        );
        assert_eq!(
            results.get("vid2"),
            Some(&ResolvedStream { url: "https://stream.example/live.m3u8".to_string(), is_live: true })
        );

        // Cached items are reused on subsequent call without running yt-dlp
        let cached_results = resolve_streams("/app/bin/yt-dlp", &["vid1"], 120, 1_030).expect("cached");
        assert_eq!(cached_results.len(), 1);
        assert_eq!(mock::process_runs().len(), 1); // no new process run
    }

    #[test]
    fn non_zero_status_returns_first_stderr_line() {
        mock::reset();
        mock::set_now(1_000);

        let stderr = "\n\nERROR: [youtube] vid1: Video unavailable\nTraceback (most recent call last):\n";
        mock::process_respond("yt-dlp", err_process(1, stderr));

        let err = resolve_streams("yt-dlp", &["vid1"], 120, 1_030).expect_err("fails");
        assert_eq!(err, "ERROR: [youtube] vid1: Video unavailable");
    }

    #[test]
    fn a_failed_video_does_not_discard_the_rest_of_the_batch() {
        mock::reset();
        mock::set_now(1_000);
        mock::process_respond(
            "yt-dlp",
            Ok(process::Output {
                status: Some(1),
                stdout: b"vid1\tFalse\thttps://stream.example/vid1.mp4\n".to_vec(),
                stderr: b"ERROR: [youtube] vid2: Private video\n".to_vec(),
            }),
        );
        let resolved = resolve_streams("yt-dlp", &["vid1", "vid2"], 120, 1_030).expect("vid1 resolves");
        assert_eq!(resolved.len(), 1);
        assert!(resolved.contains_key("vid1"));

        mock::process_respond("yt-dlp", err_process(1, "ERROR: [youtube] vid3: Private video\n"));
        let err = resolve_streams("yt-dlp", &["vid3"], 120, 1_030).expect_err("nothing resolved");
        assert_eq!(err, "ERROR: [youtube] vid3: Private video");
    }

    #[test]
    fn the_hls_master_playlist_wins_over_a_video_only_format_url() {
        mock::reset();
        mock::set_now(1_000);
        mock::process_respond(
            "yt-dlp",
            ok_process(concat!(
                "vid1\tFalse\thttps://video.example/only-video.m3u8\thttps://manifest.example/master.m3u8\n",
                "vid2\tFalse\thttps://stream.example/vid2.mp4\tNA\n",
            )),
        );
        let resolved = resolve_streams("yt-dlp", &["vid1", "vid2"], 120, 1_030).expect("both resolve");
        assert_eq!(resolved["vid1"].url, "https://manifest.example/master.m3u8");
        assert_eq!(resolved["vid2"].url, "https://stream.example/vid2.mp4");
    }
}
