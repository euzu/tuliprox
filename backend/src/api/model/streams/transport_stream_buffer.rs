use std::collections::HashMap;
use bytes::{Bytes, BytesMut};
use std::sync::Arc;
use futures::task::AtomicWaker;
use std::task::Waker;

const MAX_PCR: u64 = 1 << 42;        // 42 bit PCR cycle
const MAX_PTS_DTS: u64 = 1 << 33;    // 33 bit PTS/DTS cycle

const TS_PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PACKET_COUNT: usize = 250;
const CHUNK_SIZE: usize = TS_PACKET_SIZE * PACKET_COUNT;

const ADAPTATION_FIELD_FLAG_PCR: u8 = 0x10; // PCR flag bit in adaptation field flags

/// Decodes a 5-byte DTS/PTS field from PES header into u64 timestamp.
fn decode_timestamp(ts_bytes: &[u8]) -> u64 {
    (((u64::from(ts_bytes[0]) >> 1) & 0x07) << 30)
        | (u64::from(ts_bytes[1]) << 22)
        | (((u64::from(ts_bytes[2]) >> 1) & 0x7F) << 15)
        | (u64::from(ts_bytes[3]) << 7)
        | ((u64::from(ts_bytes[4])  >> 1) & 0x7F)
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
    let pcr_ext = ((u64::from(pcr_bytes[4]) & 1) << 8) |  u64::from(pcr_bytes[5]);
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

type TsInfoExtraction = (Vec<(usize, Option<(usize, usize, u16)>)>, Vec<(u16, u8)>);

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

                    result.push((i, Some((pts_offset_in_packet, dts_offset_in_packet, (diff & 0xFFFF) as u16))));
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
        if let (idx, Some((pts, dts, _))) = result[first_dts_idx] {
            result[first_dts_idx] = (idx, Some((pts, dts, (avg_diff & 0xFFFF) as u16)));
        }
    }
    let mut vec = Vec::with_capacity(continuity_counters.len());
    vec.extend(continuity_counters.iter().map(|(&k, &v)| (k, v)));

    (result, vec)
}

/// Replace PTS and DTS timestamps in the TS packet slice
fn replace_pts_dts(packet_slice: &[u8], pts_index: usize, dts_index: usize, new_presentation_ts: u64, new_decoding_ts: u64) -> Vec<u8> {
    let before_pts = &packet_slice[..pts_index];
    let between_pts_dts = &packet_slice[pts_index + 5..dts_index];
    let after_dts = &packet_slice[dts_index + 5..];

    let new_presentation_ts_bytes = encode_timestamp(new_presentation_ts);
    let new_decoding_ts_bytes = encode_timestamp(new_decoding_ts);

    let mut new_packet = Vec::with_capacity(packet_slice.len());
    new_packet.extend_from_slice(before_pts);
    new_packet.extend_from_slice(&new_presentation_ts_bytes);
    new_packet.extend_from_slice(between_pts_dts);
    new_packet.extend_from_slice(&new_decoding_ts_bytes);
    new_packet.extend_from_slice(after_dts);

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

/// Calculates stream duration in seconds from PTS values
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn duration_seconds(buffer: &[u8], packet_indices: &PacketIndices) -> Option<u64> {
    let mut first_pts: Option<u64> = None;
    let mut last_pts: Option<u64> = None;

    for &(packet_start, pts_dts_opt) in packet_indices {
        if let Some((pts_offset, _dts_offset, _diff)) = pts_dts_opt {
            let pts_bytes = &buffer[packet_start + pts_offset..packet_start + pts_offset + 5];
            let pts = decode_timestamp(pts_bytes);

            if first_pts.is_none() {
                first_pts = Some(pts);
            }
            last_pts = Some(pts);
        }
    }

    match (first_pts, last_pts) {
        (Some(start), Some(end)) if end >= start => {
            let duration_ticks = end - start;
            Some((duration_ticks as f64 / 90000.0).round() as u64)
        }
        _ => None,
    }
}

type PacketIndices = Vec<(usize, Option<(usize, usize, u16)>)>;

#[derive(Debug)]
pub struct TransportStreamBuffer {
    buffer: Arc<Vec<u8>>,
    packet_indices: Arc<PacketIndices>,
    current_pos: usize,
    current_dts: u64,
    timestamp_offset: u64,
    length: usize,
    stream_duration_90khz: u64, // Duration in 90kHz units
    initial_continuity_counters: Arc<Vec<(u16,u8)>>,
    continuity_counters: Vec<(u16,u8)>,
    waker: Arc<AtomicWaker>,
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
            continuity_counters: self.initial_continuity_counters.as_ref().clone(),
            waker: Arc::clone(&self.waker),
        }
    }
}

impl TransportStreamBuffer {
    pub fn new(mut raw: Vec<u8>) -> Self {
        let offset = find_ts_alignment(&raw).unwrap_or(0);
        raw.drain(..offset);

        for i in 0..raw.len() / TS_PACKET_SIZE {
            if raw[i * TS_PACKET_SIZE] != SYNC_BYTE {
                raw.truncate(i * TS_PACKET_SIZE);
                break;
            }
        }

        let (packet_indices, continuity_counters) = extract_pts_dts_indices_with_continuity(&raw);
        let length = packet_indices.len();

        let stream_duration_seconds = duration_seconds(&raw, &packet_indices).unwrap_or(0);
        let stream_duration_90khz = stream_duration_seconds * 90_000;

        Self {
            buffer: Arc::new(raw),
            current_pos: 9,
            current_dts: 0,
            timestamp_offset: 0,
            length,
            packet_indices: Arc::new(packet_indices),
            stream_duration_90khz,
            continuity_counters: continuity_counters.clone(),
            initial_continuity_counters:  Arc::new(continuity_counters),
            waker: Arc::new(AtomicWaker::new()),
        }
    }

    pub fn register_waker(&self, waker: &Waker) {
        self.waker.register(waker);
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
                // Loop back and update timestamp offset
                self.current_pos = 0;

                let new_offset = (self.timestamp_offset + self.stream_duration_90khz) % MAX_PTS_DTS;
                self.timestamp_offset = new_offset;

                self.current_dts = 0;
            }

            let current_pos = self.current_pos;
            let (packet_start, pts_dts_maybe) = self.packet_indices[current_pos];
            let packet = &self.buffer[packet_start..packet_start + TS_PACKET_SIZE];

            let mut new_packet = packet.to_vec();

            // update continuity counter
            let pid = (u16::from(new_packet[1] & 0x1F) << 8) | u16::from(new_packet[2]);
            // let counter = &mut self.continuity_counters[pid as usize];
            // new_packet[3] = (new_packet[3] & 0xF0) | (*counter & 0x0F);
            // *counter = (*counter + 1) % 16;

            // Find the entry with this PID (mutable), or insert a new entry if it doesn't exist
            if let Some((_pid, counter)) = self.continuity_counters.iter_mut().find(|(p, _)| *p == pid) {
                new_packet[3] = (new_packet[3] & 0xF0) | (*counter & 0x0F);
                *counter = (*counter + 1) % 16;
            } else {
                // This should not happen because we scan the entire pids.
                // PID not present yet, insert new entry with counter = 0
                self.continuity_counters.push((pid, 1)); // Start at 1, since we immediately use the counter
                new_packet[3] &= 0xF0;
            }

            // adjust PCR based on the original PCR, then add the offset
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
            if let Some((pts_offset, dts_offset, _diff)) = pts_dts_maybe {
                let orig_dts = decode_timestamp(&new_packet[dts_offset..dts_offset + 5]);
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
