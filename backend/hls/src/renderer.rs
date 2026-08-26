use super::{
    is_hls_provisioning_gap_segment, is_hls_provisioning_segment, session::HlsPublishedLiveOriginBaseline,
    HlsSegmentEncryption, HlsSession, MapCacheStatus, ProxySessionId, SegmentCacheStatus, SegmentEntry,
    HLS_ACCESS_LEASE_ID_PLACEHOLDER, HLS_PROVISIONING_TARGET_DURATION_SECS,
};
use shared::model::HlsStripMode;
use std::fmt::Write as _;
use tuliprox_core::{
    model::StripConfig,
    utils::{format_hls_duration_ms, hls_target_duration_secs},
};

const MIN_VISIBLE_SEGMENTS: usize = 3;
const TARGET_VISIBLE_SEGMENTS: usize = 6;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RenderPolicy {
    pub initial_render_gap_segments: usize,
    pub max_not_ready_render_segments: usize,
}

impl RenderPolicy {
    pub fn new(initial_render_gap_segments: usize) -> Self {
        Self { initial_render_gap_segments, max_not_ready_render_segments: 2 }
    }

    pub fn from_strip_config(strip: &StripConfig, segment_durations_ms: &[u64]) -> Self {
        match strip.mode {
            HlsStripMode::Segments => Self::new(usize::try_from(strip.value).unwrap_or(usize::MAX)),
            HlsStripMode::Seconds => {
                let target_ms = strip.value.saturating_mul(1_000);
                let mut accumulated_ms = 0_u64;
                let mut gap_segments = 0_usize;
                for duration_ms in segment_durations_ms.iter().rev() {
                    if accumulated_ms >= target_ms {
                        break;
                    }
                    accumulated_ms = accumulated_ms.saturating_add(*duration_ms);
                    gap_segments = gap_segments.saturating_add(1);
                }
                Self::new(gap_segments)
            }
        }
    }
}

