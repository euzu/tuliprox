use super::{
    safe_proxy_session_id, HlsSession, MapEntry, OriginMapKey, ProxyMapId, ProxySessionId, SegmentCacheKey,
    SegmentFetchPriority,
};
use crate::processing::parser::hls::origin_manifest::{
    ParsedByteRange, ParsedOriginManifest, ParsedOriginMap, ParsedOriginSegment,
};
use log::info;
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
};
use url::Url;

const SEGMENT_EXTENSIONS: &[&str] = &["ts", "m4s", "m4a"];
const MAP_EXTENSIONS: &[&str] = &["mp4", "m4s", "m4a"];

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct OriginSegmentKey {
    pub origin_epoch: u64,
    pub origin_seq: u64,
}

#[derive(Clone, Eq, PartialEq)]
pub struct OriginSegmentFetchRef {
    pub resolved_origin_url: String,
    pub byte_range: Option<ParsedByteRange>,
    pub valid_until_ms: Option<u64>,
}

impl OriginSegmentFetchRef {
    pub fn is_valid_at(&self, now_ms: u64) -> bool {
        self.valid_until_ms.is_none_or(|valid_until_ms| now_ms <= valid_until_ms)
    }
}

impl fmt::Debug for OriginSegmentFetchRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OriginSegmentFetchRef")
            .field("resolved_origin_url", &"<redacted>")
            .field("byte_range", &self.byte_range)
            .field("valid_until_ms", &self.valid_until_ms)
            .finish()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SegmentCacheStatus {
    Discovered,
    Queued { priority: SegmentFetchPriority, queued_at_ms: u64 },
    Fetching { priority: SegmentFetchPriority, started_at_ms: u64 },
    Ready { content_length: u64, ready_at_ms: u64 },
    Failed { failed_at_ms: u64 },
    Expired,
}

/// Mutable access counters shared with response streams without holding a session lock.
#[derive(Debug, Default)]
pub struct CacheAccessState {
    active_readers: AtomicU32,
    last_accessed_at_ms: AtomicU64,
}

impl CacheAccessState {
    pub fn new() -> Self { Self::default() }

    pub fn reader_started(&self, now_ms: u64) {
        self.active_readers.fetch_add(1, Ordering::AcqRel);
        self.last_accessed_at_ms.store(now_ms, Ordering::Release);
    }

