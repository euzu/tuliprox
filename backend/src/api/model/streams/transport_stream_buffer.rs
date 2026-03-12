use bytes::{Bytes, BytesMut};
use futures::task::AtomicWaker;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    task::Waker,
};

// PCR wraps at 2^33 * 300 (base is 33-bit, multiplied by 300 to get 27 MHz units).
// Using 1<<42 was slightly too large and could cause strict-decoder issues on hardware
// that computes modulo 2^33 on the base before multiplying.
const MAX_PCR: u64 = (1u64 << 33) * 300;
const MAX_PTS_DTS: u64 = 1 << 33; // 33 bit PTS/DTS cycle

const TS_PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PACKET_COUNT: usize = 7; // Reduced from 250 to 7 (1316 bytes) to prevent latency/timeout on low-bitrate streams
const CHUNK_SIZE: usize = TS_PACKET_SIZE * PACKET_COUNT;

const ADAPTATION_FIELD_FLAG_PCR: u8 = 0x10; // PCR flag bit in adaptation field flags

/// Byte offset of PTS within a PES payload (after the 3-byte start code, `stream_id`, length, flags).
const PES_PTS_OFFSET: usize = 9;
/// Byte offset of DTS within a PES payload when both PTS and DTS are present.
const PES_DTS_OFFSET: usize = 14;

/// Decodes a 5-byte DTS/PTS field from PES header into u64 timestamp.
fn decode_timestamp(ts_bytes: &[u8]) -> u64 {
    (((u64::from(ts_bytes[0]) >> 1) & 0x07) << 30)
        | (u64::from(ts_bytes[1]) << 22)
        | (((u64::from(ts_bytes[2]) >> 1) & 0x7F) << 15)
        | (u64::from(ts_bytes[3]) << 7)
        | ((u64::from(ts_bytes[4]) >> 1) & 0x7F)
}

/// Encodes a u64 timestamp into 5-byte PES DTS/PTS field
fn encode_timestamp(ts: u64) -> [u8; 5] {
    [
        0x20 | ((((ts >> 30) & 0x07) as u8) << 1) | 1,
        ((ts >> 22) & 0xFF) as u8,
        ((((ts >> 15) & 0x7F) as u8) << 1) | 1,
        ((ts >> 7) & 0xFF) as u8,
        (((ts & 0x7F) as u8) << 1) | 1,
    ]
}

/// Decode PCR from 6 bytes (adaptation field) into 42-bit PCR base + 9-bit extension as u64
fn decode_pcr(pcr_bytes: &[u8]) -> u64 {
    let pcr_base = (u64::from(pcr_bytes[0]) << 25)
        | ((u64::from(pcr_bytes[1])) << 17)
        | ((u64::from(pcr_bytes[2])) << 9)
        | ((u64::from(pcr_bytes[3])) << 1)
        | ((u64::from(pcr_bytes[4])) >> 7);
    let pcr_ext = ((u64::from(pcr_bytes[4]) & 1) << 8) | u64::from(pcr_bytes[5]);
    pcr_base * 300 + pcr_ext
}

/// Encode PCR timestamp (u64) back into 6 bytes
#[allow(clippy::cast_possible_truncation)]
fn encode_pcr(pcr: u64) -> [u8; 6] {
    let pcr_base = pcr / 300;
    let pcr_ext = pcr % 300;

    [
        ((pcr_base >> 25) & 0xFF) as u8,
        ((pcr_base >> 17) & 0xFF) as u8,
        ((pcr_base >> 9) & 0xFF) as u8,
        ((pcr_base >> 1) & 0xFF) as u8,
        // Bit 7 = bit0 of pcr_base, Bits 6-1 reserved '111111', Bit 0 = high bit of pcr_ext
        (((pcr_base & 1) << 7) as u8) | 0x7E | (((pcr_ext >> 8) & 1) as u8),
        (pcr_ext & 0xFF) as u8,
    ]
}

