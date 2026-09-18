use crate::TestkitError;
use std::collections::HashMap;

/// Maps a stable synthetic-origin marker to the virtual playback URL published by Tuliprox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualIdMap {
    urls: HashMap<u32, String>,
    named_urls: HashMap<String, String>,
}

impl VirtualIdMap {
    pub fn from_m3u(document: &str) -> Result<Self, TestkitError> {
        let mut urls = HashMap::new();
        let mut named_urls = HashMap::new();
        let mut pending_marker = None;
        let mut pending_name = None;
        for raw_line in document.lines() {
            let line = raw_line.trim();
            if line.starts_with("#EXTINF:") {
                pending_marker = marker_from_extinf(line);
                pending_name = name_from_extinf(line);
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(marker) = pending_marker.take() {
                if urls.insert(marker, line.to_owned()).is_some() {
                    return Err(TestkitError::Configuration(format!("duplicate origin marker {marker} in playlist")));
                }
            }
            if let Some(name) = pending_name.take() {
                named_urls.insert(name, line.to_owned());
            }
        }
        if urls.is_empty() && named_urls.is_empty() {
            return Err(TestkitError::Configuration("playlist has no testkit marker entries".to_owned()));
        }
        Ok(Self { urls, named_urls })
    }

    pub fn playback_url(&self, marker: u32) -> Result<&str, TestkitError> {
        self.urls
            .get(&marker)
            .map(String::as_str)
            .ok_or_else(|| TestkitError::Configuration(format!("origin marker {marker} was not published by Tuliprox")))
    }

    pub fn named_playback_url(&self, name: &str) -> Result<&str, TestkitError> {
        self.named_urls
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| TestkitError::Configuration(format!("named media {name} was not published by Tuliprox")))
    }

    /// Extract the numeric virtual ID from the URL stored for `marker`. The virtual ID is the last
    /// path segment after stripping any query string and file extension. Returns an error if the
    /// segment cannot be parsed as a `u64`.
    pub fn virtual_id_from_marker(&self, marker: u32) -> Result<u64, TestkitError> {
        let url = self.playback_url(marker)?;
        extract_virtual_id_from_url(url)
    }
}

fn marker_from_extinf(line: &str) -> Option<u32> {
    let value = line.split("tvg-id=\"").nth(1)?.split('"').next()?;
    value.strip_prefix("test-")?.parse().ok()
}

fn name_from_extinf(line: &str) -> Option<String> {
    let value = line.split("tvg-id=\"").nth(1)?.split('"').next()?;
    value.strip_prefix("test-vod-").map(str::to_owned)
}

/// Extract the numeric virtual ID from the last path segment of a URL.
/// Query strings and file extensions (e.g. `.ts`, `.m3u8`) are stripped before parsing.
fn extract_virtual_id_from_url(url: &str) -> Result<u64, TestkitError> {
    // Drop query string
    let path = url.split('?').next().unwrap_or(url);
    // Drop file extension: find the last `.` in the last path segment only
    let path = {
        let last_slash = path.rfind('/').map_or(0, |i| i + 1);
        if let Some(dot_pos) = path[last_slash..].rfind('.') {
            &path[..last_slash + dot_pos]
        } else {
            path
        }
    };
    let segment = path.rsplit('/').next().unwrap_or("");
    segment.parse::<u64>().map_err(|_| {
        TestkitError::Configuration(format!(
            "cannot extract numeric virtual ID from URL {url}: last path segment {segment:?} is not a valid u64"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_virtual_playback_urls_without_assuming_their_ids() {
        let playlist = "#EXTM3U\n#EXTINF:-1 tvg-id=\"test-17\",News\nhttp://tuliprox/m3u/abc/17\n#EXTINF:-1 tvg-id=\"test-vod-movie.mkv\" type=\"movie\",Movie\nhttp://tuliprox/movie/abc/def/1.mkv\n";
        let map = VirtualIdMap::from_m3u(playlist).unwrap();
        assert_eq!(map.playback_url(17).unwrap(), "http://tuliprox/m3u/abc/17");
        assert_eq!(map.named_playback_url("movie.mkv").unwrap(), "http://tuliprox/movie/abc/def/1.mkv");
    }

    #[test]
    fn extracts_virtual_id_from_plain_m3u_url() {
        let map = VirtualIdMap::from_m3u("#EXTM3U\n#EXTINF:-1 tvg-id=\"test-17\",News\nhttp://tuliprox/m3u/abc/42\n")
            .unwrap();
        assert_eq!(map.virtual_id_from_marker(17).unwrap(), 42);
    }

    #[test]
    fn strips_query_string_before_extracting_virtual_id() {
        let map = VirtualIdMap::from_m3u(
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"test-5\",Ch5\nhttp://tuliprox/m3u/user/99?token=abc\n",
        )
        .unwrap();
        assert_eq!(map.virtual_id_from_marker(5).unwrap(), 99);
    }

    #[test]
    fn strips_file_extension_before_extracting_virtual_id() {
        let map = VirtualIdMap::from_m3u("#EXTM3U\n#EXTINF:-1 tvg-id=\"test-3\",Ch3\nhttp://tuliprox/m3u/user/77.ts\n")
            .unwrap();
        assert_eq!(map.virtual_id_from_marker(3).unwrap(), 77);
    }

    #[test]
    fn non_numeric_last_segment_is_an_error() {
        let map = VirtualIdMap::from_m3u("#EXTM3U\n#EXTINF:-1 tvg-id=\"test-1\",Ch1\nhttp://tuliprox/m3u/user/abc\n")
            .unwrap();
        assert!(map.virtual_id_from_marker(1).is_err());
    }
}
