#![allow(clippy::cast_possible_truncation, reason = "test builders write PIDs and lengths into single bytes")]

use crate::packet::{mpeg2_crc32, TS_PACKET_SIZE};
use bytes::Bytes;

pub(crate) const PMT_PID: u16 = 0x1000;
pub(crate) const VIDEO_PID: u16 = 0x100;
pub(crate) const AUDIO_PID: u16 = 0x101;
pub(crate) const H264: u8 = 0x1B;
pub(crate) const HEVC: u8 = 0x24;
pub(crate) const MPEG2: u8 = 0x02;
pub(crate) const AAC: u8 = 0x0F;

/// One TS packet. With `af_flags` the adaptation field fills the room the payload leaves;
/// without, the payload is padded with 0xFF to 184 bytes.
pub(crate) fn ts(pid: u16, pusi: bool, af_flags: Option<u8>, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0x47, (if pusi { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1F), pid as u8];
    if let Some(flags) = af_flags {
        let af_len = 184 - 1 - payload.len();
        assert!(af_len >= 1);
        packet.push(0x30);
        packet.push(af_len as u8);
        packet.push(flags);
        packet.extend(std::iter::repeat_n(0xFF, af_len - 1));
        packet.extend_from_slice(payload);
    } else {
        packet.push(0x10);
        packet.extend_from_slice(payload);
        packet.resize(TS_PACKET_SIZE, 0xFF);
    }
    assert_eq!(packet.len(), TS_PACKET_SIZE);
    packet
}

/// One TS packet carrying only an adaptation field with a PCR.
pub(crate) fn pcr_packet(pid: u16, pcr_27mhz: u64) -> Vec<u8> {
    let base = pcr_27mhz / 300;
    let ext = pcr_27mhz % 300;
    let mut packet = vec![0x47, (pid >> 8) as u8 & 0x1F, pid as u8, 0x20, 183, 0x10];
    packet.extend([
        (base >> 25) as u8,
        (base >> 17) as u8,
        (base >> 9) as u8,
        (base >> 1) as u8,
        (((base & 1) << 7) as u8) | 0x7E | ((ext >> 8) as u8),
        ext as u8,
    ]);
    packet.resize(TS_PACKET_SIZE, 0xFF);
    packet
}

pub(crate) fn finish_section(mut section: Vec<u8>) -> Vec<u8> {
    let length = section.len() - 3 + 4;
    section[1] = 0xB0 | ((length >> 8) as u8 & 0x0F);
    section[2] = length as u8;
    let crc = mpeg2_crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

pub(crate) fn pat_section(version: u8, program: u16, pmt_pid: u16) -> Vec<u8> {
    let section = vec![
        0x00,
        0,
        0,
        0x00,
        0x01,
        0xC1 | (version << 1),
        0x00,
        0x00,
        (program >> 8) as u8,
        program as u8,
        0xE0 | (pmt_pid >> 8) as u8,
        pmt_pid as u8,
    ];
    finish_section(section)
}

pub(crate) fn pmt_section(version: u8, program: u16, streams: &[(u8, u16)]) -> Vec<u8> {
    let pcr_pid = streams.first().map_or(0x1FFF, |(_, pid)| *pid);
    let mut section = vec![
        0x02,
        0,
        0,
        (program >> 8) as u8,
        program as u8,
        0xC1 | (version << 1),
        0x00,
        0x00,
        0xE0 | (pcr_pid >> 8) as u8,
        pcr_pid as u8,
        0xF0,
        0x00,
    ];
    for (stream_type, pid) in streams {
        section.extend([*stream_type, 0xE0 | (pid >> 8) as u8, *pid as u8, 0xF0, 0x00]);
    }
    finish_section(section)
}

pub(crate) fn psi(pid: u16, section: &[u8]) -> Vec<u8> {
    let mut payload = vec![0x00];
    payload.extend_from_slice(section);
    ts(pid, true, None, &payload)
}

pub(crate) fn pat(version: u8) -> Vec<u8> { psi(0, &pat_section(version, 1, PMT_PID)) }

pub(crate) fn pmt(version: u8, streams: &[(u8, u16)]) -> Vec<u8> { psi(PMT_PID, &pmt_section(version, 1, streams)) }

pub(crate) fn video_pmt(version: u8) -> Vec<u8> { pmt(version, &[(H264, VIDEO_PID), (AAC, AUDIO_PID)]) }

/// PES start on `pid` whose elementary data holds NAL units of the given header bytes.
pub(crate) fn pes(pid: u16, rai: bool, af: bool, nal_headers: &[&[u8]]) -> Vec<u8> {
    let mut payload = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05, 0x21, 0x00, 0x01, 0x00, 0x01];
    for header in nal_headers {
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        payload.extend_from_slice(header);
        payload.extend_from_slice(&[0x88, 0x84]);
    }
    let flags = if rai {
        Some(0x40)
    } else if af {
        Some(0x00)
    } else {
        None
    };
    ts(pid, true, flags, &payload)
}

/// PES start on `pid` with an adaptation field containing PCR and optionally RAI flag.
pub(crate) fn pes_with_pcr(pid: u16, rai: bool, pcr_27mhz: u64, nal_headers: &[&[u8]]) -> Vec<u8> {
    let mut payload = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05, 0x21, 0x00, 0x01, 0x00, 0x01];
    for header in nal_headers {
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        payload.extend_from_slice(header);
        payload.extend_from_slice(&[0x88, 0x84]);
    }
    let mut packet = vec![0x47, 0x40 | ((pid >> 8) as u8 & 0x1F), pid as u8, 0x30];
    let af_len = 184 - 1 - payload.len();
    assert!(af_len >= 7, "room for PCR in adaptation field");
    packet.push(af_len as u8);
    let flags = if rai { 0x50 } else { 0x10 };
    packet.push(flags);
    let base = pcr_27mhz / 300;
    let ext = pcr_27mhz % 300;
    packet.extend([
        (base >> 25) as u8,
        (base >> 17) as u8,
        (base >> 9) as u8,
        (base >> 1) as u8,
        (((base & 1) << 7) as u8) | 0x7E | ((ext >> 8) as u8),
        ext as u8,
    ]);
    packet.extend(std::iter::repeat_n(0xFF, af_len - 7));
    packet.extend_from_slice(&payload);
    assert_eq!(packet.len(), TS_PACKET_SIZE);
    packet
}

pub(crate) fn h264(nal_type: u8) -> Vec<u8> { vec![0x60 | nal_type] }

pub(crate) fn hevc(nal_type: u8) -> Vec<u8> { vec![nal_type << 1, 0x01] }

pub(crate) fn mpeg2(header: u8) -> Vec<u8> { vec![header] }

pub(crate) fn video_cont(af: bool) -> Vec<u8> { ts(VIDEO_PID, false, af.then_some(0x00), &[0x11; 100]) }

pub(crate) fn join(packets: &[Vec<u8>]) -> Bytes { Bytes::from(packets.concat()) }

pub(crate) fn packets(bytes: &[u8]) -> Vec<&[u8]> {
    assert_eq!(bytes.len() % TS_PACKET_SIZE, 0, "output is whole packets");
    bytes.chunks(TS_PACKET_SIZE).collect()
}

pub(crate) fn pid_of(packet: &[u8]) -> u16 { (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]) }
