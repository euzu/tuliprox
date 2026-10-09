use super::{
    clock::{
        add_pts_dts_offset, decode_pcr_at_location, ticks_90khz_to_rounded_millis, ts_chunk_packet_count,
        TS_PACKET_SIZE,
    },
    layout::{build_hls_finite_ts_layout, find_ts_alignment, finite_hls_presentation_duration},
    profile::timestamp_profile_from_finite_layout,
    HlsTsTimestampProfile, TransportStreamBuffer,
};
use bytes::{Bytes, BytesMut};
use futures::task::AtomicWaker;
use sha2::{Digest, Sha256};
use std::sync::Arc;

impl std::fmt::Debug for TransportStreamBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportStreamBuffer")
            .field("length", &self.length)
            .field("current_pos", &self.current_pos)
            .field("current_dts", &self.current_dts)
            .field("timestamp_offset", &self.timestamp_offset)
            .field("finite_hls_presentation_duration", &self.finite_hls_presentation_duration)
            .field("first_pcr", &self.first_pcr)
            .field("finite_hls_timestamp_profile", &self.finite_hls_timestamp_profile)
            .field("force_discontinuity_on_wrap", &self.force_discontinuity_on_wrap)
            .finish_non_exhaustive()
    }
}

impl Clone for TransportStreamBuffer {
    fn clone(&self) -> Self {
        Self {
            buffer: self.buffer.clone(),
            packet_starts: Arc::clone(&self.packet_starts),
            finite_hls_layout: self.finite_hls_layout.clone(),
            finite_hls_presentation_duration: self.finite_hls_presentation_duration.clone(),
            current_pos: 0,
            current_dts: 0,
            timestamp_offset: 0,
            length: self.length,
            // Each clone starts with a fresh CC state; the discontinuity packets at the first
            // loop boundary will signal decoders to reset their CC expectations.
            cc_entries: Box::new([None; 8192]),
            waker: Arc::clone(&self.waker),
            first_pcr: self.first_pcr,
            finite_hls_timestamp_profile: self.finite_hls_timestamp_profile,
            finite_hls_track_signature: self.finite_hls_track_signature.clone(),
            finite_hls_asset_fingerprint: self.finite_hls_asset_fingerprint,
            #[cfg(any(test, feature = "test-support"))]
            finite_hls_render_count: Arc::clone(&self.finite_hls_render_count),
            #[cfg(any(test, feature = "test-support"))]
            finite_hls_finalize_count: Arc::clone(&self.finite_hls_finalize_count),
            // Start each clone with one initial discontinuity marker to keep
            // continuity behavior consistent for fresh consumers.
            force_discontinuity_on_wrap: true,
        }
    }
}