type TsInfoExtraction = (Vec<(usize, Option<(usize, Option<usize>, u16)>)>, Vec<(u16, u8)>);

/// Extracts PTS and DTS info from MPEG-TS data.
/// Returns a vector of tuples containing:
/// - the start offset of each TS packet within the data,
/// - an optional tuple with the PTS offset, DTS offset (both relative to the packet start),
///   and the lower 16 bits of the DTS difference compared to the previous DTS.
pub fn extract_pts_dts_indices_with_continuity(ts_data: &[u8]) -> TsInfoExtraction {
    let length = ts_data.len();
    let mut result = Vec::with_capacity(length / TS_PACKET_SIZE);
    let mut i = 0;

    let mut continuity_counters: HashMap<u16, u8> = HashMap::new();

    let mut first_dts: Option<usize> = None;
    let mut last_dts: u64 = 0;
    let mut sum_diff: u64 = 0;

    while i + TS_PACKET_SIZE <= length {
        if ts_data[i] != SYNC_BYTE {
            i += 1;
            continue;
        }

        let packet = &ts_data[i..i + TS_PACKET_SIZE];
        let pid = ((u16::from(packet[1]) & 0x1F) << 8) | u16::from(packet[2]);

        // Set Continuity Counter for this PID
        let counter = continuity_counters.entry(pid).or_insert(0);
        // packet[3] = (packet[3] & 0xF0) | (*counter & 0x0F);
        *counter = (*counter + 1) % 16;

        let pusi = (packet[1] & 0x40) != 0;

        if !pusi {
            result.push((i, None));
            i += TS_PACKET_SIZE;
            continue;
        }

        let adaptation_field_control = (packet[3] >> 4) & 0b11;
        let mut payload_offset = 4;

        if adaptation_field_control == 3 {
            let adaptation_field_length = packet[4] as usize;
            payload_offset += 1 + adaptation_field_length;
        }

        if payload_offset >= TS_PACKET_SIZE {
            result.push((i, None));
            i += TS_PACKET_SIZE;
            continue;
        }

        let payload = &packet[payload_offset..];

        // Need at least PES_PTS_OFFSET+5 bytes to safely read the PTS field.
        // This also guarantees payload_offset + PES_PTS_OFFSET + 5 <= TS_PACKET_SIZE.
        if payload.len() >= PES_PTS_OFFSET + 5 && payload.starts_with(&[0x00, 0x00, 0x01]) {
            let flags = payload[7];
            let pts_dts_flags = (flags >> 6) & 0b11;

            if pts_dts_flags == 0b11 {
                // PTS at PES_PTS_OFFSET, DTS at PES_DTS_OFFSET
                // Guard: need PES_DTS_OFFSET+5 bytes in the payload and the slice must fit in the packet.
                if payload.len() >= PES_DTS_OFFSET + 5
                    && payload_offset + PES_DTS_OFFSET + 5 <= TS_PACKET_SIZE
                {
                    let pts_offset_in_packet = payload_offset + PES_PTS_OFFSET;
                    let dts_offset_in_packet = payload_offset + PES_DTS_OFFSET;

                    let dts_bytes = &packet[dts_offset_in_packet..dts_offset_in_packet + 5];
                    let dts = decode_timestamp(dts_bytes);
                    let diff = if last_dts > 0 { dts.wrapping_sub(last_dts) } else { 0 };
                    sum_diff = sum_diff.wrapping_add(diff);
                    last_dts = dts;
                    if first_dts.is_none() {
                        first_dts = Some(result.len());
                    }

                    result.push((i, Some((pts_offset_in_packet, Some(dts_offset_in_packet), (diff & 0xFFFF) as u16))));
                } else {
                    result.push((i, None));
                }
            } else if pts_dts_flags == 0b10 {
                // PTS only — DTS = PTS for timing purposes
                // Guard: PES_PTS_OFFSET+5 bytes already confirmed by the outer check.
                if payload_offset + PES_PTS_OFFSET + 5 <= TS_PACKET_SIZE {
                    let pts_offset_in_packet = payload_offset + PES_PTS_OFFSET;
                    let pts_bytes = &packet[pts_offset_in_packet..pts_offset_in_packet + 5];
                    let dts = decode_timestamp(pts_bytes); // use PTS as DTS approximation

                    let diff = if last_dts > 0 { dts.wrapping_sub(last_dts) } else { 0 };
                    sum_diff = sum_diff.wrapping_add(diff);
                    last_dts = dts;
                    if first_dts.is_none() {
                        first_dts = Some(result.len());
                    }

                    result.push((i, Some((pts_offset_in_packet, None, (diff & 0xFFFF) as u16))));
                } else {
                    result.push((i, None));
                }
            } else {
                result.push((i, None));
            }
        } else {
            result.push((i, None));
        }

        i += TS_PACKET_SIZE;
    }

    if let Some(first_dts_idx) = first_dts {
        let avg_diff = sum_diff / result.len() as u64;
        if let (idx, Some((pts, dts_opt, _))) = result[first_dts_idx] {
            result[first_dts_idx] = (idx, Some((pts, dts_opt, (avg_diff & 0xFFFF) as u16)));
        }
    }
    let mut vec = Vec::with_capacity(continuity_counters.len());
    vec.extend(continuity_counters.iter().map(|(&k, &v)| (k, v)));

    (result, vec)
}


