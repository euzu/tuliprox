use super::{
    clock::{
        append_finite_discontinuity_packet, decode_pcr, decode_pcr_at_location, decode_timestamp,
        decode_timestamp_at_location, encode_pcr, encode_timestamp, forward_clock_distance_90khz,
        gather_timestamp_bytes, pcr_offset_27mhz, ts_packet_pid, ADAPTATION_FIELD_FLAG_PCR,
        HLS_TS_SPLICE_MIN_GAP_TICKS_90KHZ, MAX_PTS_DTS, NULL_PID, SYNC_BYTE, TS_PACKET_SIZE,
    },
    layout::build_hls_finite_ts_layout,
    pes::inspect_hls_ts_packet,
    *,
};
use bytes::BytesMut;

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

fn build_pts_only_payload_packet(pid: u16, cc: u8, pts: u64) -> [u8; TS_PACKET_SIZE] {
    let mut packet = [0xFF_u8; TS_PACKET_SIZE];
    packet[0] = SYNC_BYTE;
    packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    packet[2] = (pid & 0xFF) as u8;
    packet[3] = 0x10 | (cc & 0x0F);
    let payload = &mut packet[4..];
    payload[..9].copy_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05]);
    payload[9..14].copy_from_slice(&encode_timestamp(pts));
    packet
}

fn pts_only_pes_header(pts: u64) -> [u8; 14] {
    let mut header = [0u8; 14];
    header[..9].copy_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05]);
    header[9..14].copy_from_slice(&encode_timestamp(pts));
    header
}

fn pts_dts_pes_header(pts: u64, dts: u64) -> [u8; 19] {
    let mut header = [0u8; 19];
    header[..9].copy_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0xC0, 0x0A]);
    let mut pts_bytes = encode_timestamp(pts);
    pts_bytes[0] = (pts_bytes[0] & 0x0F) | 0x30;
    header[9..14].copy_from_slice(&pts_bytes);
    let mut dts_bytes = encode_timestamp(dts);
    dts_bytes[0] = (dts_bytes[0] & 0x0F) | 0x10;
    header[14..19].copy_from_slice(&dts_bytes);
    header
}

fn build_split_pes_header_packets(
    pid: u16,
    first_cc: u8,
    header: &[u8],
    payload_lengths: &[usize],
) -> Vec<[u8; TS_PACKET_SIZE]> {
    let mut header_offset = 0usize;
    payload_lengths
        .iter()
        .copied()
        .enumerate()
        .map(|(packet_index, payload_length)| {
            assert!((1..=184).contains(&payload_length));
            let mut packet = [0xFF_u8; TS_PACKET_SIZE];
            let packet_index = u8::try_from(packet_index).expect("test packet index fits");
            packet[0] = SYNC_BYTE;
            packet[1] = ((pid >> 8) as u8 & 0x1F) | (u8::from(packet_index == 0) * 0x40);
            packet[2] = (pid & 0xFF) as u8;
            packet[3] = if payload_length == 184 {
                0x10 | (first_cc.wrapping_add(packet_index) & 0x0F)
            } else {
                0x30 | (first_cc.wrapping_add(packet_index) & 0x0F)
            };
            let payload_offset = if payload_length == 184 {
                4
            } else {
                let adaptation_length = 183usize.saturating_sub(payload_length);
                packet[4] = u8::try_from(adaptation_length).expect("test adaptation length fits");
                packet[5] = 0;
                5 + adaptation_length
            };
            let remaining = header.len().saturating_sub(header_offset);
            let copied = remaining.min(payload_length);
            packet[payload_offset..payload_offset + copied]
                .copy_from_slice(&header[header_offset..header_offset + copied]);
            header_offset = header_offset.saturating_add(copied);
            packet
        })
        .collect()
}

