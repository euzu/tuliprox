use super::{HlsSession, SegmentCacheStatus};
use std::collections::{BTreeSet, HashMap};

/// Fetch priority for live HLS segment origin requests.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum SegmentFetchPriority {
    Demand,
    RenderWindow,
    Prefetch,
}

impl SegmentFetchPriority {
    pub const fn rank(self) -> u8 {
        match self {
            Self::Demand => 0,
            Self::RenderWindow => 1,
            Self::Prefetch => 2,
        }
    }

    pub const fn is_higher_priority_than(self, other: Self) -> bool { self.rank() < other.rank() }
}

/// Session-local deduplicating priority queue for segment fetch candidates.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SegmentPrefetchQueue {
    max_prefetch_depth: usize,
    demand: BTreeSet<u64>,
    render_window: BTreeSet<u64>,
    prefetch: BTreeSet<u64>,
    queued: HashMap<u64, SegmentFetchPriority>,
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct ManifestFetchQueueReport {
    pub render_window_queued: usize,
    pub prefetch_queued: usize,
    pub prefetch_skipped: usize,
}

impl SegmentPrefetchQueue {
    pub fn new(max_prefetch_depth: usize) -> Self {
        Self {
            max_prefetch_depth,
            demand: BTreeSet::new(),
            render_window: BTreeSet::new(),
            prefetch: BTreeSet::new(),
            queued: HashMap::new(),
        }
    }

    pub fn max_prefetch_depth(&self) -> usize { self.max_prefetch_depth }

    pub fn set_max_prefetch_depth(&mut self, max_prefetch_depth: usize) {
        self.max_prefetch_depth = max_prefetch_depth;
        while self.prefetch.len() > self.max_prefetch_depth {
            let Some(proxy_seq) = self.prefetch.pop_last() else {
                break;
            };
            self.queued.remove(&proxy_seq);
        }
    }

    pub fn enqueue(&mut self, proxy_seq: u64, priority: SegmentFetchPriority) -> bool {
        if priority == SegmentFetchPriority::Prefetch && !self.queued.contains_key(&proxy_seq) {
            let protected_prefetch_capacity = self.max_prefetch_depth;
            if self.prefetch.len() >= protected_prefetch_capacity {
                return false;
            }
        }

        match self.queued.get(&proxy_seq).copied() {
            Some(existing) if priority.is_higher_priority_than(existing) => {
                self.remove_from_priority(proxy_seq, existing);
                self.insert_into_priority(proxy_seq, priority);
                self.queued.insert(proxy_seq, priority);
                true
            }
            Some(_) => false,
            None => {
                self.insert_into_priority(proxy_seq, priority);
                self.queued.insert(proxy_seq, priority);
                true
            }
        }
    }

    pub fn pop_next(&mut self) -> Option<(u64, SegmentFetchPriority)> {
        if let Some(proxy_seq) = self.demand.pop_first() {
            self.queued.remove(&proxy_seq);
            return Some((proxy_seq, SegmentFetchPriority::Demand));
        }
        if let Some(proxy_seq) = self.render_window.pop_first() {
            self.queued.remove(&proxy_seq);
            return Some((proxy_seq, SegmentFetchPriority::RenderWindow));
        }
        if let Some(proxy_seq) = self.prefetch.pop_first() {
            self.queued.remove(&proxy_seq);
            return Some((proxy_seq, SegmentFetchPriority::Prefetch));
        }
        None
    }

    pub fn remove(&mut self, proxy_seq: u64) -> Option<SegmentFetchPriority> {
        let priority = self.queued.remove(&proxy_seq)?;
        self.remove_from_priority(proxy_seq, priority);
        Some(priority)
    }

    pub fn contains(&self, proxy_seq: u64) -> bool { self.queued.contains_key(&proxy_seq) }

    pub fn len(&self) -> usize { self.queued.len() }

    pub fn is_empty(&self) -> bool { self.queued.is_empty() }

    pub fn prefetch_len(&self) -> usize { self.prefetch.len() }

    pub fn proxy_seqs(&self) -> Vec<u64> { self.queued.keys().copied().collect() }

