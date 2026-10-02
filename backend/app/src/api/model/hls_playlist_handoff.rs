use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};
use tuliprox_core::model::ProviderBindingTag;

const HLS_PLAYLIST_HANDOFF_CAPACITY: usize = 1_000;
const HLS_PLAYLIST_HANDOFF_MAX_TTL: Duration = Duration::from_secs(3);

/// Media playlist downloaded on an HLS entry request, kept for the immediate variant request.
#[derive(Debug, Clone)]
pub struct HlsPlaylistHandoffEntry {
    /// Rewritten media playlist, already carrying sealed session tokens.
    pub content: String,
    /// Provider (account) the playlist was fetched with.
    pub provider: Arc<str>,
    /// Provider binding of the session when the playlist was fetched.
    pub binding_tag: Option<ProviderBindingTag>,
    expires_at: Instant,
}

impl HlsPlaylistHandoffEntry {
    pub fn matches(&self, provider: &str, binding_tag: Option<ProviderBindingTag>) -> bool {
        self.provider.as_ref() == provider && self.binding_tag == binding_tag
    }
}

/// Single-use hand-off of the entry playlist to the wrapped variant request, so channel start
/// does not fetch the same upstream playlist twice within milliseconds.
///
/// Entries are keyed by session token and sealed URL, consumed atomically by `take`, live at most
/// one target duration (capped at 3 s), and the number of sessions is bounded. Lookups borrow
/// their keys; expired entries are pruned on insert.
#[derive(Debug, Default)]
pub struct HlsPlaylistHandoffCache {
    sessions: Mutex<HashMap<String, Vec<(String, HlsPlaylistHandoffEntry)>>>,
}

impl HlsPlaylistHandoffCache {
    pub fn new() -> Self { Self::default() }

    pub fn insert(
        &self,
        session_token: &str,
        sealed_url: &str,
        content: String,
        provider: Arc<str>,
        binding_tag: Option<ProviderBindingTag>,
    ) {
        self.insert_at(session_token, sealed_url, content, provider, binding_tag, Instant::now());
    }

    fn insert_at(
        &self,
        session_token: &str,
        sealed_url: &str,
        content: String,
        provider: Arc<str>,
        binding_tag: Option<ProviderBindingTag>,
        now: Instant,
    ) {
        let entry = HlsPlaylistHandoffEntry { expires_at: now + handoff_ttl(&content), content, provider, binding_tag };
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|_, entries| {
            entries.retain(|(_, entry)| entry.expires_at > now);
            !entries.is_empty()
        });
        if let Some(entries) = sessions.get_mut(session_token) {
            entries.retain(|(url, _)| url != sealed_url);
            entries.push((sealed_url.to_string(), entry));
            return;
        }
        if sessions.len() >= HLS_PLAYLIST_HANDOFF_CAPACITY {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, entries)| entries.iter().map(|(_, entry)| entry.expires_at).min())
                .map(|(token, _)| token.clone());
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(session_token.to_string(), vec![(sealed_url.to_string(), entry)]);
    }

    /// Removes and returns the entry; an expired entry is dropped and yields `None`.
    pub fn take(&self, session_token: &str, sealed_url: &str) -> Option<HlsPlaylistHandoffEntry> {
        self.take_at(session_token, sealed_url, Instant::now())
    }

    fn take_at(&self, session_token: &str, sealed_url: &str, now: Instant) -> Option<HlsPlaylistHandoffEntry> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let entries = sessions.get_mut(session_token)?;
        let index = entries.iter().position(|(url, _)| url == sealed_url)?;
        let (_, entry) = entries.swap_remove(index);
        if entries.is_empty() {
            sessions.remove(session_token);
        }
        (entry.expires_at > now).then_some(entry)
    }

    /// Drops every entry of a session token, e.g. before a session is recreated from a token.
    pub fn remove_session(&self, session_token: &str) {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner).remove(session_token);
    }

    #[cfg(test)]
    fn len(&self) -> usize { self.sessions.lock().unwrap_or_else(PoisonError::into_inner).values().map(Vec::len).sum() }
}