fn build_pts_dts_pcr_packet(pid: u16, cc: u8, pts: u64, dts: u64, pcr_90khz: u64) -> [u8; TS_PACKET_SIZE] {
    let mut packet = [0xFF_u8; TS_PACKET_SIZE];
    packet[0] = SYNC_BYTE;
    packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    packet[2] = (pid & 0xFF) as u8;
    packet[3] = 0x30 | (cc & 0x0F);
    packet[4] = 7;
    packet[5] = ADAPTATION_FIELD_FLAG_PCR;
    packet[6..12].copy_from_slice(&encode_pcr(pcr_90khz.saturating_mul(300)));

    let payload = &mut packet[12..];
    payload[0] = 0x00;
    payload[1] = 0x00;
    payload[2] = 0x01;
    payload[3] = 0xE0;
    payload[4] = 0x00;
    payload[5] = 0x00;
    payload[6] = 0x80;
    payload[7] = 0xC0;
    payload[8] = 0x0A;

    let mut pts_bytes = encode_timestamp(pts);
    pts_bytes[0] = (pts_bytes[0] & 0x0F) | 0x30;
    payload[9..14].copy_from_slice(&pts_bytes);
    let mut dts_bytes = encode_timestamp(dts);
    dts_bytes[0] = (dts_bytes[0] & 0x0F) | 0x10;
    payload[14..19].copy_from_slice(&dts_bytes);
    packet
}

fn packet_timestamps(packet: &[u8]) -> (u64, u64, u64) {
    let layout = build_hls_finite_ts_layout(packet).expect("test packet layout");
    let pts = layout
        .timestamp_fields
        .iter()
        .copied()
        .find(|field| field.kind == HlsTsTimestampFieldKind::Pts)
        .and_then(|field| decode_timestamp_at_location(packet, field).ok())
        .expect("test packet PTS");
    let dts = layout
        .timestamp_fields
        .iter()
        .copied()
        .find(|field| field.kind == HlsTsTimestampFieldKind::Dts)
        .and_then(|field| decode_timestamp_at_location(packet, field).ok())
        .expect("test packet DTS");
    let pcr = layout
        .pcr_fields
        .first()
        .copied()
        .and_then(|field| decode_pcr_at_location(packet, field).ok())
        .expect("test packet PCR")
        / 300;
    (pts, dts, pcr)
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

fn build_pcr_only_packet(pid: u16, cc: u8, pcr_90khz: u64) -> [u8; TS_PACKET_SIZE] {
    let mut packet = build_adaptation_only_packet(pid, cc);
    packet[5] = ADAPTATION_FIELD_FLAG_PCR;
    packet[6..12].copy_from_slice(&encode_pcr(pcr_90khz.saturating_mul(300)));
    packet
}

fn build_payload_packet(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
    let mut packet = [0xFF_u8; TS_PACKET_SIZE];
    packet[0] = SYNC_BYTE;
    packet[1] = (pid >> 8) as u8 & 0x1F;
    packet[2] = (pid & 0xFF) as u8;
    packet[3] = 0x10 | (cc & 0x0F);
    packet
}

fn assert_ffmpeg_compatible_continuity(bytes: &[u8]) {
    assert_eq!(bytes.len() % TS_PACKET_SIZE, 0, "unaligned MPEG-TS fixture");
    let mut last_continuity: [Option<u8>; 8192] = [None; 8192];
    for (packet_index, packet) in bytes.as_chunks::<TS_PACKET_SIZE>().0.iter().enumerate() {
        let evidence = inspect_hls_ts_packet(packet, packet_index.saturating_mul(TS_PACKET_SIZE))
            .expect("valid MPEG-TS fixture packet");
        if evidence.pid == NULL_PID {
            continue;
        }
        if let Some(previous) = last_continuity[usize::from(evidence.pid)] {
            let expected = if evidence.has_payload() { previous.wrapping_add(1) & 0x0F } else { previous };
            assert!(
                evidence.discontinuity || evidence.continuity_counter == expected,
                "PID {} continuity failed at packet {packet_index}: expected {expected}, actual {}",
                evidence.pid,
                evidence.continuity_counter
            );
        }
        last_continuity[usize::from(evidence.pid)] = Some(evidence.continuity_counter);
    }
}

mod policy;
mod terminal;
mod transport;