    pub fn reader_finished(&self) {
        let mut current = self.active_readers.load(Ordering::Acquire);
        while current > 0 {
            match self.active_readers.compare_exchange_weak(current, current - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    pub fn active_readers(&self) -> u32 { self.active_readers.load(Ordering::Acquire) }

    pub fn last_accessed_at_ms(&self) -> u64 { self.last_accessed_at_ms.load(Ordering::Acquire) }
}

impl PartialEq for CacheAccessState {
    fn eq(&self, other: &Self) -> bool {
        self.active_readers() == other.active_readers() && self.last_accessed_at_ms() == other.last_accessed_at_ms()
    }
}

impl Eq for CacheAccessState {}

pub fn default_content_type_for_segment_ext(extension: &str) -> &'static str {
    match extension {
        "ts" => "video/MP2T",
        "m4a" => "audio/mp4",
        "m4s" => "video/mp4",
        _ => "application/octet-stream",
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SegmentEntry {
    pub origin_key: OriginSegmentKey,
    pub proxy_seq: u64,
    pub duration_ms: u64,
    pub proxy_file_ext: String,
    pub content_type: String,
    pub cache_key: SegmentCacheKey,
    pub discontinuity_before: bool,
    pub program_date_time: Option<String>,
    pub daterange_tags_before: Vec<String>,
    pub origin_byte_range: Option<ParsedByteRange>,
    pub map_ref: Option<ProxyMapId>,
    pub origin_fetch_ref: Option<OriginSegmentFetchRef>,
    pub status: SegmentCacheStatus,
    pub last_rendered_at_ms: Option<u64>,
    pub access: Arc<CacheAccessState>,
}

impl fmt::Debug for SegmentEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentEntry")
            .field("origin_key", &self.origin_key)
            .field("proxy_seq", &self.proxy_seq)
            .field("duration_ms", &self.duration_ms)
            .field("proxy_file_ext", &self.proxy_file_ext)
            .field("content_type", &self.content_type)
            .field("cache_key", &self.cache_key)
            .field("discontinuity_before", &self.discontinuity_before)
            .field("program_date_time", &self.program_date_time)
            .field("daterange_tags_before", &self.daterange_tags_before)
            .field("origin_byte_range", &self.origin_byte_range)
            .field("map_ref", &self.map_ref)
            .field("origin_fetch_ref", &self.origin_fetch_ref)
            .field("status", &self.status)
            .field("last_rendered_at_ms", &self.last_rendered_at_ms)
            .field("active_readers", &self.access.active_readers())
            .field("last_accessed_at_ms", &self.access.last_accessed_at_ms())
            .finish()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TimelineMapError {
    UnsupportedSegmentExtension,
    UnsupportedMapExtension,
    ProxySequenceOverflow,
    ProxyMapIdOverflow,
}

#[derive(Clone)]
struct TimelineDraft {
    proxy_session_id: ProxySessionId,
    origin_epoch: u64,
    origin_seq_highwater: Option<u64>,
    proxy_next_seq: Option<u64>,
    origin_to_proxy: HashMap<OriginSegmentKey, u64>,
    discontinuity_sequence: u64,
    pending_handoff_discontinuity_sequence: Option<u64>,
    segments: BTreeMap<u64, SegmentEntry>,
    maps: BTreeMap<ProxyMapId, MapEntry>,
    origin_map_to_proxy: HashMap<OriginMapKey, ProxyMapId>,
    next_proxy_map_id: u64,
    publishable_origin_tail_proxy_seq: Option<u64>,
    origin_version: Option<u16>,
    target_duration: Option<u32>,
    independent_segments: bool,
}

impl From<&HlsSession> for TimelineDraft {
    fn from(session: &HlsSession) -> Self {
        Self {
            proxy_session_id: session.proxy_session_id.clone(),
            origin_epoch: session.origin_epoch,
            origin_seq_highwater: session.origin_seq_highwater,
            proxy_next_seq: session.proxy_next_seq,
            origin_to_proxy: session.origin_to_proxy.clone(),
            discontinuity_sequence: session.discontinuity_sequence,
            pending_handoff_discontinuity_sequence: session.pending_handoff_discontinuity_sequence,
            segments: session.segments.clone(),
            maps: session.maps.clone(),
            origin_map_to_proxy: session.origin_map_to_proxy.clone(),
            next_proxy_map_id: session.next_proxy_map_id,
            publishable_origin_tail_proxy_seq: session.publishable_origin_tail_proxy_seq,
            origin_version: session.origin_version,
            target_duration: session.target_duration,
            independent_segments: session.independent_segments,
        }
    }
}

impl HlsSession {
    pub fn apply_origin_manifest(&mut self, manifest: &ParsedOriginManifest) -> Result<(), TimelineMapError> {
        let mut draft = TimelineDraft::from(&*self);
        draft.apply_manifest(manifest)?;

        self.origin_epoch = draft.origin_epoch;
        self.origin_seq_highwater = draft.origin_seq_highwater;
        self.proxy_next_seq = draft.proxy_next_seq;
        self.origin_to_proxy = draft.origin_to_proxy;
        self.discontinuity_sequence = draft.discontinuity_sequence;
        self.pending_handoff_discontinuity_sequence = draft.pending_handoff_discontinuity_sequence;
        self.segments = draft.segments;
        self.maps = draft.maps;
        self.origin_map_to_proxy = draft.origin_map_to_proxy;
        self.next_proxy_map_id = draft.next_proxy_map_id;
        self.publishable_origin_tail_proxy_seq = draft.publishable_origin_tail_proxy_seq;
        self.origin_version = draft.origin_version;
        self.target_duration = draft.target_duration;
        self.independent_segments = draft.independent_segments;

        Ok(())
    }
}

impl TimelineDraft {
    fn apply_manifest(&mut self, manifest: &ParsedOriginManifest) -> Result<(), TimelineMapError> {
        let handoff_discontinuity_sequence = self.pending_handoff_discontinuity_sequence.take();
        if self.segments.is_empty() {
            self.discontinuity_sequence = manifest
                .discontinuity_sequence
                .unwrap_or(0)
                .saturating_add(handoff_discontinuity_sequence.unwrap_or(0));
        }
        self.origin_version = manifest.version;
        self.target_duration = manifest.target_duration;
        self.independent_segments = manifest.independent_segments;
        self.apply_forward_jump(manifest)?;

        let mut mark_handoff_discontinuity = handoff_discontinuity_sequence.is_some();
        for parsed in &manifest.segments {
            self.map_origin_segment(parsed, manifest, mark_handoff_discontinuity)?;
            mark_handoff_discontinuity = false;
        }

        self.publishable_origin_tail_proxy_seq = self.segments.keys().next_back().copied();
        Ok(())
    }

    fn apply_forward_jump(&mut self, manifest: &ParsedOriginManifest) -> Result<(), TimelineMapError> {
        let Some(highwater) = self.origin_seq_highwater else {
            return Ok(());
        };
        let next_origin_seq = highwater.checked_add(1).ok_or(TimelineMapError::ProxySequenceOverflow)?;
        if manifest.origin_manifest_sequence <= next_origin_seq {
            return Ok(());
        }

        let missing_origin_segments = manifest.origin_manifest_sequence - next_origin_seq;
        let proxy_next_seq = self.proxy_next_seq.unwrap_or(next_origin_seq);
        self.proxy_next_seq =
            Some(proxy_next_seq.checked_add(missing_origin_segments).ok_or(TimelineMapError::ProxySequenceOverflow)?);
        self.origin_seq_highwater = Some(manifest.origin_manifest_sequence - 1);
        info!(
            "HLS forward jump accepted: proxy_session_id={} origin_sequence={} missing_segments={missing_origin_segments}",
            safe_proxy_session_id(&self.proxy_session_id),
            manifest.origin_manifest_sequence
        );
        Ok(())
    }

    fn map_origin_segment(
        &mut self,
        parsed: &ParsedOriginSegment,
        manifest: &ParsedOriginManifest,
        handoff_discontinuity_before: bool,
    ) -> Result<(), TimelineMapError> {
        let current_epoch_key = OriginSegmentKey { origin_epoch: self.origin_epoch, origin_seq: parsed.origin_seq };
        if self.origin_to_proxy.contains_key(&current_epoch_key) {
            if let Some(proxy_seq) = self.origin_to_proxy.get(&current_epoch_key).copied() {
                if let Some(entry) = self.segments.get_mut(&proxy_seq) {
                    entry.origin_fetch_ref = Some(OriginSegmentFetchRef {
                        resolved_origin_url: parsed.resolved_origin_url.clone(),
                        byte_range: parsed.origin_byte_range,
                        valid_until_ms: None,
                    });
                }
            }
            return Ok(());
        }

        let mut rollover_discontinuity = false;
        match self.origin_seq_highwater {
            Some(highwater) if parsed.origin_seq < highwater => {
                self.origin_epoch = self.origin_epoch.checked_add(1).ok_or(TimelineMapError::ProxySequenceOverflow)?;
                self.origin_seq_highwater = Some(parsed.origin_seq);
                rollover_discontinuity = true;
                info!(
                    "HLS media sequence rollover detected: proxy_session_id={} previous_highwater={highwater} next_origin_seq={} origin_epoch={}",
                    safe_proxy_session_id(&self.proxy_session_id),
                    parsed.origin_seq,
                    self.origin_epoch
                );
            }
            Some(highwater) => self.origin_seq_highwater = Some(highwater.max(parsed.origin_seq)),
            None => self.origin_seq_highwater = Some(parsed.origin_seq),
        }

        let origin_key = OriginSegmentKey { origin_epoch: self.origin_epoch, origin_seq: parsed.origin_seq };

        let proxy_seq = match self.proxy_next_seq {
            Some(next) => next,
            None => parsed.origin_seq,
        };
        self.proxy_next_seq = Some(proxy_seq.checked_add(1).ok_or(TimelineMapError::ProxySequenceOverflow)?);

        let map_ref = parsed
            .map_ref
            .map(|map_id| {
                let parsed_map = manifest.maps.get(map_id).ok_or(TimelineMapError::UnsupportedMapExtension)?;
                self.map_origin_map(parsed_map)
            })
            .transpose()?;
        let proxy_file_ext = proxy_extension_from_url(&parsed.resolved_origin_url, SEGMENT_EXTENSIONS)
            .ok_or(TimelineMapError::UnsupportedSegmentExtension)?;
        let entry = SegmentEntry {
            origin_key,
            proxy_seq,
            duration_ms: parsed.duration_ms,
            content_type: default_content_type_for_segment_ext(&proxy_file_ext).to_string(),
            cache_key: SegmentCacheKey::new(self.proxy_session_id.clone(), proxy_seq, &proxy_file_ext),
            proxy_file_ext,
            discontinuity_before: parsed.discontinuity_before || rollover_discontinuity || handoff_discontinuity_before,
            program_date_time: parsed.program_date_time.clone(),
            daterange_tags_before: parsed.daterange_tags_before.clone(),
            origin_byte_range: parsed.origin_byte_range,
            map_ref,
            origin_fetch_ref: Some(OriginSegmentFetchRef {
                resolved_origin_url: parsed.resolved_origin_url.clone(),
                byte_range: parsed.origin_byte_range,
                valid_until_ms: None,
            }),
            status: SegmentCacheStatus::Discovered,
            last_rendered_at_ms: None,
            access: Arc::new(CacheAccessState::new()),
        };

        self.origin_to_proxy.insert(origin_key, proxy_seq);
        self.segments.insert(proxy_seq, entry);
        Ok(())
    }

    fn map_origin_map(&mut self, parsed_map: &ParsedOriginMap) -> Result<ProxyMapId, TimelineMapError> {
        let origin_map_key = OriginMapKey {
            origin_epoch: self.origin_epoch,
            resolved_origin_uri: parsed_map.resolved_origin_uri.clone(),
            byte_range: parsed_map.byte_range,
        };
        if let Some(proxy_map_id) = self.origin_map_to_proxy.get(&origin_map_key).copied() {
            return Ok(proxy_map_id);
        }

        let proxy_map_id = ProxyMapId(self.next_proxy_map_id);
        self.next_proxy_map_id = self.next_proxy_map_id.checked_add(1).ok_or(TimelineMapError::ProxyMapIdOverflow)?;
        let proxy_file_ext = proxy_extension_from_url(&origin_map_key.resolved_origin_uri, MAP_EXTENSIONS)
            .ok_or(TimelineMapError::UnsupportedMapExtension)?;
        self.maps.insert(
            proxy_map_id,
            MapEntry::new(&self.proxy_session_id, proxy_map_id, origin_map_key.clone(), proxy_file_ext),
        );
        self.origin_map_to_proxy.insert(origin_map_key, proxy_map_id);
        Ok(proxy_map_id)
    }
}

fn proxy_extension_from_url(url: &str, allowed_extensions: &[&str]) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let file_name = parsed.path_segments()?.next_back()?;
    let extension = file_name.rsplit_once('.')?.1.to_ascii_lowercase();
    allowed_extensions.contains(&extension.as_str()).then_some(extension)
}

#[cfg(test)]
mod tests {
    use super::{SegmentCacheStatus, TimelineMapError};
    use crate::{
        api::model::{
            HlsSession, HlsSessionKey, MapCacheStatus, OriginSegmentKey, ProxyMapId, RenderPolicy, SegmentFetchPriority,
        },
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
    fn parsed_target_duration_is_stored_for_account_overlap_timing() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:12.0,\n10.ts\n",
        );

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        assert_eq!(session.target_duration, Some(12));
        assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
        assert_eq!(session.account_overlap_timing().hard_active_window_ms, 12_000);
        assert_eq!(session.account_overlap_timing().soft_active_window_ms, 24_000);
    }

    #[test]
    fn origin_rollover_maps_to_monotone_proxy_sequence() {
        let mut session = session();
        let first = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:322\n#EXTINF:4.0,\n322.ts\n#EXTINF:4.0,\n323.ts\n#EXTINF:4.0,\n324.ts\n",
        );
        let second = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n",
        );

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.apply_origin_manifest(&second).expect("second manifest should map");

        assert_eq!(session.segments.keys().copied().collect::<Vec<_>>(), vec![322, 323, 324, 325, 326, 327]);
        assert_eq!(session.origin_to_proxy.get(&OriginSegmentKey { origin_epoch: 1, origin_seq: 0 }), Some(&325));
        assert!(session.segments.get(&325).expect("rollover segment").discontinuity_before);
    }