/// Finds TS alignment by checking for 0x47 sync byte every 188 bytes
fn find_ts_alignment(buf: &[u8]) -> Option<usize> {
    for offset in 0..TS_PACKET_SIZE {
        let mut valid = true;
        for i in 0..5 {
            if buf.get(offset + i * TS_PACKET_SIZE) != Some(&SYNC_BYTE) {
                valid = false;
                break;
            }
        }
        if valid {
            return Some(offset);
        }
    }
    None
}

/// Calculates exact stream duration in 90kHz ticks.
/// Duration = (`last_pts` - `first_pts`) + `estimated_frame_duration`.
pub fn calculate_duration_ticks(buffer: &[u8], packet_indices: &PacketIndices) -> u64 {
    let mut first_pts: Option<u64> = None;
    let mut last_pts: Option<u64> = None;
    let mut count = 0;

    // We already calculated average diff/duration in `extract_pts_dts_indices_with_continuity`
    // but we didn't expose it. We can re-estimate it here or assume a default.
    // However, packet_indices stores `diff` in the tuple! `(pts, dts, diff)`.
    // But only for the first packet of a frame?
    // Let's just find first and last.

    // Also, we can estimate frame duration by taking the minimal non-zero diff between frames?
    // Or just (last - first) / (count - 1).

    // Note: packet_indices contains ALL packets. Many have None.
    // Those with Some have PTS/DTS.

    for &(packet_start, ref pts_dts_opt) in packet_indices {
        if let Some((pts_offset, _dts_offset, _diff)) = pts_dts_opt {
            let pts_bytes = &buffer[packet_start + pts_offset..packet_start + pts_offset + 5];
            let pts = decode_timestamp(pts_bytes);

            if first_pts.is_none() {
                first_pts = Some(pts);
            }
            last_pts = Some(pts);
            count += 1;
        }
    }

    match (first_pts, last_pts) {
        (Some(start), Some(end)) if end >= start && count > 1 => {
            let visible_duration = end - start;
            let avg_frame_duration = visible_duration / (count - 1);
            // Limit avg frame duration to something reasonable (e.g. < 1 sec = 90000) to avoid outliers
            let frame_duration = if avg_frame_duration > 0 && avg_frame_duration < 90000 {
                avg_frame_duration
            } else {
                3000 // Default to ~30fps (3000 ticks) if calculation fails
            };

            visible_duration + frame_duration
        }
        (Some(start), Some(end)) if end >= start => {
            // Single frame?
            end - start + 3000
        }
        _ => 0,
    }
}

type PacketIndices = Vec<(usize, Option<(usize, Option<usize>, u16)>)>;