impl TransportStreamBuffer {
    pub fn new(mut raw: Vec<u8>) -> Self {
        let offset = find_ts_alignment(&raw).unwrap_or(0);
        raw.drain(..offset);

        // Remove trailing partial packets
        let valid_length = (raw.len() / TS_PACKET_SIZE) * TS_PACKET_SIZE;
        raw.truncate(valid_length);

        let length = raw.len() / TS_PACKET_SIZE;
        let packet_starts =
            (0..length).map(|packet_index| packet_index.saturating_mul(TS_PACKET_SIZE)).collect::<Arc<[_]>>();
        let finite_hls_layout = build_hls_finite_ts_layout(&raw).map(Arc::new);
        let finite_hls_presentation_duration =
            finite_hls_layout.as_ref().ok().and_then(|layout| finite_hls_presentation_duration(&raw, layout).ok());
        let finite_hls_track_signature = match crate::ts_inspector::inspect_mpeg_ts(
            std::io::Cursor::new(&raw),
            crate::ts_inspector::HlsTsProbeProtection::Clear,
            crate::ts_inspector::HlsTsProbeBudget::default(),
        ) {
            Ok(crate::ts_inspector::HlsTsProbeOutcome::Found(signature)) => Some(signature),
            Ok(
                crate::ts_inspector::HlsTsProbeOutcome::ProbeBudgetExhausted { .. }
                | crate::ts_inspector::HlsTsProbeOutcome::Malformed(_)
                | crate::ts_inspector::HlsTsProbeOutcome::UnsupportedProtection(_),
            )
            | Err(_) => None,
        };
        let finite_hls_asset_fingerprint = Sha256::digest(&raw).into();
        let finite_hls_timestamp_profile = finite_hls_layout.as_ref().ok().and_then(|layout| {
            finite_hls_presentation_duration
                .as_ref()
                .and_then(|duration| timestamp_profile_from_finite_layout(&raw, layout, duration.duration_ticks_90khz))
        });
        let first_pcr = finite_hls_layout
            .as_ref()
            .ok()
            .and_then(|layout| layout.pcr_fields.first().copied())
            .and_then(|location| decode_pcr_at_location(&raw, location).ok());

        Self {
            buffer: Bytes::from(raw),
            current_pos: 0,
            current_dts: 0,
            timestamp_offset: 0,
            length,
            packet_starts,
            finite_hls_layout,
            finite_hls_presentation_duration,
            cc_entries: Box::new([None; 8192]),
            waker: Arc::new(AtomicWaker::new()),
            first_pcr,
            finite_hls_timestamp_profile,
            finite_hls_track_signature,
            finite_hls_asset_fingerprint,
            #[cfg(any(test, feature = "test-support"))]
            finite_hls_render_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(any(test, feature = "test-support"))]
            finite_hls_finalize_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            // Emit a discontinuity packet on startup. Subsequent injections are
            // governed by wrap handling and duration/discontinuity logic.
            force_discontinuity_on_wrap: true,
        }
    }

    pub fn as_bytes(&self) -> &[u8] { &self.buffer }

    /// Cheap clone of the underlying buffer as `Bytes` (refcount bump).
    /// Use this from response builders to avoid `Bytes::copy_from_slice(&[u8])`.
    pub fn clone_bytes(&self) -> Bytes { self.buffer.clone() }

    pub fn duration_ms(&self) -> Option<u64> { self.duration_ticks_90khz().and_then(ticks_90khz_to_rounded_millis) }

    pub fn duration_ticks_90khz(&self) -> Option<u64> {
        self.finite_hls_presentation_duration.as_ref().map(|duration| duration.duration_ticks_90khz)
    }

    pub const fn finite_hls_timestamp_profile(&self) -> Option<HlsTsTimestampProfile> {
        self.finite_hls_timestamp_profile
    }

    pub fn finite_hls_track_signature(&self) -> Option<crate::ts_inspector::HlsTsTrackSignature> {
        self.finite_hls_track_signature.clone()
    }

    pub const fn has_finite_hls_track_signature(&self) -> bool { self.finite_hls_track_signature.is_some() }

    pub const fn finite_hls_asset_fingerprint(&self) -> [u8; 32] { self.finite_hls_asset_fingerprint }

    #[cfg(any(test, feature = "test-support"))]
    pub fn finite_hls_render_count(&self) -> usize {
        self.finite_hls_render_count.load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn finite_hls_finalize_count(&self) -> usize {
        self.finite_hls_finalize_count.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Returns next chunks with adjusted PTS/DTS and PCR.
    /// All timestamp rewrites are performed in-place on the `BytesMut` output buffer to avoid
    /// per-packet heap allocations. PID continuity-counter lookup is O(1) via a fixed 8192-entry array.
    pub fn next_chunk(&mut self) -> Option<Bytes> {
        if self.length == 0 {
            return None;
        }
        let packet_count = ts_chunk_packet_count();
        let mut bytes = BytesMut::with_capacity(TS_PACKET_SIZE * packet_count);
        let mut packets_remaining = packet_count;

        while packets_remaining > 0 {
            if self.current_pos >= self.length {
                // Loop back — advance timestamp offset so PTS/DTS/PCR remain monotonically
                // increasing across loops. Resetting to 0 causes decoders (MPV, ffmpeg) to see
                // a backward timestamp jump and treat the loop as end-of-stream or corrupt data.
                self.current_pos = 0;
                // Advance timestamps by one full source duration per loop so output time is
                // monotonic for clients. Resetting to zero causes backward jumps that some
                // players interpret as stream end/corruption after the first cycle.
                if let Some(stream_duration_90khz) = self.duration_ticks_90khz() {
                    self.timestamp_offset = add_pts_dts_offset(self.timestamp_offset, stream_duration_90khz);
                    self.current_dts = add_pts_dts_offset(self.current_dts, stream_duration_90khz);
                } else {
                    // PCR-only (or malformed) assets may not expose PTS/DTS-derived duration.
                    // Force one discontinuity marker after wrap so decoders do not see identical
                    // timestamp cycles as a continuous timeline.
                    self.force_discontinuity_on_wrap = true;
                }

                // Reset only the discontinuity-sent flag so injection packets are emitted at the
                // start of the next loop. Continuity counter values keep running so CC remains
                // globally monotonic across loops.
                for entry in self.cc_entries.iter_mut().flatten() {
                    entry.1 = false;
                }
            }

            let current_pos = self.current_pos;
            let packet_start = self.packet_starts[current_pos];
            let packet = &self.buffer[packet_start..packet_start + TS_PACKET_SIZE];
            let packet_layout =
                self.finite_hls_layout.as_ref().ok().and_then(|layout| layout.packets.get(current_pos)).copied();
            let packet_has_payload = packet_layout
                .map_or_else(|| matches!((packet[3] >> 4) & 0b11, 0b01 | 0b11), |layout| layout.has_payload);

            // O(1) PID lookup — PID is at most 13 bits (0–8191).
            let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
            let entry = &mut self.cc_entries[pid as usize];
            // Normalize payload continuity per PID to a clean local sequence.
            let (counter, discontinuity_sent) = entry.get_or_insert((0, false));

            // Disable synthetic discontinuity packet insertion for looped custom streams.
            // In practice this can produce demuxer corruption on some clients (PES mismatch).
            // Monotonic timestamps + stable continuity counters are sufficient here.
            let inject_discontinuity = self.force_discontinuity_on_wrap && !*discontinuity_sent;
            if inject_discontinuity {
                let extra_packet_cc = if packet_has_payload { *counter } else { packet[3] & 0x0F };
                Self::generate_discontinuity_packet(
                    packet,
                    extra_packet_cc,
                    self.first_pcr,
                    self.timestamp_offset,
                    &mut bytes,
                );
                self.force_discontinuity_on_wrap = false;
            }
            *discontinuity_sent = true;
            let payload_packet_cc = if packet_has_payload { *counter } else { packet[3] & 0x0F };

            // TS continuity counter increments only when payload is present (AFC=01/11).
            if packet_has_payload {
                *counter = (*counter + 1) % 16;
            }

            // Append the original packet into `bytes`, then mutate the appended slice in-place.
            let pkt_start = bytes.len();
            bytes.extend_from_slice(packet);

            // Apply the computed CC to the payload packet.
            bytes[pkt_start + 3] = (bytes[pkt_start + 3] & 0xF0) | (payload_packet_cc & 0x0F);

            if let (Ok(layout), Some(packet_layout)) = (&self.finite_hls_layout, packet_layout) {
                if self
                    .rewrite_source_packet_timestamps(
                        &mut bytes,
                        pkt_start,
                        layout,
                        packet_layout,
                        self.timestamp_offset,
                    )
                    .is_err()
                {
                    return None;
                }
            }

            self.current_pos += 1;
            packets_remaining -= 1;
        }

        Some(bytes.freeze())
    }
}