    fn insert_into_priority(&mut self, proxy_seq: u64, priority: SegmentFetchPriority) {
        match priority {
            SegmentFetchPriority::Demand => {
                self.demand.insert(proxy_seq);
            }
            SegmentFetchPriority::RenderWindow => {
                self.render_window.insert(proxy_seq);
            }
            SegmentFetchPriority::Prefetch => {
                self.prefetch.insert(proxy_seq);
            }
        }
    }

    fn remove_from_priority(&mut self, proxy_seq: u64, priority: SegmentFetchPriority) {
        match priority {
            SegmentFetchPriority::Demand => {
                self.demand.remove(&proxy_seq);
            }
            SegmentFetchPriority::RenderWindow => {
                self.render_window.remove(&proxy_seq);
            }
            SegmentFetchPriority::Prefetch => {
                self.prefetch.remove(&proxy_seq);
            }
        }
    }
}

impl Default for SegmentPrefetchQueue {
    fn default() -> Self { Self::new(6) }
}

impl HlsSession {
    pub fn queue_segment_fetch_candidate(
        &mut self,
        proxy_seq: u64,
        priority: SegmentFetchPriority,
        now_ms: u64,
    ) -> bool {
        if self.is_gc_marked_for_removal() {
            return false;
        }
        let Some(entry) = self.segments.get_mut(&proxy_seq) else {
            return false;
        };
        if entry.origin_fetch_ref.is_none() {
            return false;
        }

        match entry.status {
            SegmentCacheStatus::Discovered => {
                if self.segment_prefetch_queue.enqueue(proxy_seq, priority) {
                    entry.status = SegmentCacheStatus::Queued { priority, queued_at_ms: now_ms };
                    return true;
                }
                false
            }
            SegmentCacheStatus::Queued { priority: existing_priority, queued_at_ms } => {
                if priority.is_higher_priority_than(existing_priority) {
                    self.segment_prefetch_queue.enqueue(proxy_seq, priority);
                    entry.status = SegmentCacheStatus::Queued { priority, queued_at_ms };
                    return true;
                }
                false
            }
            SegmentCacheStatus::Fetching { .. }
            | SegmentCacheStatus::Ready { .. }
            | SegmentCacheStatus::FailedRetryable { .. }
            | SegmentCacheStatus::FailedPermanent { .. }
            | SegmentCacheStatus::Expired => false,
        }
    }

    pub fn queue_manifest_prefetch_candidates(&mut self, now_ms: u64) {
        let _ = self.queue_manifest_fetch_candidates(now_ms, true);
    }

    pub fn queue_manifest_fetch_candidates(&mut self, now_ms: u64, allow_prefetch: bool) -> ManifestFetchQueueReport {
        let mut report = ManifestFetchQueueReport::default();
        if self.is_gc_marked_for_removal() {
            return report;
        }
        let Some(tail_proxy_seq) = self.publishable_origin_tail_proxy_seq else {
            return report;
        };
        let known_sequences =
            self.segments.keys().copied().filter(|proxy_seq| *proxy_seq <= tail_proxy_seq).collect::<Vec<_>>();
        if known_sequences.is_empty() {
            return report;
        }

        let tail_index =
            known_sequences.len().saturating_sub(1).saturating_sub(self.render_policy.initial_render_gap_segments);
        let render_window_len = known_sequences.len().min(6).min(tail_index.saturating_add(1));
        let render_start_index = tail_index.saturating_add(1).saturating_sub(render_window_len);

        for proxy_seq in &known_sequences[render_start_index..=tail_index] {
            if self.queue_segment_fetch_candidate(*proxy_seq, SegmentFetchPriority::RenderWindow, now_ms) {
                report.render_window_queued = report.render_window_queued.saturating_add(1);
            }
        }

        let prefetch_start = tail_index.saturating_add(1);
        if !allow_prefetch {
            report.prefetch_skipped = known_sequences
                .iter()
                .skip(prefetch_start)
                .take(self.segment_prefetch_queue.max_prefetch_depth())
                .filter(|proxy_seq| {
                    self.segments
                        .get(proxy_seq)
                        .is_some_and(|entry| matches!(entry.status, SegmentCacheStatus::Discovered))
                })
                .count();
            return report;
        }
        for proxy_seq in
            known_sequences.iter().skip(prefetch_start).take(self.segment_prefetch_queue.max_prefetch_depth())
        {
            if self.queue_segment_fetch_candidate(*proxy_seq, SegmentFetchPriority::Prefetch, now_ms) {
                report.prefetch_queued = report.prefetch_queued.saturating_add(1);
            }
        }
        report
    }

