use super::{
    clock::{
        add_pcr_offset_27mhz, add_pts_dts_offset, append_finite_discontinuity_packet, decode_pcr_at_location,
        decode_timestamp, encode_pcr, encode_timestamp, gather_timestamp_bytes, pcr_offset_27mhz,
        same_finite_ts_packet_layout, ts_packet_pid, ADAPTATION_FIELD_FLAG_PCR, NULL_PID, SYNC_BYTE, TS_PACKET_SIZE,
    },
    pes::HlsFiniteTsLayoutError,
    HlsFiniteTsLayout, HlsFiniteTsPacketLayout, TransportStreamBuffer,
};
use bytes::{Bytes, BytesMut};
use std::task::Waker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsFiniteTsRenderSpec {
    pub timestamp_offset_ticks_90khz: u64,
    /// Seed used by logical segment zero. Later segments derive each PID's starting counter from
    /// the number of payload packets for that PID in one immutable asset cycle.
    pub continuity_seed: u8,
    pub logical_segment_index: u16,
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum HlsFiniteTsRenderError {
    #[error("transport stream asset is empty or invalid")]
    InvalidAsset,
    #[error("prepared transport stream does not match the immutable asset packet layout")]
    PreparedLayoutMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsFiniteTsDiscontinuityMode {
    None,
    FirstPacketPerPid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlsFiniteTsFinalizeSpec {
    pub additional_timestamp_offset_ticks_90khz: u64,
    pub discontinuity: HlsFiniteTsDiscontinuityMode,
}

impl TransportStreamBuffer {
    /// Renders one immutable, finite HLS media segment without mutating the looping stream state.
    pub fn render_finite_hls_segment(&self, spec: HlsFiniteTsRenderSpec) -> Result<Bytes, HlsFiniteTsRenderError> {
        #[cfg(any(test, feature = "test-support"))]
        self.finite_hls_render_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let layout = self.finite_hls_layout.as_ref().map_err(|_| HlsFiniteTsRenderError::InvalidAsset)?;
        if self.buffer.is_empty() || layout.packets.is_empty() {
            return Err(HlsFiniteTsRenderError::InvalidAsset);
        }
        let mut output = BytesMut::with_capacity(self.buffer.len());
        let mut payload_counts = [0_u8; 8192];
        for packet_layout in layout.packets.iter().copied() {
            if packet_layout.has_payload {
                let count = &mut payload_counts[usize::from(packet_layout.pid)];
                *count = count.wrapping_add(1) & 0x0F;
            }
        }
        let mut continuity = [None; 8192];
        for packet_layout in layout.packets.iter().copied() {
            let packet_end = packet_layout.packet_start.saturating_add(TS_PACKET_SIZE);
            let Some(packet) = self.buffer.get(packet_layout.packet_start..packet_end) else {
                return Err(HlsFiniteTsRenderError::InvalidAsset);
            };
            let output_start = output.len();
            output.extend_from_slice(packet);
            let counter = continuity[usize::from(packet_layout.pid)].get_or_insert_with(|| {
                let cycle_advance = payload_counts[usize::from(packet_layout.pid)]
                    .wrapping_mul((spec.logical_segment_index & 0x0F) as u8)
                    & 0x0F;
                spec.continuity_seed.wrapping_add(cycle_advance) & 0x0F
            });
            output[output_start + 3] = (output[output_start + 3] & 0xF0) | *counter;
            if packet_layout.has_payload {
                *counter = counter.wrapping_add(1) & 0x0F;
            }
        }
        Self::rewrite_layout_timestamps(&mut output, layout, spec.timestamp_offset_ticks_90khz)
            .map_err(|_| HlsFiniteTsRenderError::InvalidAsset)?;
        Ok(output.freeze())
    }

    /// Applies one lease-specific timestamp anchor to an already prepared relative segment.
    ///
    /// The immutable source packet layout is verified before any bytes are published. Optional
    /// splice markers are adaptation-only packets and therefore do not advance payload CC.
    pub fn finalize_prepared_finite_hls_segment(
        &self,
        prepared: &Bytes,
        spec: HlsFiniteTsFinalizeSpec,
    ) -> Result<Bytes, HlsFiniteTsRenderError> {
        #[cfg(any(test, feature = "test-support"))]
        self.finite_hls_finalize_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let layout = self.finite_hls_layout.as_ref().map_err(|_| HlsFiniteTsRenderError::PreparedLayoutMismatch)?;
        if self.buffer.is_empty()
            || layout.packets.is_empty()
            || prepared.len() != layout.packets.len().saturating_mul(TS_PACKET_SIZE)
        {
            return Err(HlsFiniteTsRenderError::PreparedLayoutMismatch);
        }
        let mut first_pid_seen = [false; 8192];
        let mut marker_count = 0usize;
        for (position, packet_layout) in layout.packets.iter().enumerate() {
            let prepared_start = position.saturating_mul(TS_PACKET_SIZE);
            let Some(source) =
                self.buffer.get(packet_layout.packet_start..packet_layout.packet_start.saturating_add(TS_PACKET_SIZE))
            else {
                return Err(HlsFiniteTsRenderError::PreparedLayoutMismatch);
            };
            let Some(candidate) = prepared.get(prepared_start..prepared_start.saturating_add(TS_PACKET_SIZE)) else {
                return Err(HlsFiniteTsRenderError::PreparedLayoutMismatch);
            };
            if !same_finite_ts_packet_layout(source, candidate) {
                return Err(HlsFiniteTsRenderError::PreparedLayoutMismatch);
            }
            let pid = ts_packet_pid(candidate);
            if spec.discontinuity == HlsFiniteTsDiscontinuityMode::FirstPacketPerPid
                && pid != NULL_PID
                && !first_pid_seen[usize::from(pid)]
            {
                first_pid_seen[usize::from(pid)] = true;
                marker_count = marker_count.saturating_add(1);
            }
        }

        let mut rewritten = BytesMut::from(prepared.as_ref());
        Self::rewrite_layout_timestamps(&mut rewritten, layout, spec.additional_timestamp_offset_ticks_90khz)
            .map_err(|_| HlsFiniteTsRenderError::PreparedLayoutMismatch)?;
        first_pid_seen.fill(false);
        let additional_bytes = marker_count.saturating_mul(TS_PACKET_SIZE);
        let mut output = BytesMut::with_capacity(prepared.len().saturating_add(additional_bytes));
        for position in 0..layout.packets.len() {
            let prepared_start = position.saturating_mul(TS_PACKET_SIZE);
            let packet = &rewritten[prepared_start..prepared_start + TS_PACKET_SIZE];
            let pid = ts_packet_pid(packet);
            if spec.discontinuity == HlsFiniteTsDiscontinuityMode::FirstPacketPerPid
                && pid != NULL_PID
                && !first_pid_seen[usize::from(pid)]
            {
                append_finite_discontinuity_packet(packet, &mut output);
                first_pid_seen[usize::from(pid)] = true;
            }
            output.extend_from_slice(packet);
        }
        Ok(output.freeze())
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn estimated_bitrate_kbps(&self) -> Option<usize> {
        let stream_duration_90khz = self.duration_ticks_90khz()?;
        if self.buffer.is_empty() {
            return None;
        }
        let duration_secs = stream_duration_90khz as f64 / 90_000.0;
        if duration_secs <= 0.0 {
            return None;
        }
        let kbps = ((self.buffer.len() as f64 * 8.0) / duration_secs / 1_000.0).round();
        if !kbps.is_finite() || kbps <= 0.0 {
            return None;
        }
        Some(kbps as usize)
    }

    pub fn register_waker(&self, waker: &Waker) { self.waker.register(waker); }

    /// Sets the timestamp offset used when rewriting PTS/DTS/PCR values.
    pub fn set_timestamp_offset(&mut self, offset: u64) { self.timestamp_offset = offset; }

    /// Generates a Discontinuity packet for the given packet/PID state, writing it directly into `out`.
    pub(super) fn generate_discontinuity_packet(
        new_packet: &[u8],
        cc: u8,
        first_pcr: Option<u64>,
        timestamp_offset: u64,
        out: &mut BytesMut,
    ) {
        let start = out.len();
        out.resize(start + TS_PACKET_SIZE, 0xFF);
        let pkt = &mut out[start..start + TS_PACKET_SIZE];

        pkt[0] = SYNC_BYTE;
        pkt[1] = new_packet[1] & 0x1F;
        pkt[2] = new_packet[2];

        // Check if the current packet has a PCR (need at least 7 adaptation bytes: 1 flag + 6 PCR).
        let new_pkt_has_pcr = {
            let afc = (new_packet[3] >> 4) & 0b11;
            if afc == 2 || afc == 3 {
                let adaptation_len = new_packet[4] as usize;
                adaptation_len >= 7 && (new_packet[5] & ADAPTATION_FIELD_FLAG_PCR) != 0
            } else {
                false
            }
        };

        // AFC=2 (Adaptation Only), Scrambling=00 (Unscrambled), CC=cc
        pkt[3] = 0x20 | (cc & 0x0F);

        // Adaptation Field covers rest of packet (183 bytes)
        pkt[4] = 183;

        // If we contain a PCR, inject it. Otherwise just Discontinuity.
        if new_pkt_has_pcr {
            if let Some(base_pcr) = first_pcr {
                pkt[5] = 0x80 | 0x10; // Discontinuity (0x80) + PCR Flag (0x10)
                let offset_27mhz = pcr_offset_27mhz(timestamp_offset);
                let new_pcr = add_pcr_offset_27mhz(base_pcr, offset_27mhz);
                let pcr_bytes = encode_pcr(new_pcr);
                pkt[6..12].copy_from_slice(&pcr_bytes);
            } else {
                pkt[5] = 0x80;
            }
        } else {
            pkt[5] = 0x80; // Discontinuity Indicator Only
        }
    }

    pub(super) fn rewrite_layout_timestamps(
        bytes: &mut BytesMut,
        layout: &HlsFiniteTsLayout,
        timestamp_offset: u64,
    ) -> Result<(), HlsFiniteTsLayoutError> {
        if timestamp_offset == 0 {
            return Ok(());
        }
        for location in layout.pcr_fields.iter().copied() {
            let original = decode_pcr_at_location(bytes, location)?;
            let adjusted = add_pcr_offset_27mhz(original, pcr_offset_27mhz(timestamp_offset));
            let end = location.byte_offset.checked_add(6).ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let destination =
                bytes.get_mut(location.byte_offset..end).ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            destination.copy_from_slice(&encode_pcr(adjusted));
        }
        for location in layout.timestamp_fields.iter().copied() {
            let original = gather_timestamp_bytes(bytes, location)?;
            let prefix = original[0] & 0xF0;
            let mut encoded = encode_timestamp(add_pts_dts_offset(decode_timestamp(&original), timestamp_offset));
            encoded[0] = (encoded[0] & 0x0F) | prefix;
            for (marker_index, (source_offset, value)) in location.byte_offsets.into_iter().zip(encoded).enumerate() {
                let destination =
                    bytes.get_mut(source_offset).ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
                *destination = value;
                if matches!(marker_index, 0 | 2 | 4) && value & 1 == 0 {
                    return Err(HlsFiniteTsLayoutError::InvalidPesTimestampField {
                        pid: location.pid,
                        kind: location.kind,
                    });
                }
            }
        }
        Ok(())
    }

    pub(super) fn rewrite_source_packet_timestamps(
        &self,
        bytes: &mut BytesMut,
        output_start: usize,
        layout: &HlsFiniteTsLayout,
        packet_layout: HlsFiniteTsPacketLayout,
        timestamp_offset: u64,
    ) -> Result<(), HlsFiniteTsLayoutError> {
        if timestamp_offset == 0 {
            return Ok(());
        }
        let packet_end = packet_layout.packet_start.saturating_add(TS_PACKET_SIZE);
        if let Some(pcr_field_index) = packet_layout.pcr_field_index {
            let location = layout
                .pcr_fields
                .get(pcr_field_index)
                .copied()
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let original = decode_pcr_at_location(&self.buffer, location)?;
            let adjusted = add_pcr_offset_27mhz(original, pcr_offset_27mhz(timestamp_offset));
            let relative_offset = location
                .byte_offset
                .checked_sub(packet_layout.packet_start)
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let destination_start =
                output_start.checked_add(relative_offset).ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let destination_end =
                destination_start.checked_add(6).ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            bytes
                .get_mut(destination_start..destination_end)
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?
                .copy_from_slice(&encode_pcr(adjusted));
        }
        let field_indices = layout
            .packet_timestamp_field_indices
            .get(packet_layout.timestamp_field_indices_start..packet_layout.timestamp_field_indices_end)
            .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
        for field_index in field_indices.iter().copied() {
            let location = layout
                .timestamp_fields
                .get(field_index)
                .copied()
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let original = gather_timestamp_bytes(&self.buffer, location)?;
            let prefix = original[0] & 0xF0;
            let mut encoded = encode_timestamp(add_pts_dts_offset(decode_timestamp(&original), timestamp_offset));
            encoded[0] = (encoded[0] & 0x0F) | prefix;
            for (source_offset, value) in location.byte_offsets.into_iter().zip(encoded) {
                if !(packet_layout.packet_start..packet_end).contains(&source_offset) {
                    continue;
                }
                let relative_offset = source_offset
                    .checked_sub(packet_layout.packet_start)
                    .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
                let destination_offset = output_start
                    .checked_add(relative_offset)
                    .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
                *bytes.get_mut(destination_offset).ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)? = value;
            }
        }
        Ok(())
    }
}