impl Default for RenderPolicy {
    fn default() -> Self { Self::new(0) }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RenderedManifest {
    pub body: String,
    pub first_proxy_seq: u64,
    pub last_proxy_seq: u64,
    pub discontinuity_sequence: u64,
    pub target_duration_ms: u64,
    pub playlist_duration_ms: u64,
    pub valid_until_ms: u64,
    pub render_gap_segments: usize,
    pub rendered_at_ms: u64,
    pub segment_proxy_seqs: Vec<u64>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RenderedManifestStoreOutcome {
    Stored,
    Rejected(RenderedManifestStoreRejectReason),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RenderedManifestStoreRejectReason {
    RegressiveMediaSequence { previous_first_proxy_seq: u64, candidate_first_proxy_seq: u64 },
    DuplicateMediaResource { existing_proxy_seq: u64, candidate_proxy_seq: u64 },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RenderError {
    NoRenderableWindow,
    InvalidState,
    StoreRejected(RenderedManifestStoreRejectReason),
}

pub struct HlsManifestRenderer;

#[derive(Clone, Copy)]
enum RenderTailPolicy {
    RequirePlannedTail,
    TruncateUnavailableTail,
}

impl HlsManifestRenderer {
    pub fn render(session: &HlsSession, rendered_at_ms: u64) -> Result<RenderedManifest, RenderError> {
        for render_gap_segments in 0..=session.render_policy.initial_render_gap_segments {
            let Some(window) = select_window(session, render_gap_segments, RenderTailPolicy::TruncateUnavailableTail)
            else {
                continue;
            };
            let manifest = render_window(session, &window, render_gap_segments, rendered_at_ms)?;
            return Ok(manifest);
        }

        Err(RenderError::NoRenderableWindow)
    }
}

impl HlsSession {
    pub fn render_and_store_manifest(&mut self, rendered_at_ms: u64) -> Result<RenderedManifest, RenderError> {
        let rendered = HlsManifestRenderer::render(self, rendered_at_ms)?;
        match self.store_rendered_manifest(rendered.clone()) {
            RenderedManifestStoreOutcome::Stored => Ok(rendered),
            RenderedManifestStoreOutcome::Rejected(reason) => Err(RenderError::StoreRejected(reason)),
        }
    }

    pub fn store_rendered_manifest(&mut self, rendered: RenderedManifest) -> RenderedManifestStoreOutcome {
        if let Some(previous) = &self.last_rendered_manifest {
            if rendered.first_proxy_seq < previous.first_proxy_seq {
                return RenderedManifestStoreOutcome::Rejected(
                    RenderedManifestStoreRejectReason::RegressiveMediaSequence {
                        previous_first_proxy_seq: previous.first_proxy_seq,
                        candidate_first_proxy_seq: rendered.first_proxy_seq,
                    },
                );
            }
        }
        let rendered_identities = rendered
            .segment_proxy_seqs
            .iter()
            .filter_map(|proxy_seq| {
                self.segments
                    .get(proxy_seq)
                    .and_then(SegmentEntry::media_resource_identity)
                    .map(|identity| (*proxy_seq, identity))
            })
            .collect::<Vec<_>>();
        for (index, (candidate_proxy_seq, candidate_identity)) in rendered_identities.iter().enumerate() {
            if let Some((existing_proxy_seq, _)) = rendered_identities[..index]
                .iter()
                .find(|(_, existing_identity)| existing_identity.matches(*candidate_identity))
            {
                return RenderedManifestStoreOutcome::Rejected(
                    RenderedManifestStoreRejectReason::DuplicateMediaResource {
                        existing_proxy_seq: *existing_proxy_seq,
                        candidate_proxy_seq: *candidate_proxy_seq,
                    },
                );
            }
            if let Some(existing_proxy_seq) = self.published_resource_history.proxy_seq_for(*candidate_identity) {
                if existing_proxy_seq != *candidate_proxy_seq {
                    return RenderedManifestStoreOutcome::Rejected(
                        RenderedManifestStoreRejectReason::DuplicateMediaResource {
                            existing_proxy_seq,
                            candidate_proxy_seq: *candidate_proxy_seq,
                        },
                    );
                }
            }
        }
        let published_origin_evidence = rendered.segment_proxy_seqs.iter().find_map(|proxy_seq| {
            let segment = self.segments.get(proxy_seq)?;
            if is_local_provisioning_segment(segment) {
                return None;
            }
            Some(HlsPublishedLiveOriginBaseline {
                evidence_proxy_seq: *proxy_seq,
                origin_epoch: segment.origin_key.origin_epoch,
                rendered_at_ms: rendered.rendered_at_ms,
            })
        });
        self.longest_rendered_playlist_duration_ms =
            self.longest_rendered_playlist_duration_ms.max(rendered.playlist_duration_ms);
        for proxy_seq in &rendered.segment_proxy_seqs {
            if let Some(segment) = self.segments.get_mut(proxy_seq) {
                segment.last_rendered_at_ms = Some(rendered.rendered_at_ms);
            }
        }
        for (proxy_seq, identity) in rendered_identities {
            self.published_resource_history.record(identity, proxy_seq);
        }
        if self.published_live_origin_baseline.is_none() {
            self.published_live_origin_baseline = published_origin_evidence;
        }
        self.last_rendered_manifest = Some(rendered);
        RenderedManifestStoreOutcome::Stored
    }
}

pub fn renderer_candidate_window_proxy_seqs(session: &HlsSession) -> Vec<u64> {
    for render_gap_segments in 0..=session.render_policy.initial_render_gap_segments {
        if let Some(window) = select_window(session, render_gap_segments, RenderTailPolicy::RequirePlannedTail) {
            return window;
        }
    }
    Vec::new()
}

fn is_renderable(entry: &SegmentEntry, session: &HlsSession) -> bool {
    match entry.status {
        SegmentCacheStatus::Ready { .. } => {}
        SegmentCacheStatus::Queued { .. } | SegmentCacheStatus::Fetching { .. } => {
            if entry.origin_fetch_ref.is_none() {
                return false;
            }
        }
        SegmentCacheStatus::Discovered
        | SegmentCacheStatus::CapacityDeferred { .. }
        | SegmentCacheStatus::FailedRetryable { .. }
        | SegmentCacheStatus::FailedPermanent { .. }
        | SegmentCacheStatus::Expired => {
            return false;
        }
    }
    let Some(map_ref) = entry.map_ref else {
        return true;
    };
    session.maps.get(&map_ref).is_some_and(|map| matches!(map.status, MapCacheStatus::Ready { .. }))
}

fn select_window(session: &HlsSession, render_gap_segments: usize, tail_policy: RenderTailPolicy) -> Option<Vec<u64>> {
    let head_seq = session.publishable_origin_head_proxy_seq?;
    let requested_tail_seq =
        session.publishable_origin_tail_proxy_seq?.checked_sub(u64::try_from(render_gap_segments).ok()?)?;
    if requested_tail_seq < head_seq {
        return None;
    }

    let tail_seq = match tail_policy {
        RenderTailPolicy::RequirePlannedTail => requested_tail_seq,
        RenderTailPolicy::TruncateUnavailableTail => {
            // Capacity and permanent failures truncate only the unpublished
            // tail. Never skip a hole and never regress media sequence.
            let mut tail_seq = None;
            for current in head_seq..=requested_tail_seq {
                let entry = session.segments.get(&current)?;
                if !is_renderable(entry, session) {
                    if matches!(
                        entry.status,
                        SegmentCacheStatus::CapacityDeferred { .. }
                            | SegmentCacheStatus::FailedRetryable { .. }
                            | SegmentCacheStatus::FailedPermanent { .. }
                            | SegmentCacheStatus::Expired
                    ) {
                        break;
                    }
                    return None;
                }
                tail_seq = Some(current);
            }
            tail_seq?
        }
    };

    let current_origin_window_len = tail_seq.saturating_sub(head_seq).saturating_add(1);
    let target_window_len = current_origin_window_len.min(u64::try_from(TARGET_VISIBLE_SEGMENTS).ok()?);
    let start_seq = tail_seq.saturating_add(1).saturating_sub(target_window_len).max(head_seq);
    let mut window = Vec::new();
    let mut not_ready_count = 0_usize;

    for current in start_seq..=tail_seq {
        let entry = session.segments.get(&current)?;
        if !is_renderable(entry, session) {
            return None;
        }
        if !matches!(entry.status, SegmentCacheStatus::Ready { .. }) {
            not_ready_count = not_ready_count.saturating_add(1);
            if not_ready_count > session.render_policy.max_not_ready_render_segments {
                return None;
            }
        }
        window.push(current);
    }

    if window.len() < MIN_VISIBLE_SEGMENTS {
        return None;
    }
    // Lease admission enforces the READY-duration reserve. The renderer only
    // selects a structurally renderable window and must not duplicate that
    // policy from its already-selected duration.
    Some(window)
}

fn render_window(
    session: &HlsSession,
    window: &[u64],
    render_gap_segments: usize,
    rendered_at_ms: u64,
) -> Result<RenderedManifest, RenderError> {
    let Some((&first_proxy_seq, &last_proxy_seq)) = window.first().zip(window.last()) else {
        return Err(RenderError::InvalidState);
    };
    let playlist_duration_ms = playlist_duration_ms(session, window);
    let target_duration = resolve_target_duration(session, window);
    let hls_version = resolve_hls_version(session, window);
    let discontinuity_sequence =
        session.discontinuity_sequence + hidden_discontinuities_before(session, first_proxy_seq);
    let mut body = String::new();

    body.push_str("#EXTM3U\n");
    writeln!(body, "#EXT-X-VERSION:{hls_version}").map_err(|_| RenderError::InvalidState)?;
    if session.independent_segments || window_contains_provisioning_segment(session, window) {
        body.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    }
    writeln!(body, "#EXT-X-TARGETDURATION:{target_duration}").map_err(|_| RenderError::InvalidState)?;
    writeln!(body, "#EXT-X-MEDIA-SEQUENCE:{first_proxy_seq}").map_err(|_| RenderError::InvalidState)?;
    writeln!(body, "#EXT-X-DISCONTINUITY-SEQUENCE:{discontinuity_sequence}").map_err(|_| RenderError::InvalidState)?;

    let mut current_map_ref = None;
    let mut current_encryption: Option<HlsSegmentEncryption> = None;
    let contains_provisioning = window_contains_provisioning_segment(session, window);
    let mut media_units_rendered = 0_usize;
    for proxy_seq in window {
        let entry = session.segments.get(proxy_seq).ok_or(RenderError::InvalidState)?;
        if contains_provisioning && media_units_rendered == 0 {
            append_manifest_block_separator(&mut body);
        }
        for daterange in &entry.daterange_tags_before {
            body.push_str(daterange);
            body.push('\n');
        }
        if let Some(program_date_time) = &entry.program_date_time {
            writeln!(body, "#EXT-X-PROGRAM-DATE-TIME:{program_date_time}").map_err(|_| RenderError::InvalidState)?;
        }
        if entry.discontinuity_before {
            if contains_provisioning {
                append_manifest_block_separator(&mut body);
            }
            body.push_str("#EXT-X-DISCONTINUITY\n");
        }
        if is_hls_provisioning_gap_segment(entry) {
            if contains_provisioning {
                append_manifest_block_separator(&mut body);
            }
            body.push_str("#EXT-X-GAP\n");
        }
        if entry.map_ref != current_map_ref {
            if let Some(map_ref) = entry.map_ref {
                let map = session.maps.get(&map_ref).ok_or(RenderError::InvalidState)?;
                writeln!(
                    body,
                    "#EXT-X-MAP:URI=\"/hls/shared/live/{}/{}/map/{:06}.{}\"",
                    session.proxy_session_id.0, HLS_ACCESS_LEASE_ID_PLACEHOLDER, map.proxy_map_id.0, map.proxy_file_ext
                )
                .map_err(|_| RenderError::InvalidState)?;
            }
            current_map_ref = entry.map_ref;
        }
        if entry.encryption != current_encryption {
            render_encryption_transition(&mut body, session, entry.encryption.as_ref())?;
            current_encryption.clone_from(&entry.encryption);
        }
        writeln!(body, "#EXTINF:{},", format_hls_duration_ms(entry.duration_ms))
            .map_err(|_| RenderError::InvalidState)?;
        if is_local_provisioning_segment(entry) {
            writeln!(
                body,
                "/hls/shared/live/{}/{}/{:06}.{}?pseq={}",
                proxy_session_id(session),
                HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                entry.proxy_seq,
                entry.proxy_file_ext,
                entry.proxy_seq
            )
            .map_err(|_| RenderError::InvalidState)?;
        } else {
            writeln!(
                body,
                "/hls/shared/live/{}/{}/{:06}.{}",
                proxy_session_id(session),
                HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                entry.proxy_seq,
                entry.proxy_file_ext
            )
            .map_err(|_| RenderError::InvalidState)?;
        }
        media_units_rendered = media_units_rendered.saturating_add(1);
    }

    Ok(RenderedManifest {
        body,
        first_proxy_seq,
        last_proxy_seq,
        discontinuity_sequence,
        target_duration_ms: u64::from(target_duration).saturating_mul(1_000),
        playlist_duration_ms,
        valid_until_ms: rendered_at_ms.saturating_add(playlist_duration_ms),
        render_gap_segments,
        rendered_at_ms,
        segment_proxy_seqs: window.to_vec(),
    })
}

fn render_encryption_transition(
    body: &mut String,
    session: &HlsSession,
    encryption: Option<&HlsSegmentEncryption>,
) -> Result<(), RenderError> {
    let Some(encryption) = encryption else {
        body.push_str("#EXT-X-KEY:METHOD=NONE\n");
        return Ok(());
    };
    write!(
        body,
        "#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/{}/{}/r/{}.{}\"",
        proxy_session_id(session),
        HLS_ACCESS_LEASE_ID_PLACEHOLDER,
        encryption.resource_id.0,
        encryption.resource_extension
    )
    .map_err(|_| RenderError::InvalidState)?;
    if let Some(iv) = &encryption.iv {
        write!(body, ",IV={iv}").map_err(|_| RenderError::InvalidState)?;
    }
    if let Some(key_format) = &encryption.key_format {
        write!(body, ",KEYFORMAT=\"{key_format}\"").map_err(|_| RenderError::InvalidState)?;
    }
    if let Some(versions) = &encryption.key_format_versions {
        write!(body, ",KEYFORMATVERSIONS=\"{versions}\"").map_err(|_| RenderError::InvalidState)?;
    }
    body.push('\n');
    Ok(())
}

fn append_manifest_block_separator(body: &mut String) {
    if body.is_empty() || body.ends_with("\n\n") {
        return;
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push('\n');
}

fn proxy_session_id(session: &HlsSession) -> &str {
    let ProxySessionId(value) = &session.proxy_session_id;
    value
}

fn resolve_hls_version(session: &HlsSession, window: &[u64]) -> u16 {
    if window_contains_provisioning_segment(session, window) {
        return 7;
    }
    let needs_map_version =
        window.iter().filter_map(|proxy_seq| session.segments.get(proxy_seq)).any(|entry| entry.map_ref.is_some());
    let needs_key_format_version = window
        .iter()
        .filter_map(|proxy_seq| session.segments.get(proxy_seq))
        .filter_map(|entry| entry.encryption.as_ref())
        .any(|encryption| encryption.key_format.is_some() || encryption.key_format_versions.is_some());
    let feature_version = if needs_map_version {
        6
    } else if needs_key_format_version {
        5
    } else {
        3
    };
    session.origin_version.unwrap_or(3).max(feature_version)
}

fn resolve_target_duration(session: &HlsSession, window: &[u64]) -> u32 {
    if window_contains_provisioning_segment(session, window) && !window_contains_origin_segment(session, window) {
        return HLS_PROVISIONING_TARGET_DURATION_SECS;
    }
    session.target_duration.unwrap_or_else(|| {
        window
            .iter()
            .filter_map(|proxy_seq| session.segments.get(proxy_seq))
            .map(|entry| hls_target_duration_secs(entry.duration_ms))
            .max()
            .unwrap_or(1)
            .max(1)
            .try_into()
            .unwrap_or(u32::MAX)
    })
}

fn window_contains_provisioning_segment(session: &HlsSession, window: &[u64]) -> bool {
    window.iter().filter_map(|proxy_seq| session.segments.get(proxy_seq)).any(is_local_provisioning_segment)
}

fn window_contains_origin_segment(session: &HlsSession, window: &[u64]) -> bool {
    window
        .iter()
        .filter_map(|proxy_seq| session.segments.get(proxy_seq))
        .any(|entry| !is_local_provisioning_segment(entry))
}

fn is_local_provisioning_segment(entry: &SegmentEntry) -> bool {
    is_hls_provisioning_segment(entry) || is_hls_provisioning_gap_segment(entry)
}

fn playlist_duration_ms(session: &HlsSession, window: &[u64]) -> u64 {
    window
        .iter()
        .filter_map(|proxy_seq| session.segments.get(proxy_seq))
        .fold(0_u64, |duration_ms, entry| duration_ms.saturating_add(entry.duration_ms))
}

fn hidden_discontinuities_before(session: &HlsSession, first_proxy_seq: u64) -> u64 {
    session
        .segments
        .range(..first_proxy_seq)
        .filter(|(_, entry)| entry.discontinuity_before)
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        HlsManifestRenderer, HlsPublishedLiveOriginBaseline, RenderError, RenderPolicy, RenderedManifest,
        RenderedManifestStoreOutcome, RenderedManifestStoreRejectReason,
    };
    use crate::{
        manifest_origin_binding::HlsManifestOriginBinding,
        timeline::{HLS_PROVISIONING_GAP_ORIGIN_EPOCH, HLS_PROVISIONING_ORIGIN_EPOCH},
        HlsSession, HlsSessionKey, MapCacheStatus, SegmentCacheStatus, SegmentFetchPriority,
    };
    use shared::model::HlsStripMode;
    use tuliprox_core::model::StripConfig;
    use tuliprox_parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome};
    use url::Url;

    const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

    fn session() -> HlsSession { HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0) }

    fn normal_manifest(body: &str) -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
        match parse_origin_media_manifest(body, BASE_URL) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        }
    }

