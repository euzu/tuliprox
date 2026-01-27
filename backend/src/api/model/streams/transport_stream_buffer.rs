use bytes::{Bytes, BytesMut};
use futures::task::AtomicWaker;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::task::Waker;

const MAX_PCR: u64 = 1 << 42;        // 42 bit PCR cycle
const MAX_PTS_DTS: u64 = 1 << 33;    // 33 bit PTS/DTS cycle

const TS_PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PACKET_COUNT: usize = 7; // Reduced from 250 to 7 (1316 bytes) to prevent latency/timeout on low-bitrate streams
const CHUNK_SIZE: usize = TS_PACKET_SIZE * PACKET_COUNT;

const ADAPTATION_FIELD_FLAG_PCR: u8 = 0x10; // PCR flag bit in adaptation field flags

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

        if payload.len() >= 14 && payload.starts_with(&[0x00, 0x00, 0x01]) {
            let flags = payload[7];
            let pts_dts_flags = (flags >> 6) & 0b11;

            if pts_dts_flags == 0b11 {
                // PTS at 9, DTS at 14
                let pts_start = 9;
                let dts_start = 14;

                if payload.len() >= dts_start + 5 {
                    let pts_offset_in_packet = payload_offset + pts_start;
                    let dts_offset_in_packet = payload_offset + dts_start;

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
                // PTS at 9, no DTS
                let pts_start = 9;
                if payload.len() >= pts_start + 5 {
                    let pts_offset_in_packet = payload_offset + pts_start;
                    // For PTS-only, DTS = PTS
                    let pts_bytes = &packet[pts_offset_in_packet..pts_offset_in_packet + 5];
                    let pts = decode_timestamp(pts_bytes);

                    // Approximate DTS diff using PTS?
                    // Or just ignore diff logic for PTS-only packets?
                    // We need 'diff' for smoothing via 'sum_diff'?
                    // If we mix Video (PTS+DTS) and Audio (PTS only).
                    // 'diff' is used for first_dts_idx fallback?
                    // Let's preserve 'last_dts' logic using PTS as DTS.
                    let dts = pts;
                    let diff = if last_dts > 0 { dts.wrapping_sub(last_dts) } else { 0 };
                    // Only accumulated if we consider this a valid frame for timing.
                    // Audio frames are valid.
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

/// Replace PTS and DTS timestamps in the TS packet slice
fn replace_pts_dts(packet_slice: &[u8], pts_index: usize, dts_index: Option<usize>, new_presentation_ts: u64, new_decoding_ts: u64) -> Vec<u8> {
    let new_presentation_ts_bytes = encode_timestamp(new_presentation_ts);

    let mut new_packet = Vec::with_capacity(packet_slice.len());

    if let Some(dts_idx) = dts_index {
        // PTS and DTS Case
        let before_pts = &packet_slice[..pts_index];
        let between_pts_dts = &packet_slice[pts_index + 5..dts_idx];
        let after_dts = &packet_slice[dts_idx + 5..];

        // Correctly handle PTS prefix (should be 0x30 / 0011xxxx for PTS in PTS+DTS)
        let mut pts_bytes = new_presentation_ts_bytes;
        let pts_prefix_bits = packet_slice[pts_index] & 0xF0;
        pts_bytes[0] = (pts_bytes[0] & 0x0F) | pts_prefix_bits;

        // Correctly handle DTS prefix (should be 0x10 / 0001xxxx)
        let mut dts_bytes = encode_timestamp(new_decoding_ts);
        let dts_prefix_bits = packet_slice[dts_idx] & 0xF0;
        dts_bytes[0] = (dts_bytes[0] & 0x0F) | dts_prefix_bits;

        new_packet.extend_from_slice(before_pts);
        new_packet.extend_from_slice(&pts_bytes);
        new_packet.extend_from_slice(between_pts_dts);
        new_packet.extend_from_slice(&dts_bytes);
        new_packet.extend_from_slice(after_dts);
    } else {
        // PTS Only Case
        let before_pts = &packet_slice[..pts_index];
        let after_pts = &packet_slice[pts_index + 5..];

        // Correctly handle PTS prefix (should be 0x20 / 0010xxxx for PTS only)
        // We use the original prefix to be safe (it might have flags embedded in other bits if not standard?)
        // Standard says '0010FB...'
        let pts_prefix_bits = packet_slice[pts_index] & 0xF0;
        let mut pts_bytes = new_presentation_ts_bytes;
        pts_bytes[0] = (pts_bytes[0] & 0x0F) | pts_prefix_bits;

        new_packet.extend_from_slice(before_pts);
        new_packet.extend_from_slice(&pts_bytes);
        new_packet.extend_from_slice(after_pts);
    }

    new_packet
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

#[derive(Debug)]
pub struct TransportStreamBuffer {
    buffer: Arc<Vec<u8>>,
    packet_indices: Arc<PacketIndices>,
    current_pos: usize,
    current_dts: u64,
    timestamp_offset: u64,
    length: usize,
    stream_duration_90khz: u64, // Duration in 90kHz units
    initial_continuity_counters: Arc<Vec<(u16, u8)>>,
    continuity_counters: Vec<(u16, u8, bool)>,
    waker: Arc<AtomicWaker>,
    first_pcr: Option<u64>,
    pids_with_timestamps: Arc<HashSet<u16>>,
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
            initial_continuity_counters: Arc::clone(&self.initial_continuity_counters),
            continuity_counters: self.initial_continuity_counters.iter().map(|(p, c)| (*p, *c, false)).collect(),
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

        let (packet_indices, continuity_counters) = extract_pts_dts_indices_with_continuity(&raw);
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
                if adaptation_len > 0 {
                    let flags = packet[5];
                    if (flags & ADAPTATION_FIELD_FLAG_PCR) != 0 && packet.len() >= 6 + 6 {
                        first_pcr = Some(decode_pcr(&packet[6..12]));
                        pids_with_timestamps.insert(pid);
                        break;
                    }
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
            continuity_counters: continuity_counters.iter().map(|(p, c)| (*p, *c, false)).collect(),
            initial_continuity_counters: Arc::new(continuity_counters),
            waker: Arc::new(AtomicWaker::new()),
            first_pcr,
            pids_with_timestamps: Arc::new(pids_with_timestamps),
        }
    }

    pub fn register_waker(&self, waker: &Waker) {
        self.waker.register(waker);
    }

    /// Generates a Discontinuity packet for the given packet/PID state.
    fn generate_discontinuity_packet(_pid: u16, new_packet: &[u8], cc: u8, first_pcr: Option<u64>, timestamp_offset: u64) -> Vec<u8> {
        let mut pkt = vec![0xFF; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        pkt[1] = new_packet[1] & 0x1F;
        pkt[2] = new_packet[2];

        // Check if the current packet has a PCR.
        let new_pkt_has_pcr = {
            let afc = (new_packet[3] >> 4) & 0b11;
            if (afc == 2 || afc == 3) && new_packet.len() >= 6 {
                let adaptation_len = new_packet[4] as usize;
                // Flags are at offset 5
                adaptation_len > 0 && (new_packet[5] & ADAPTATION_FIELD_FLAG_PCR) != 0
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

                let offset = timestamp_offset * 300;
                let new_pcr = (base_pcr + offset) % MAX_PCR;
                let pcr_bytes = encode_pcr(new_pcr);
                pkt[6..12].copy_from_slice(&pcr_bytes);
            } else {
                pkt[5] = 0x80;
            }
        } else {
            pkt[5] = 0x80; // Discontinuity Indicator Only
        }

        pkt
    }

    /// Returns next chunks with adjusted PTS/DTS and PCR
    pub fn next_chunk(&mut self) -> Option<Bytes> {
        if self.length == 0 {
            return None;
        }
        let mut bytes = BytesMut::with_capacity(CHUNK_SIZE);
        // we send this amount of packets in one chunk
        let mut packets_remaining = PACKET_COUNT;

        while packets_remaining > 0 {
            if self.current_pos >= self.length {
                // Loop back
                self.current_pos = 0;

                // Reset timestamps to start
                self.timestamp_offset = 0;
                self.current_dts = 0;

                // Reset discontinuity flags to trigger injection of discontinuity packets for each PID
                for (_, _, sent) in &mut self.continuity_counters {
                    *sent = false;
                }
            }

            let current_pos = self.current_pos;
            let (packet_start, pts_dts_maybe) = self.packet_indices[current_pos];
            let packet = &self.buffer[packet_start..packet_start + TS_PACKET_SIZE];

            let mut new_packet = packet.to_vec();

            // update continuity counter
            let pid = (u16::from(new_packet[1] & 0x1F) << 8) | u16::from(new_packet[2]);

            // Find the entry with this PID (mutable), or insert a new entry if it doesn't exist
            let mut entry_idx = None;
            for (idx, (p, _, _)) in self.continuity_counters.iter().enumerate() {
                if *p == pid {
                    entry_idx = Some(idx);
                    break;
                }
            }

            if entry_idx.is_none() {
                self.continuity_counters.push((pid, 1, false));
                entry_idx = Some(self.continuity_counters.len() - 1);
                new_packet[3] &= 0xF0;
            }

            let idx = entry_idx.unwrap();
            let (_, counter, discontinuity_sent) = &mut self.continuity_counters[idx];

            let payload_packet_cc;
            let needs_discontinuity = self.pids_with_timestamps.contains(&pid);
            let inject_discontinuity = !*discontinuity_sent && needs_discontinuity;

            if !*discontinuity_sent && !needs_discontinuity {
                // For PIDs that don't need discontinuity (PSI), just mark as sent so we don't check again this loop
                *discontinuity_sent = true;
            }

            if inject_discontinuity {
                // Extra packet gets current counter (N)
                let extra_packet_cc = *counter;
                *counter = (*counter + 1) % 16;

                // Payload packet gets next counter (N+1)
                payload_packet_cc = *counter;
                *counter = (*counter + 1) % 16;

                *discontinuity_sent = true;

                let extra = Self::generate_discontinuity_packet(pid, &new_packet, extra_packet_cc, self.first_pcr, self.timestamp_offset);
                bytes.extend_from_slice(&extra);
            } else {
                // Payload packet gets current counter (N)
                payload_packet_cc = *counter;
                *counter = (*counter + 1) % 16;
            }

            // Apply CC to Payload Packet
            new_packet[3] = (new_packet[3] & 0xF0) | (payload_packet_cc & 0x0F);

            let afc = (new_packet[3] >> 4) & 0b11;
            if afc == 2 || afc == 3 {
                let adaptation_len = new_packet[4] as usize;
                if adaptation_len > 0 {
                    let flags = new_packet[5];
                    if (flags & ADAPTATION_FIELD_FLAG_PCR) != 0 {
                        let pcr_pos = 6;
                        if new_packet.len() >= pcr_pos + 6 {
                            // read original PCR
                            let orig_pcr = decode_pcr(&new_packet[pcr_pos..pcr_pos + 6]);
                            // Apply PCR offset; PCR runs at 27 MHz, so multiply by 300 to convert from 90 kHz to 27 MHz
                            let offset = self.timestamp_offset * 300;
                            let new_pcr = (orig_pcr + offset) % MAX_PCR;
                            let pcr_bytes = encode_pcr(new_pcr);
                            new_packet[pcr_pos..pcr_pos + 6].copy_from_slice(&pcr_bytes);
                        }
                    }
                }
            }

            // adjust PTS/DTS
            if let Some((pts_offset, dts_offset_opt, _diff)) = pts_dts_maybe {
                let (orig_dts, dts_offset) = if let Some(dts_offset) = dts_offset_opt {
                    (decode_timestamp(&new_packet[dts_offset..dts_offset + 5]), Some(dts_offset))
                } else {
                    // If no DTS, use PTS as DTS
                    (decode_timestamp(&new_packet[pts_offset..pts_offset + 5]), None)
                };

                let new_decoding_ts = (orig_dts + self.timestamp_offset) % MAX_PTS_DTS;

                let orig_presentation_ts = decode_timestamp(&new_packet[pts_offset..pts_offset + 5]);
                let new_presentation_ts = (orig_presentation_ts + self.timestamp_offset) % MAX_PTS_DTS;

                let replaced = replace_pts_dts(&new_packet, pts_offset, dts_offset, new_presentation_ts, new_decoding_ts);
                new_packet = replaced;
            }

            bytes.extend_from_slice(&new_packet);

            self.current_pos += 1;
            packets_remaining -= 1;
        }

        Some(bytes.freeze())
    }
}
