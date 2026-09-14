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
}

fn marker_from_extinf(line: &str) -> Option<u32> {
    let value = line.split("tvg-id=\"").nth(1)?.split('"').next()?;
    value.strip_prefix("test-")?.parse().ok()
}

fn name_from_extinf(line: &str) -> Option<String> {
    let value = line.split("tvg-id=\"").nth(1)?.split('"').next()?;
    value.strip_prefix("test-vod-").map(str::to_owned)
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
}