    #[test]
    fn known_origin_key_is_not_remapped() {
        let mut session = session();
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\n10.ts\n");

        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.apply_origin_manifest(&manifest).expect("same manifest should be ignored");

        assert_eq!(session.segments.len(), 1);
        assert_eq!(session.proxy_next_seq, Some(11));
    }

    #[test]
    fn overlapping_old_origin_sequences_do_not_trigger_rollover() {
        let mut session = session();
        let first = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n",
        );
        let overlap = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n#EXTINF:4.0,\n103.ts\n",
        );

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.apply_origin_manifest(&overlap).expect("overlapping manifest should map");

        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.origin_seq_highwater, Some(103));
        assert_eq!(session.segments.keys().copied().collect::<Vec<_>>(), vec![100, 101, 102, 103]);
    }

    #[test]
    fn forward_jump_advances_proxy_next_seq_before_mapping() {
        let mut session = session();
        let first = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n");
        let jump = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:107\n#EXTINF:4.0,\n107.ts\n#EXTINF:4.0,\n108.ts\n#EXTINF:4.0,\n109.ts\n",
        );

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.apply_origin_manifest(&jump).expect("jump manifest should map");

        assert_eq!(session.origin_to_proxy.get(&OriginSegmentKey { origin_epoch: 0, origin_seq: 107 }), Some(&107));
        assert_eq!(session.proxy_next_seq, Some(110));
        assert_eq!(session.publishable_origin_tail_proxy_seq, Some(109));
    }

    #[test]
    fn mapping_error_does_not_commit_partial_session_state() {
        let mut session = session();
        let first = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\n10.ts\n");
        let invalid =
            normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:11\n#EXTINF:4.0,\n11.ts\n#EXTINF:4.0,\n12.webm\n");

        session.apply_origin_manifest(&first).expect("first manifest should map");
        let previous_proxy_next_seq = session.proxy_next_seq;
        let previous_highwater = session.origin_seq_highwater;
        let previous_tail = session.publishable_origin_tail_proxy_seq;
        let previous_segments = session.segments.clone();
        let previous_origin_to_proxy = session.origin_to_proxy.clone();

        assert_eq!(session.apply_origin_manifest(&invalid), Err(TimelineMapError::UnsupportedSegmentExtension));

        assert_eq!(session.proxy_next_seq, previous_proxy_next_seq);
        assert_eq!(session.origin_seq_highwater, previous_highwater);
        assert_eq!(session.publishable_origin_tail_proxy_seq, previous_tail);
        assert_eq!(session.segments, previous_segments);
        assert_eq!(session.origin_to_proxy, previous_origin_to_proxy);
    }

    #[test]
    fn unsupported_segment_extension_fails_mapping() {
        let mut session = session();
        let manifest = normal_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.webm\n");

        assert_eq!(session.apply_origin_manifest(&manifest), Err(TimelineMapError::UnsupportedSegmentExtension));
    }

    #[test]
    fn mapped_maps_start_as_discovered_placeholders() {
        let mut session = session();
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg.m4s\n");

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        let map = session.maps.values().next().expect("map placeholder");
        assert_eq!(map.status, MapCacheStatus::Discovered);
        assert_eq!(map.proxy_map_id, ProxyMapId(0));
        assert_eq!(map.proxy_file_ext, "mp4");
        assert_eq!(map.origin_key.origin_epoch, 0);
        assert_eq!(map.origin_key.byte_range, None);
        assert_eq!(map.cache_key.proxy_map_id(), ProxyMapId(0));
        let fetch_ref = map.origin_fetch_ref.as_ref().expect("map fetch ref");
        assert_eq!(fetch_ref.byte_range, None);
        assert!(format!("{fetch_ref:?}").contains("<redacted>"));
        assert!(!format!("{fetch_ref:?}").contains("init.mp4"));
    }

    #[test]
    fn manifest_mapping_sets_origin_fetch_ref() {
        let mut session = session();
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\nseg.ts\n");

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        let segment = session.segments.get(&1).expect("segment should be mapped");
        let fetch_ref = segment.origin_fetch_ref.as_ref().expect("fetch ref should be set");
        assert!(format!("{fetch_ref:?}").contains("<redacted>"));
        assert!(!format!("{fetch_ref:?}").contains("seg.ts"));
    }

    #[test]
    fn cold_start_prefetch_prioritizes_visible_window_then_known_tail() {
        let mut session = session();
        session.render_policy = RenderPolicy::new(3);
        session.configure_segment_prefetch_queue(6);
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n#EXTINF:4.0,\n103.ts\n#EXTINF:4.0,\n104.ts\n#EXTINF:4.0,\n105.ts\n",
        );

        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.queue_manifest_prefetch_candidates(10);

        assert!(matches!(
            session.segments.get(&100).expect("100").status,
            SegmentCacheStatus::Queued { priority: SegmentFetchPriority::RenderWindow, .. }
        ));
        assert!(matches!(
            session.segments.get(&102).expect("102").status,
            SegmentCacheStatus::Queued { priority: SegmentFetchPriority::RenderWindow, .. }
        ));
        assert!(matches!(
            session.segments.get(&103).expect("103").status,
            SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, .. }
        ));
        assert_eq!(session.segment_prefetch_queue.prefetch_len(), 3);
    }

    #[test]
    fn same_map_uri_with_different_byterange_uses_distinct_proxy_map_ids() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"100@0\"\n#EXTINF:4.0,\n1.m4s\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"100@100\"\n#EXTINF:4.0,\n2.m4s\n",
        );

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        assert_eq!(session.maps.len(), 2);
        assert_eq!(session.segments.get(&0).expect("first segment").map_ref, Some(ProxyMapId(0)));
        assert_eq!(session.segments.get(&1).expect("second segment").map_ref, Some(ProxyMapId(1)));
    }

    #[test]
    fn same_map_uri_in_same_epoch_reuses_proxy_map_id() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n2.m4s\n",
        );

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        assert_eq!(session.maps.len(), 1);
        assert_eq!(session.segments.get(&0).expect("first segment").map_ref, Some(ProxyMapId(0)));
        assert_eq!(session.segments.get(&1).expect("second segment").map_ref, Some(ProxyMapId(0)));
    }

    #[test]
    fn same_map_uri_after_rollover_uses_distinct_proxy_map_id() {
        let mut session = session();
        let first =
            normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:322\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n322.m4s\n");
        let second =
            normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n0.m4s\n");

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.apply_origin_manifest(&second).expect("second manifest should map");

        assert_eq!(session.maps.len(), 2);
        assert_eq!(session.segments.get(&322).expect("first epoch segment").map_ref, Some(ProxyMapId(0)));
        assert_eq!(session.segments.get(&323).expect("second epoch segment").map_ref, Some(ProxyMapId(1)));
        assert_eq!(session.maps.get(&ProxyMapId(1)).expect("second map").origin_key.origin_epoch, 1);
    }
}