    pub fn configure_segment_prefetch_queue(&mut self, max_prefetch_depth: usize) {
        self.segment_prefetch_queue.set_max_prefetch_depth(max_prefetch_depth);
    }
}

#[cfg(test)]
mod tests {
    use super::{SegmentFetchPriority, SegmentPrefetchQueue};
    use crate::{
        api::model::{HlsSession, HlsSessionKey, RenderPolicy, SegmentCacheStatus},
        processing::parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
    };

    const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

    fn session() -> HlsSession { HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0) }

    fn normal_manifest(body: &str) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
        match parse_origin_media_manifest(body, BASE_URL) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        }
    }

    #[test]
    fn queue_deduplicates_proxy_sequence() {
        let mut queue = SegmentPrefetchQueue::new(6);

        assert!(queue.enqueue(10, SegmentFetchPriority::Prefetch));
        assert!(!queue.enqueue(10, SegmentFetchPriority::Prefetch));

        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn demand_upgrades_existing_prefetch_or_render_window_item() {
        let mut queue = SegmentPrefetchQueue::new(6);

        assert!(queue.enqueue(10, SegmentFetchPriority::Prefetch));
        assert!(queue.enqueue(10, SegmentFetchPriority::Demand));

        assert_eq!(queue.pop_next(), Some((10, SegmentFetchPriority::Demand)));
        assert!(queue.is_empty());
    }

    #[test]
    fn pop_order_is_priority_then_chronological() {
        let mut queue = SegmentPrefetchQueue::new(6);

        assert!(queue.enqueue(30, SegmentFetchPriority::Prefetch));
        assert!(queue.enqueue(20, SegmentFetchPriority::Demand));
        assert!(queue.enqueue(10, SegmentFetchPriority::Demand));
        assert!(queue.enqueue(40, SegmentFetchPriority::RenderWindow));

        assert_eq!(queue.pop_next(), Some((10, SegmentFetchPriority::Demand)));
        assert_eq!(queue.pop_next(), Some((20, SegmentFetchPriority::Demand)));
        assert_eq!(queue.pop_next(), Some((40, SegmentFetchPriority::RenderWindow)));
        assert_eq!(queue.pop_next(), Some((30, SegmentFetchPriority::Prefetch)));
    }

    #[test]
    fn max_prefetch_depth_limits_prefetch_only() {
        let mut queue = SegmentPrefetchQueue::new(1);

        assert!(queue.enqueue(30, SegmentFetchPriority::Prefetch));
        assert!(!queue.enqueue(31, SegmentFetchPriority::Prefetch));
        assert!(queue.enqueue(10, SegmentFetchPriority::Demand));
        assert!(queue.enqueue(20, SegmentFetchPriority::RenderWindow));

        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn manifest_fetch_candidates_skip_prefetch_when_backpressure_disallows_it() {
        let mut session = session();
        session.configure_segment_prefetch_queue(6);
        session.render_policy = RenderPolicy::new(2);
        session
            .apply_origin_manifest(&normal_manifest(
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n#EXTINF:4.0,\n103.ts\n#EXTINF:4.0,\n104.ts\n#EXTINF:4.0,\n105.ts\n",
            ))
            .expect("manifest maps");

        let report = session.queue_manifest_fetch_candidates(10, false);

        assert_eq!(report.render_window_queued, 4);
        assert_eq!(report.prefetch_queued, 0);
        assert_eq!(report.prefetch_skipped, 2);
        assert!(session.segments.values().all(|segment| !matches!(
            segment.status,
            SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, .. }
        )));
    }
}