fn handoff_ttl(content: &str) -> Duration {
    content
        .lines()
        .find_map(|line| line.trim().strip_prefix("#EXT-X-TARGETDURATION:"))
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map_or(HLS_PLAYLIST_HANDOFF_MAX_TTL, |secs| {
            // Cap before converting: `Duration::from_secs_f64` panics on values beyond `Duration::MAX`.
            Duration::from_secs_f64(secs.min(HLS_PLAYLIST_HANDOFF_MAX_TTL.as_secs_f64()))
        })
}

#[cfg(test)]
mod tests {
    use super::{HlsPlaylistHandoffCache, HLS_PLAYLIST_HANDOFF_CAPACITY};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    const PLAYLIST: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\nseg.ts";

    #[test]
    fn entry_is_single_use() {
        let cache = HlsPlaylistHandoffCache::new();
        cache.insert("s", "u", PLAYLIST.to_string(), Arc::from("p"), None);
        let entry = cache.take("s", "u").expect("first take hits");
        assert!(entry.matches("p", None));
        assert!(!entry.matches("other", None));
        assert!(cache.take("s", "u").is_none());
    }

    #[test]
    fn entry_is_keyed_by_session_and_sealed_url() {
        let cache = HlsPlaylistHandoffCache::new();
        cache.insert("s", "video", PLAYLIST.to_string(), Arc::from("p"), None);
        assert!(cache.take("s", "audio").is_none());
        assert!(cache.take("other", "video").is_none());
        assert!(cache.take("s", "video").is_some());
    }

    #[test]
    fn entry_expires_after_target_duration_capped_at_three_seconds() {
        let cache = HlsPlaylistHandoffCache::new();
        let now = Instant::now();
        cache.insert_at("s", "u", PLAYLIST.to_string(), Arc::from("p"), None, now);
        assert!(cache.take_at("s", "u", now + Duration::from_millis(2_100)).is_none());

        let long = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\nseg.ts";
        cache.insert_at("s", "u", long.to_string(), Arc::from("p"), None, now);
        assert!(cache.take_at("s", "u", now + Duration::from_millis(3_100)).is_none());
        cache.insert_at("s", "u", long.to_string(), Arc::from("p"), None, now);
        assert!(cache.take_at("s", "u", now + Duration::from_millis(2_900)).is_some());
    }

    #[test]
    fn oversized_target_duration_is_capped_without_panicking() {
        assert_eq!(super::handoff_ttl("#EXTM3U\n#EXT-X-TARGETDURATION:1e300\n"), super::HLS_PLAYLIST_HANDOFF_MAX_TTL);
    }

    #[test]
    fn remove_session_drops_all_entries_of_the_token() {
        let cache = HlsPlaylistHandoffCache::new();
        cache.insert("s", "video", PLAYLIST.to_string(), Arc::from("p"), None);
        cache.insert("s", "audio", PLAYLIST.to_string(), Arc::from("p"), None);
        cache.insert("t", "video", PLAYLIST.to_string(), Arc::from("p"), None);
        cache.remove_session("s");
        assert_eq!(cache.len(), 1);
        assert!(cache.take("t", "video").is_some());
    }

    #[test]
    fn cache_is_bounded() {
        let cache = HlsPlaylistHandoffCache::new();
        for index in 0..=HLS_PLAYLIST_HANDOFF_CAPACITY {
            cache.insert(&index.to_string(), "u", PLAYLIST.to_string(), Arc::from("p"), None);
        }
        assert_eq!(cache.len(), HLS_PLAYLIST_HANDOFF_CAPACITY);
    }

    #[test]
    fn parallel_takes_yield_exactly_one_hit() {
        let cache = Arc::new(HlsPlaylistHandoffCache::new());
        cache.insert("s", "u", PLAYLIST.to_string(), Arc::from("p"), None);
        let hits = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || cache.take("s", "u").is_some())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("thread joins"))
            .filter(|hit| *hit)
            .count();
        assert_eq!(hits, 1);
    }
}