    fn six_segment_manifest() -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
        normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:120\n#EXTINF:4.0,\norigin-name-120.ts\n#EXTINF:4.0,\norigin-name-121.ts\n#EXTINF:4.0,\norigin-name-122.ts\n#EXTINF:4.0,\norigin-name-123.ts\n#EXTINF:4.0,\norigin-name-124.ts\n#EXTINF:4.0,\norigin-name-125.ts\n",
        )
    }

    fn mark_all_segments_ready(session: &mut HlsSession) {
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1024, ready_at_ms: 10 };
        }
    }

    fn install_recovery_gate_fields(session: &mut HlsSession) {
        session.origin_control.manifest_origin_binding = Some(
            HlsManifestOriginBinding::new(
                Url::parse("https://origin.example/live/index.m3u8?token=test").expect("binding URL"),
                Some(0),
            )
            .expect("binding"),
        );
        session.origin_control.pinned_host = Some("origin.example".to_string());
        session.origin_seq_highwater = Some(125);
        session.origin_control.record_media_progress(10, 4_000);
    }

    fn rendered_manifest(first_proxy_seq: u64, last_proxy_seq: u64) -> RenderedManifest {
        RenderedManifest {
            body: format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{first_proxy_seq}\n"),
            first_proxy_seq,
            last_proxy_seq,
            discontinuity_sequence: 0,
            target_duration_ms: 4_000,
            playlist_duration_ms: 4_000,
            valid_until_ms: 4_000,
            render_gap_segments: 0,
            rendered_at_ms: first_proxy_seq,
            segment_proxy_seqs: (first_proxy_seq..=last_proxy_seq).collect(),
        }
    }

    #[test]
    fn rendering_an_empty_internal_window_does_not_panic() {
        let session = session();

        let rendered =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| super::render_window(&session, &[], 0, 0)));

        assert!(rendered.is_ok());
    }

    #[test]
    fn renderer_does_not_emit_origin_uri_or_file_name() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert!(!rendered.body.contains("origin.example.com"));
        assert!(!rendered.body.contains("origin-name-120.ts"));
        assert!(rendered.body.contains("/hls/shared/live/"));
    }

    #[test]
    fn renderer_emits_segment_local_key_rotation_and_explicit_clear_transition() {
        let mut manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"origin-a.key\",KEYFORMAT=\"identity\",KEYFORMATVERSIONS=\"1\"\n\
             #EXTINF:4,\n1.ts\n#EXTINF:4,\n2.ts\n\
             #EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\n3.ts\n#EXTINF:4,\n4.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"origin-b.key\"\n#EXTINF:4,\n5.ts\n#EXTINF:4,\n6.ts\n",
        );
        for encryption in manifest.segments.iter_mut().filter_map(|segment| segment.encryption.as_mut()) {
            encryption.proxy_resource_id = Some(
                if encryption.resolved_origin_uri.ends_with("origin-a.key") { "opaque-a" } else { "opaque-b" }
                    .to_string(),
            );
            encryption.proxy_resource_extension = Some("key".to_string());
        }
        let mut session = session();
        session.apply_origin_manifest(&manifest).expect("encrypted timeline maps");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest renders").body;
        let key_a = rendered.find("/r/opaque-a.key").expect("first key");
        let clear = rendered.find("#EXT-X-KEY:METHOD=NONE").expect("clear boundary");
        let key_b = rendered.find("/r/opaque-b.key").expect("rotated key");

        assert!(key_a < clear && clear < key_b);
        assert!(rendered.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        for iv in [
            "0x00000000000000000000000000000001",
            "0x00000000000000000000000000000002",
            "0x00000000000000000000000000000005",
            "0x00000000000000000000000000000006",
        ] {
            assert!(rendered.contains(&format!(",IV={iv}")), "missing materialized IV {iv}");
        }
        assert_eq!(rendered.matches("#EXT-X-KEY:METHOD=AES-128").count(), 4);
        assert!(rendered.contains("#EXT-X-VERSION:5\n"));
        assert!(!rendered.contains("origin-a.key"));
        assert!(!rendered.contains("origin-b.key"));
    }

    #[test]
    fn renderer_emits_six_digit_proxy_sequence_urls() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert!(rendered.body.contains("/000000.ts"));
    }

    #[test]
    fn media_sequence_is_first_visible_proxy_sequence() {
        let mut session = session();
        session.render_policy = RenderPolicy::new(1);
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert_eq!(rendered.first_proxy_seq, 0);
        assert!(rendered.body.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        assert_eq!(rendered.render_gap_segments, 0);
    }

    #[test]
    fn discontinuity_and_sequence_are_rendered_for_visible_window() {
        let mut session = session();
        let first = normal_manifest(
            "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:3\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\n3.ts\n#EXTINF:4.0,\n4.ts\n#EXTINF:4.0,\n5.ts\n#EXTINF:4.0,\n6.ts\n#EXTINF:4.0,\n7.ts\n",
        );
        session.apply_origin_manifest(&first).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert_eq!(rendered.first_proxy_seq, 1);
        assert_eq!(rendered.discontinuity_sequence, 4);
        assert_eq!(rendered.target_duration_ms, 4_000);
        assert!(rendered.body.contains("#EXT-X-DISCONTINUITY-SEQUENCE:4\n"));
        assert!(rendered.body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.000,\n/hls/shared/live/"));
    }

    #[test]
    fn provisioning_handoff_discontinuity_is_rendered_for_first_origin_segment() {
        let mut session = session();
        session.mark_pending_handoff_discontinuity(0);
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\n10.ts\n#EXTINF:4.0,\n11.ts\n#EXTINF:4.0,\n12.ts\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert!(rendered.body.contains("#EXT-X-DISCONTINUITY-SEQUENCE:7\n"));
        assert!(rendered.body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.000,\n/hls/shared/live/"));
        assert_eq!(rendered.body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
        assert_eq!(session.pending_handoff_discontinuity_sequence, None);
    }

    #[test]
    fn provisioning_handoff_does_not_duplicate_origin_first_segment_discontinuity() {
        let mut session = session();
        session.mark_pending_handoff_discontinuity(0);
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\n10.ts\n#EXTINF:4.0,\n11.ts\n#EXTINF:4.0,\n12.ts\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert!(rendered.body.contains("#EXT-X-DISCONTINUITY-SEQUENCE:7\n"));
        assert_eq!(rendered.body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    }

    #[test]
    fn byterange_is_not_rendered() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-BYTERANGE:500@1000\n#EXTINF:4.0,\nbig.m4s\n#EXT-X-BYTERANGE:500\n#EXTINF:4.0,\nbig.m4s\n#EXT-X-BYTERANGE:500\n#EXTINF:4.0,\nbig.m4s\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert!(!rendered.body.contains("#EXT-X-BYTERANGE"));
    }

    #[test]
    fn queued_and_fetching_segments_with_fetch_ref_are_renderable_with_limit() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n#EXTINF:4.0,\n4.ts\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.segments.get_mut(&0).expect("segment").status =
            SegmentCacheStatus::Ready { content_length: 100, ready_at_ms: 1 };
        session.segments.get_mut(&1).expect("segment").status =
            SegmentCacheStatus::Ready { content_length: 100, ready_at_ms: 1 };
        session.segments.get_mut(&2).expect("segment").status =
            SegmentCacheStatus::Queued { priority: SegmentFetchPriority::RenderWindow, queued_at_ms: 1 };
        session.segments.get_mut(&3).expect("segment").status =
            SegmentCacheStatus::Fetching { priority: SegmentFetchPriority::RenderWindow, started_at_ms: 1 };

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert_eq!(rendered.segment_proxy_seqs, vec![0, 1, 2, 3]);
    }

    #[test]
    fn max_not_ready_render_segments_blocks_too_many_queued_segments() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        for segment in session.segments.values_mut() {
            segment.status =
                SegmentCacheStatus::Queued { priority: SegmentFetchPriority::RenderWindow, queued_at_ms: 1 };
        }

        assert_eq!(HlsManifestRenderer::render(&session, 10), Err(RenderError::NoRenderableWindow));
    }

    #[test]
    fn map_not_ready_prevents_rendering_affected_segments() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n#EXTINF:4.0,\n2.m4s\n#EXTINF:4.0,\n3.m4s\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        assert_eq!(HlsManifestRenderer::render(&session, 10), Err(RenderError::NoRenderableWindow));
    }

    #[test]
    fn current_origin_manifest_head_blocks_suffix_fallback() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        session.segments.get_mut(&0).expect("segment 0").status = SegmentCacheStatus::Discovered;
        session.segments.get_mut(&1).expect("segment 1").status = SegmentCacheStatus::Discovered;

        assert_eq!(HlsManifestRenderer::render(&session, 10), Err(RenderError::NoRenderableWindow));
    }

    #[test]
    fn map_ready_renders_only_proxy_map_uri() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"origin-init-name.mp4\"\n#EXTINF:4.0,\n1.m4s\n#EXTINF:4.0,\n2.m4s\n#EXTINF:4.0,\n3.m4s\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        for map in session.maps.values_mut() {
            map.status = MapCacheStatus::Ready { content_length: 128, ready_at_ms: 10 };
        }

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert!(rendered.body.contains("/hls/shared/live/"));
        assert!(rendered.body.contains("/map/000000.mp4"));
        assert!(!rendered.body.contains("origin-init-name.mp4"));
    }

    #[test]
    fn map_tag_is_rendered_once_for_unchanged_map_ref() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n#EXTINF:4.0,\n2.m4s\n#EXTINF:4.0,\n3.m4s\n#EXTINF:4.0,\n4.m4s\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        for map in session.maps.values_mut() {
            map.status = MapCacheStatus::Ready { content_length: 128, ready_at_ms: 10 };
        }

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert_eq!(rendered.body.matches("#EXT-X-MAP").count(), 1);
    }

    #[test]
    fn map_tag_is_rendered_again_when_map_ref_changes() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init-a.mp4\"\n#EXTINF:4.0,\n1.m4s\n#EXTINF:4.0,\n2.m4s\n#EXTINF:4.0,\n3.m4s\n#EXT-X-MAP:URI=\"init-b.mp4\"\n#EXTINF:4.0,\n4.m4s\n#EXTINF:4.0,\n5.m4s\n#EXTINF:4.0,\n6.m4s\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        for map in session.maps.values_mut() {
            map.status = MapCacheStatus::Ready { content_length: 128, ready_at_ms: 10 };
        }

        let rendered = HlsManifestRenderer::render(&session, 10).expect("manifest should render");

        assert_eq!(rendered.body.matches("#EXT-X-MAP").count(), 2);
        assert!(rendered.body.contains("/map/000000.mp4"));
        assert!(rendered.body.contains("/map/000001.mp4"));
    }

    #[test]
    fn renderer_gap_is_relative_to_publishable_tail() {
        let mut session = session();
        session.render_policy = RenderPolicy::new(3);
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n#EXTINF:4.0,\n4.ts\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n5.m4s\n#EXTINF:4.0,\n6.m4s\n#EXTINF:4.0,\n7.m4s\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("older valid window should render");

        assert_eq!(session.publishable_origin_tail_proxy_seq, Some(6));
        assert_eq!(rendered.last_proxy_seq, 3);
        assert_eq!(rendered.render_gap_segments, 3);
        assert_eq!(rendered.segment_proxy_seqs, vec![0, 1, 2, 3]);
    }

    #[test]
    fn store_rendered_manifest_rejects_regressive_media_sequence() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        let previous = rendered_manifest(2, 7);
        session.last_rendered_manifest = Some(previous.clone());
        let candidate = HlsManifestRenderer::render(&session, 10).expect("real-origin candidate should render");

        assert_eq!(
            session.store_rendered_manifest(candidate),
            RenderedManifestStoreOutcome::Rejected(RenderedManifestStoreRejectReason::RegressiveMediaSequence {
                previous_first_proxy_seq: 2,
                candidate_first_proxy_seq: 0,
            })
        );
        assert_eq!(session.last_rendered_manifest, Some(previous));
        assert!(session.published_live_origin_baseline.is_none());
    }

    #[test]
    fn store_rendered_manifest_rejects_duplicate_media_identity_at_distinct_proxy_positions() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        let duplicated_fetch_ref = session.segments.get(&0).expect("first segment").origin_fetch_ref.clone();
        session.segments.get_mut(&1).expect("second segment").origin_fetch_ref = duplicated_fetch_ref;
        let candidate = HlsManifestRenderer::render(&session, 10).expect("candidate renders before store validation");

        assert_eq!(
            session.store_rendered_manifest(candidate),
            RenderedManifestStoreOutcome::Rejected(RenderedManifestStoreRejectReason::DuplicateMediaResource {
                existing_proxy_seq: 0,
                candidate_proxy_seq: 1,
            })
        );
        assert!(session.last_rendered_manifest.is_none());
        assert!(session.published_live_origin_baseline.is_none());
    }

    #[test]
    fn provisioning_and_gap_only_manifests_do_not_set_live_baseline() {
        for synthetic_epoch in [HLS_PROVISIONING_ORIGIN_EPOCH, HLS_PROVISIONING_GAP_ORIGIN_EPOCH] {
            let mut session = session();
            session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
            mark_all_segments_ready(&mut session);
            for segment in session.segments.values_mut() {
                segment.origin_key.origin_epoch = synthetic_epoch;
            }

            session.render_and_store_manifest(10).expect("synthetic manifest should store");

            assert!(session.last_rendered_manifest.is_some());
            assert!(session.published_live_origin_baseline.is_none());
        }
    }

    #[test]
    fn mixed_provisioning_and_origin_manifest_enables_recovery_binding() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        session.segments.get_mut(&0).expect("first segment").origin_key.origin_epoch = HLS_PROVISIONING_ORIGIN_EPOCH;
        install_recovery_gate_fields(&mut session);

        let stored = session.render_and_store_manifest(10).expect("mixed manifest should store");

        assert_eq!(
            session.published_live_origin_baseline,
            Some(HlsPublishedLiveOriginBaseline {
                evidence_proxy_seq: 1,
                origin_epoch: 0,
                rendered_at_ms: stored.rendered_at_ms,
            })
        );
        assert!(session.established_manifest_recovery_binding().is_some());
    }

    #[test]
    fn real_origin_manifest_enables_recovery_binding() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        install_recovery_gate_fields(&mut session);

        let stored = session.render_and_store_manifest(10).expect("origin manifest should store");

        assert_eq!(
            session.published_live_origin_baseline,
            Some(HlsPublishedLiveOriginBaseline {
                evidence_proxy_seq: 0,
                origin_epoch: 0,
                rendered_at_ms: stored.rendered_at_ms,
            })
        );
        assert!(session.established_manifest_recovery_binding().is_some());
    }

    #[test]
    fn no_renderable_window_does_not_set_live_baseline() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        for segment in session.segments.values_mut() {
            segment.origin_key.origin_epoch = HLS_PROVISIONING_ORIGIN_EPOCH;
        }
        let provisioning = session.render_and_store_manifest(10).expect("provisioning manifest should store");
        for segment in session.segments.values_mut() {
            segment.origin_key.origin_epoch = 1;
            segment.status = SegmentCacheStatus::Discovered;
        }

        assert_eq!(session.render_and_store_manifest(20), Err(RenderError::NoRenderableWindow));
        assert_eq!(session.last_rendered_manifest, Some(provisioning));
        assert!(session.published_live_origin_baseline.is_none());
    }

    #[test]
    fn published_live_baseline_survives_gc_of_evidence_segment() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        install_recovery_gate_fields(&mut session);
        session.render_and_store_manifest(10).expect("origin manifest should store");
        let evidence = session.published_live_origin_baseline.expect("live-origin evidence");
        assert_eq!(session.store_rendered_manifest(rendered_manifest(1, 5)), RenderedManifestStoreOutcome::Stored);
        assert_eq!(session.published_live_origin_baseline, Some(evidence));
        assert!(!session
            .last_rendered_manifest
            .as_ref()
            .expect("advanced manifest remains stored")
            .segment_proxy_seqs
            .contains(&evidence.evidence_proxy_seq));

        session.segments.remove(&evidence.evidence_proxy_seq);

        assert_eq!(session.published_live_origin_baseline, Some(evidence));
        assert!(session.established_manifest_recovery_binding().is_some());
    }

    #[test]
    fn render_and_store_manifest_returns_error_on_store_rejection() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        session.last_rendered_manifest = Some(rendered_manifest(2, 7));

        assert_eq!(
            session.render_and_store_manifest(10),
            Err(RenderError::StoreRejected(RenderedManifestStoreRejectReason::RegressiveMediaSequence {
                previous_first_proxy_seq: 2,
                candidate_first_proxy_seq: 0,
            }))
        );
        assert_eq!(session.last_rendered_manifest, Some(rendered_manifest(2, 7)));
    }

    #[test]
    fn store_rendered_manifest_accepts_same_or_forward_media_sequence() {
        let mut session = session();
        let first = rendered_manifest(2, 7);
        let same = rendered_manifest(2, 8);
        let forward = rendered_manifest(3, 9);

        assert_eq!(session.store_rendered_manifest(first), RenderedManifestStoreOutcome::Stored);
        assert_eq!(session.store_rendered_manifest(same.clone()), RenderedManifestStoreOutcome::Stored);
        assert_eq!(session.last_rendered_manifest, Some(same));
        assert_eq!(session.store_rendered_manifest(forward.clone()), RenderedManifestStoreOutcome::Stored);
        assert_eq!(session.last_rendered_manifest, Some(forward));
    }

    #[test]
    fn renderer_does_not_exceed_initial_render_gap_to_find_valid_window() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        let first = session.render_and_store_manifest(10).expect("initial manifest should render");

        session.render_policy = RenderPolicy::new(2);
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:126\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n126.m4s\n#EXTINF:4.0,\n127.m4s\n#EXTINF:4.0,\n128.m4s\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        assert_eq!(HlsManifestRenderer::render(&session, 20), Err(RenderError::NoRenderableWindow));
        assert_eq!(session.render_and_store_manifest(20), Err(RenderError::NoRenderableWindow));
        assert_eq!(session.last_rendered_manifest, Some(first));
    }

    #[test]
    fn invalid_render_does_not_replace_last_rendered_manifest() {
        let mut session = session();
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
        mark_all_segments_ready(&mut session);
        let first = session.render_and_store_manifest(10).expect("manifest should render");
        session.segments.clear();

        assert_eq!(session.render_and_store_manifest(20), Err(RenderError::NoRenderableWindow));
        assert_eq!(session.last_rendered_manifest, Some(first));
    }

    #[test]
    fn render_policy_from_seconds_counts_tail_durations() {
        let policy = RenderPolicy::from_strip_config(
            &StripConfig { mode: HlsStripMode::Seconds, value: 9 },
            &[4_000, 4_000, 4_000, 4_000],
        );

        assert_eq!(policy.initial_render_gap_segments, 3);
    }

    #[test]
    fn hls_startup_policy_renderer_uses_shared_clamped_threshold() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
             #EXTINF:2.0,\n1.ts\n#EXTINF:2.0,\n2.ts\n#EXTINF:2.0,\n3.ts\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("short visible playlist should render");

        assert_eq!(rendered.target_duration_ms, 4_000);
        assert_eq!(rendered.playlist_duration_ms, 6_000);
        assert_eq!(
            crate::media_reserve::minimum_hls_startup_window_ms(
                rendered.target_duration_ms,
                rendered.playlist_duration_ms
            ),
            6_000
        );
    }

    #[test]
    fn hls_startup_policy_renderer_uses_advertised_duration_not_larger_origin_window() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
             #EXTINF:1.0,\n1.ts\n#EXTINF:1.0,\n2.ts\n#EXTINF:1.0,\n3.ts\n#EXTINF:1.0,\n4.ts\n\
             #EXTINF:1.0,\n5.ts\n#EXTINF:1.0,\n6.ts\n#EXTINF:1.0,\n7.ts\n",
        );
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        mark_all_segments_ready(&mut session);

        let rendered = HlsManifestRenderer::render(&session, 10).expect("six-second advertised playlist should render");

        assert_eq!(rendered.target_duration_ms, 4_000);
        assert_eq!(rendered.playlist_duration_ms, 6_000);
        assert_eq!(rendered.segment_proxy_seqs.len(), 6);
        assert_eq!(
            crate::media_reserve::minimum_hls_startup_window_ms(
                rendered.target_duration_ms,
                rendered.playlist_duration_ms
            ),
            6_000
        );
    }
}