pub struct TransportStreamBuffer {
    buffer: Arc<Vec<u8>>,
    packet_indices: Arc<PacketIndices>,
    current_pos: usize,
    current_dts: u64,
    timestamp_offset: u64,
    length: usize,
    stream_duration_90khz: u64, // Duration in 90kHz units
    /// Per-PID continuity counter and discontinuity-sent flag.
    /// Indexed directly by PID (0–8191) for O(1) lookup.
    cc_entries: Box<[Option<(u8, bool)>; 8192]>,
    waker: Arc<AtomicWaker>,
    first_pcr: Option<u64>,
    pids_with_timestamps: Arc<HashSet<u16>>,
}

impl std::fmt::Debug for TransportStreamBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportStreamBuffer")
            .field("length", &self.length)
            .field("current_pos", &self.current_pos)
            .field("current_dts", &self.current_dts)
            .field("timestamp_offset", &self.timestamp_offset)
            .field("stream_duration_90khz", &self.stream_duration_90khz)
            .field("first_pcr", &self.first_pcr)
            .finish_non_exhaustive()
    }
}

impl Clone for TransportStreamBuffer {
    fn clone(&self) -> Self {
        Self {
            buffer: Arc::clone(&self.buffer),
            packet_indices: Arc::clone(&self.packet_indices),
            current_pos: 0,
            current_dts: 0,
            timestamp_offset: 0,
            length: self.length,
            stream_duration_90khz: self.stream_duration_90khz,
            // Each clone starts with a fresh CC state; the discontinuity packets at the first
            // loop boundary will signal decoders to reset their CC expectations.
            cc_entries: Box::new([None; 8192]),
            waker: Arc::clone(&self.waker),
            first_pcr: self.first_pcr,
            pids_with_timestamps: Arc::clone(&self.pids_with_timestamps),
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

        let (packet_indices, _) = extract_pts_dts_indices_with_continuity(&raw);
        let length = packet_indices.len();

        let stream_duration_90khz = calculate_duration_ticks(&raw, &packet_indices);

        // Scan for the first PCR in the buffer to use as a reference for discontinuity packets
        let mut first_pcr = None;
        let mut pids_with_timestamps = HashSet::new();
        let mut i = 0;
        while i + TS_PACKET_SIZE <= raw.len() {
            if raw[i] != SYNC_BYTE {
                i += 1;
                continue;
            }
            let packet = &raw[i..i + TS_PACKET_SIZE];
            let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
            let afc = (packet[3] >> 4) & 0b11;
            if afc == 2 || afc == 3 {
                let adaptation_len = packet[4] as usize;
                // Need at least 7 adaptation bytes (1 flag + 6 PCR) to safely read the PCR field.
                if adaptation_len >= 7 && (packet[5] & ADAPTATION_FIELD_FLAG_PCR) != 0 {
                    first_pcr = Some(decode_pcr(&packet[6..12]));
                    pids_with_timestamps.insert(pid);
                    break;
                }
            }
            i += TS_PACKET_SIZE;
        }

        // Identify which PIDs actually have timestamps (PES).
        // we only want to inject Discontinuity packets on these PIDs to avoid corrupting PSI (PAT/PMT) which don't have timestamps.
        // pids_with_timestamps already seeded with PCR PID(s) above
        for (idx, info) in &packet_indices {
            if info.is_some() {
                // This packet has PTS/DTS. Find its PID.
                if *idx + 3 < raw.len() {
                    let pid = (u16::from(raw[*idx + 1] & 0x1F) << 8) | u16::from(raw[*idx + 2]);
                    pids_with_timestamps.insert(pid);
                }
            }
        }

        Self {
            buffer: Arc::new(raw),
            current_pos: 0,
            current_dts: 0,
            timestamp_offset: 0,
            length,
            packet_indices: Arc::new(packet_indices),
            stream_duration_90khz,
            cc_entries: Box::new([None; 8192]),
            waker: Arc::new(AtomicWaker::new()),
            first_pcr,
            pids_with_timestamps: Arc::new(pids_with_timestamps),
        }
    }

    /// Fallible constructor: returns an error if the raw bytes contain no valid MPEG-TS data.
    pub fn try_new(raw: Vec<u8>) -> Result<Self, crate::api::model::StreamError> {
        let buf = Self::new(raw);
        if buf.length == 0 {
            Err(crate::api::model::StreamError::MalformedPacket(
                "TS buffer does not contain decodable packet indices".to_string(),
            ))
        } else {
            Ok(buf)
        }
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn estimated_bitrate_kbps(&self) -> Option<usize> {
        if self.buffer.is_empty() || self.stream_duration_90khz == 0 {
            return None;
        }
        let duration_secs = self.stream_duration_90khz as f64 / 90_000.0;
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

    /// Returns the first DTS value found in the buffer (falls back to first PTS if no DTS).
    pub fn first_dts(&self) -> Option<u64> {
        for &(packet_start, ref pts_dts_opt) in self.packet_indices.iter() {
            if let Some((_pts, Some(dts_off), _diff)) = pts_dts_opt {
                if packet_start + dts_off + 5 <= self.buffer.len() {
                    return Some(decode_timestamp(&self.buffer[packet_start + dts_off..packet_start + dts_off + 5]));
                }
            }
        }
        for &(packet_start, ref pts_dts_opt) in self.packet_indices.iter() {
            if let Some((pts_off, None, _diff)) = pts_dts_opt {
                if packet_start + pts_off + 5 <= self.buffer.len() {
                    return Some(decode_timestamp(&self.buffer[packet_start + pts_off..packet_start + pts_off + 5]));
                }
            }
        }
        None
    }

    /// Sets the timestamp offset used when rewriting PTS/DTS/PCR values.
    pub fn set_timestamp_offset(&mut self, offset: u64) { self.timestamp_offset = offset; }

    /// Generates a Discontinuity packet for the given packet/PID state, writing it directly into `out`.
    fn generate_discontinuity_packet(new_packet: &[u8], cc: u8, first_pcr: Option<u64>, timestamp_offset: u64, out: &mut BytesMut) {
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
                let offset_27mhz = timestamp_offset.wrapping_mul(300) % MAX_PCR;
                let new_pcr = base_pcr.wrapping_add(offset_27mhz) % MAX_PCR;
                let pcr_bytes = encode_pcr(new_pcr);
                pkt[6..12].copy_from_slice(&pcr_bytes);
            } else {
                pkt[5] = 0x80;
            }
        } else {
            pkt[5] = 0x80; // Discontinuity Indicator Only
        }
    }

    /// Rewrites PCR, PTS, and DTS in-place for the TS packet that was just appended to `bytes`
    /// starting at `pkt_start`. All mutations happen directly in the `BytesMut` output buffer —
    /// no extra allocations.
    fn rewrite_timestamps_in_place(
        bytes: &mut BytesMut,
        pkt_start: usize,
        pts_dts_maybe: Option<(usize, Option<usize>, u16)>,
        timestamp_offset: u64,
    ) {
        // PCR rewrite (adaptation field must carry ≥7 bytes: 1 flag + 6 PCR).
        let afc = (bytes[pkt_start + 3] >> 4) & 0b11;
        if afc == 2 || afc == 3 {
            let adaptation_len = bytes[pkt_start + 4] as usize;
            if adaptation_len >= 7 && (bytes[pkt_start + 5] & ADAPTATION_FIELD_FLAG_PCR) != 0 {
                let pcr_pos = pkt_start + 6;
                let orig_pcr = decode_pcr(&bytes[pcr_pos..pcr_pos + 6]);
                // PCR runs at 27 MHz; convert 90 kHz offset by multiplying by 300.
                let offset_27mhz = timestamp_offset.wrapping_mul(300) % MAX_PCR;
                let new_pcr = orig_pcr.wrapping_add(offset_27mhz) % MAX_PCR;
                bytes[pcr_pos..pcr_pos + 6].copy_from_slice(&encode_pcr(new_pcr));
            }
        }

        // PTS rewrite (scoped so names don't leak into the DTS block below).
        if let Some((pts_off, dts_off_opt, _)) = pts_dts_maybe {
            {
                let pos = pkt_start + pts_off;
                let orig = decode_timestamp(&bytes[pos..pos + 5]);
                let adjusted = (orig + timestamp_offset) % MAX_PTS_DTS;
                let prefix = bytes[pos] & 0xF0;
                let mut encoded = encode_timestamp(adjusted);
                encoded[0] = (encoded[0] & 0x0F) | prefix;
                bytes[pos..pos + 5].copy_from_slice(&encoded);
            }
            // DTS rewrite (only when a separate DTS is present).
            if let Some(dts_off) = dts_off_opt {
                let pos = pkt_start + dts_off;
                let orig = decode_timestamp(&bytes[pos..pos + 5]);
                let adjusted = (orig + timestamp_offset) % MAX_PTS_DTS;
                let prefix = bytes[pos] & 0xF0;
                let mut encoded = encode_timestamp(adjusted);
                encoded[0] = (encoded[0] & 0x0F) | prefix;
                bytes[pos..pos + 5].copy_from_slice(&encoded);
            }
        }
    }

    /// Returns next chunks with adjusted PTS/DTS and PCR.
    /// All timestamp rewrites are performed in-place on the `BytesMut` output buffer to avoid
    /// per-packet heap allocations. PID continuity-counter lookup is O(1) via a fixed 8192-entry array.
    pub fn next_chunk(&mut self) -> Option<Bytes> {
        if self.length == 0 {
            return None;
        }
        let mut bytes = BytesMut::with_capacity(CHUNK_SIZE);
        let mut packets_remaining = PACKET_COUNT;

        while packets_remaining > 0 {
            if self.current_pos >= self.length {
                // Loop back — advance timestamp offset so PTS/DTS/PCR remain monotonically
                // increasing across loops. Resetting to 0 causes decoders (MPV, ffmpeg) to see
                // a backward timestamp jump and treat the loop as end-of-stream or corrupt data.
                self.current_pos = 0;
                self.timestamp_offset =
                    self.timestamp_offset.wrapping_add(self.stream_duration_90khz) % MAX_PTS_DTS;
                self.current_dts = 0;

                // Reset only the discontinuity-sent flag so injection packets are emitted at the
                // start of the next loop. Continuity counter values keep running so CC remains
                // globally monotonic across loops.
                for entry in self.cc_entries.iter_mut().flatten() {
                    entry.1 = false;
                }
            }

            let current_pos = self.current_pos;
            let (packet_start, pts_dts_maybe) = self.packet_indices[current_pos];
            let packet = &self.buffer[packet_start..packet_start + TS_PACKET_SIZE];
            let afc = (packet[3] >> 4) & 0b11;
            let packet_has_payload = afc == 0b01 || afc == 0b11;

            // O(1) PID lookup — PID is at most 13 bits (0–8191).
            let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
            let entry = &mut self.cc_entries[pid as usize];
            let is_new_pid = entry.is_none();
            if is_new_pid {
                // Initialize from source packet CC; we only advance for payload packets.
                *entry = Some((packet[3] & 0x0F, false));
            }
            let (counter, discontinuity_sent) = entry.as_mut().unwrap();

            let needs_discontinuity = self.pids_with_timestamps.contains(&pid);
            let inject_discontinuity = !*discontinuity_sent && needs_discontinuity;

            if !*discontinuity_sent && !needs_discontinuity {
                // PSI/other PIDs don't need a discontinuity packet; mark done immediately.
                *discontinuity_sent = true;
            }

            let payload_packet_cc;
            if inject_discontinuity {
                // Discontinuity packet is adaptation-only and does not consume a CC step.
                // Keep CC progression tied strictly to payload packets.
                let extra_packet_cc = if packet_has_payload { *counter } else { packet[3] & 0x0F };
                payload_packet_cc = if packet_has_payload { *counter } else { packet[3] & 0x0F };
                *discontinuity_sent = true;

                // Write discontinuity packet directly into the output buffer — no allocation.
                Self::generate_discontinuity_packet(packet, extra_packet_cc, self.first_pcr, self.timestamp_offset, &mut bytes);
            } else {
                payload_packet_cc = if packet_has_payload { *counter } else { packet[3] & 0x0F };
            }

            // TS continuity counter increments only when payload is present (AFC=01/11).
            if packet_has_payload {
                *counter = (*counter + 1) % 16;
            }

            // Append the original packet into `bytes`, then mutate the appended slice in-place.
            let pkt_start = bytes.len();
            bytes.extend_from_slice(packet);

            // Apply the computed CC to the payload packet.
            bytes[pkt_start + 3] = (bytes[pkt_start + 3] & 0xF0) | (payload_packet_cc & 0x0F);

            // Rewrite PCR / PTS / DTS in-place via the dedicated helper.
            Self::rewrite_timestamps_in_place(&mut bytes, pkt_start, pts_dts_maybe, self.timestamp_offset);

            self.current_pos += 1;
            packets_remaining -= 1;
        }

        Some(bytes.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pts_dts_payload_packet(pid: u16, cc: u8, pts: u64, dts: u64) -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xFF_u8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1F); // PUSI + PID high bits
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x10 | (cc & 0x0F); // payload only

        let payload = &mut packet[4..];
        payload[0] = 0x00;
        payload[1] = 0x00;
        payload[2] = 0x01;
        payload[3] = 0xE0;
        payload[4] = 0x00;
        payload[5] = 0x00;
        payload[6] = 0x80;
        payload[7] = 0xC0; // PTS + DTS present
        payload[8] = 0x0A;

        let mut pts_bytes = encode_timestamp(pts);
        pts_bytes[0] = (pts_bytes[0] & 0x0F) | 0x30;
        payload[9..14].copy_from_slice(&pts_bytes);

        let mut dts_bytes = encode_timestamp(dts);
        dts_bytes[0] = (dts_bytes[0] & 0x0F) | 0x10;
        payload[14..19].copy_from_slice(&dts_bytes);

        packet
    }

    fn build_adaptation_only_packet(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xFF_u8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = (pid >> 8) as u8 & 0x1F;
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x20 | (cc & 0x0F); // adaptation only
        packet[4] = 183;
        packet[5] = 0;
        packet
    }

    #[test]
    fn discontinuity_packet_does_not_advance_payload_cc() {
        let packet = build_pts_dts_payload_packet(0x0100, 7, 90_000, 87_000);
        let mut buf = TransportStreamBuffer::new(packet.to_vec());
        let chunk = buf.next_chunk().expect("expected chunk");
        assert!(chunk.len() >= TS_PACKET_SIZE * 2);

        // First emitted packet is injected discontinuity (adaptation-only),
        // second is the actual payload packet for the same PID.
        let disc_cc = chunk[3] & 0x0F;
        let disc_afc = (chunk[3] >> 4) & 0b11;
        let payload_cc = chunk[TS_PACKET_SIZE + 3] & 0x0F;
        assert_eq!(disc_afc, 0b10);
        assert_eq!(disc_cc, payload_cc);
    }

    #[test]
    fn adaptation_only_packets_keep_same_continuity_counter() {
        let packet = build_adaptation_only_packet(0x0011, 5);
        let mut buf = TransportStreamBuffer::new(packet.to_vec());
        let chunk = buf.next_chunk().expect("expected chunk");
        assert_eq!(chunk.len(), CHUNK_SIZE);

        for i in 0..PACKET_COUNT {
            let cc = chunk[i * TS_PACKET_SIZE + 3] & 0x0F;
            assert_eq!(cc, 5);
        }
    }
}
