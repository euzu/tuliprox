use super::*;
use aes::cipher::{Block, BlockEncrypt, KeyInit};
use mpeg2ts_reader::mpegts_crc;
use std::{
    cell::Cell,
    io::{Cursor, Read},
    pin::Pin,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

fn append_crc(mut section: Vec<u8>) -> Vec<u8> {
    let crc = mpegts_crc::sum32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

fn psi_version_byte(version: u8, current: bool) -> u8 { 0xC0 | ((version & 0x1F) << 1) | u8::from(current) }

fn pat_section_with_header(
    programs: &[(u16, u16)],
    version: u8,
    current: bool,
    section_number: u8,
    last_section_number: u8,
) -> Vec<u8> {
    let section_length = 5usize.saturating_add(programs.len().saturating_mul(4)).saturating_add(4);
    let mut section = vec![
        0x00,
        0xB0 | u8::try_from(section_length >> 8).unwrap_or(0),
        u8::try_from(section_length & 0xFF).unwrap_or(0),
    ];
    section.extend_from_slice(&[0x00, 0x01, psi_version_byte(version, current), section_number, last_section_number]);
    for (program_number, pid) in programs {
        section.extend_from_slice(&program_number.to_be_bytes());
        section.extend_from_slice(&(0xE000 | (pid & 0x1FFF)).to_be_bytes());
    }
    append_crc(section)
}

fn pat_section(programs: &[(u16, u16)]) -> Vec<u8> { pat_section_with_header(programs, 0, true, 0, 0) }

#[derive(Clone, Copy)]
struct PmtSectionHeader {
    version: u8,
    current: bool,
    section_number: u8,
    last_section_number: u8,
}

fn pmt_section_with_header(
    program_number: u16,
    pcr_pid: u16,
    streams: &[(u8, u16)],
    descriptor_bytes: usize,
    header: PmtSectionHeader,
) -> Vec<u8> {
    let program_info_length = descriptor_bytes.min(0x0FFF) & !1;
    let section_length =
        9usize.saturating_add(program_info_length).saturating_add(streams.len().saturating_mul(5)).saturating_add(4);
    let mut section = vec![
        0x02,
        0xB0 | u8::try_from(section_length >> 8).unwrap_or(0),
        u8::try_from(section_length & 0xFF).unwrap_or(0),
    ];
    section.extend_from_slice(&program_number.to_be_bytes());
    section.extend_from_slice(&[
        psi_version_byte(header.version, header.current),
        header.section_number,
        header.last_section_number,
    ]);
    section.extend_from_slice(&(0xE000 | (pcr_pid & 0x1FFF)).to_be_bytes());
    section.extend_from_slice(&(0xF000 | u16::try_from(program_info_length).unwrap_or(0)).to_be_bytes());
    for _ in 0..program_info_length / 2 {
        section.extend_from_slice(&[0x80, 0x00]);
    }
    for (stream_type, pid) in streams {
        section.push(*stream_type);
        section.extend_from_slice(&(0xE000 | (pid & 0x1FFF)).to_be_bytes());
        section.extend_from_slice(&[0xF0, 0x00]);
    }
    append_crc(section)
}

fn pmt_section(program_number: u16, pcr_pid: u16, streams: &[(u8, u16)], descriptor_bytes: usize) -> Vec<u8> {
    pmt_section_with_header(
        program_number,
        pcr_pid,
        streams,
        descriptor_bytes,
        PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 0 },
    )
}

fn packetize_section(pid: u16, section: &[u8], first_counter: u8) -> Vec<u8> {
    let mut output = Vec::new();
    let mut offset = 0usize;
    let mut counter = first_counter & 0x0F;
    let mut first = true;
    while offset < section.len() {
        let mut packet = [0xFF_u8; TS_PACKET_BYTES];
        packet[0] = Packet::SYNC_BYTE;
        packet[1] = u8::try_from(pid >> 8).unwrap_or(0) & 0x1F;
        packet[2] = u8::try_from(pid & 0xFF).unwrap_or(0);
        packet[3] = 0x10 | counter;
        let payload_start = if first {
            packet[1] |= 0x40;
            packet[4] = 0;
            5
        } else {
            4
        };
        let copied = section.len().saturating_sub(offset).min(TS_PACKET_BYTES.saturating_sub(payload_start));
        packet[payload_start..payload_start.saturating_add(copied)]
            .copy_from_slice(&section[offset..offset.saturating_add(copied)]);
        output.extend_from_slice(&packet);
        offset = offset.saturating_add(copied);
        counter = counter.wrapping_add(1) & 0x0F;
        first = false;
    }
    output
}

fn packetize_section_with_split_syntax_header(pid: u16, section: &[u8], first_header_bytes: usize) -> Vec<u8> {
    let first_header_bytes = first_header_bytes.min(PSI_SYNTAX_HEADER_BYTES.saturating_sub(1));
    let pid = Pid::new(pid);
    let mut output = synthetic_psi_packet(pid, true, &section[..first_header_bytes]).to_vec();
    for chunk in section[first_header_bytes..].chunks(184) {
        output.extend_from_slice(&synthetic_psi_packet(pid, false, chunk));
    }
    output
}

fn psi_start_packet_with_pointer(pid: u16, previous_section: &[u8], section_start: &[u8]) -> [u8; TS_PACKET_BYTES] {
    let payload_length = 1usize.saturating_add(previous_section.len()).saturating_add(section_start.len()).min(184);
    let mut packet = [0xFF_u8; TS_PACKET_BYTES];
    packet[0] = Packet::SYNC_BYTE;
    packet[1] = (u8::try_from(pid >> 8).unwrap_or(0) & 0x1F) | 0x40;
    packet[2] = u8::try_from(pid & 0xFF).unwrap_or(0);
    packet[3] = 0x30;
    let adaptation_length = 183usize.saturating_sub(payload_length);
    packet[4] = u8::try_from(adaptation_length).unwrap_or(182);
    if adaptation_length > 0 {
        packet[5] = 0;
    }
    let payload_offset = 5usize.saturating_add(adaptation_length);
    packet[payload_offset] = u8::try_from(previous_section.len()).unwrap_or(u8::MAX);
    let previous_offset = payload_offset.saturating_add(1);
    let previous_end = previous_offset.saturating_add(previous_section.len());
    packet[previous_offset..previous_end].copy_from_slice(previous_section);
    let section_end = previous_end.saturating_add(section_start.len());
    packet[previous_end..section_end].copy_from_slice(section_start);
    packet
}

fn null_packet() -> [u8; TS_PACKET_BYTES] {
    let mut packet = [0xFF_u8; TS_PACKET_BYTES];
    packet[0] = Packet::SYNC_BYTE;
    packet[1] = 0x1F;
    packet[2] = 0xFF;
    packet[3] = 0x10;
    packet
}

fn track_stream(descriptor_bytes: usize) -> Vec<u8> {
    let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
    stream.extend_from_slice(&packetize_section(
        0x100,
        &pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102), (0x0F, 0x103)], descriptor_bytes),
        0,
    ));
    stream.extend_from_slice(&null_packet());
    stream
}

fn media_payload_packet(
    pid: u16,
    continuity_counter: u8,
    payload_unit_start: bool,
    discontinuity: bool,
    payload: &[u8],
) -> [u8; TS_PACKET_BYTES] {
    assert!(!payload.is_empty() && payload.len() <= 182);
    let mut packet = [0xFF_u8; TS_PACKET_BYTES];
    packet[0] = Packet::SYNC_BYTE;
    packet[1] = u8::try_from(pid >> 8).unwrap_or(0) & 0x1F;
    if payload_unit_start {
        packet[1] |= 0x40;
    }
    packet[2] = u8::try_from(pid & 0xFF).unwrap_or(0);
    packet[3] = 0x30 | (continuity_counter & 0x0F);
    let adaptation_length = 183usize.saturating_sub(payload.len());
    packet[4] = u8::try_from(adaptation_length).unwrap_or(182);
    if adaptation_length > 0 {
        packet[5] = if discontinuity { 0x80 } else { 0 };
    }
    let payload_offset = 5usize.saturating_add(adaptation_length);
    packet[payload_offset..].copy_from_slice(payload);
    packet
}

fn media_adaptation_only_packet(pid: u16, continuity_counter: u8, discontinuity: bool) -> [u8; TS_PACKET_BYTES] {
    let mut packet = [0xFF_u8; TS_PACKET_BYTES];
    packet[0] = Packet::SYNC_BYTE;
    packet[1] = u8::try_from(pid >> 8).unwrap_or(0) & 0x1F;
    packet[2] = u8::try_from(pid & 0xFF).unwrap_or(0);
    packet[3] = 0x20 | (continuity_counter & 0x0F);
    packet[4] = 183;
    packet[5] = if discontinuity { 0x80 } else { 0 };
    packet
}

fn pes_bytes(stream_id: u8, declared_bytes: u16, payload_after_length: &[u8]) -> Vec<u8> {
    let mut pes = vec![0, 0, 1, stream_id];
    pes.extend_from_slice(&declared_bytes.to_be_bytes());
    pes.extend_from_slice(payload_after_length);
    pes
}

async fn complete_media_evidence(bytes: &[u8]) -> HlsTsMediaEvidence {
    complete_media_evidence_with_duration(bytes, 90_000).await
}

async fn complete_media_evidence_with_duration(bytes: &[u8], expected_duration_ticks_90khz: u64) -> HlsTsMediaEvidence {
    let source_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    inspect_mpeg_ts_media_evidence_async(
        bytes,
        HlsTsProbeProtection::Clear,
        HlsTsProbeBudget {
            max_bytes: source_size.saturating_add(1),
            max_packets: source_size.saturating_add(187).saturating_div(188).saturating_add(1),
            ..HlsTsProbeBudget::default()
        },
        expected_duration_ticks_90khz,
    )
    .await
    .expect("complete media evidence")
}

fn valid_track_prefix(minimum_bytes: usize) -> Vec<u8> {
    let mut stream = track_stream(0);
    let null_packet = null_packet();
    while stream.len() < minimum_bytes {
        stream.extend_from_slice(&null_packet);
    }
    stream
}

fn multi_packet_pat_stream() -> Vec<u8> {
    let programs =
        (0_u16..46).map(|index| (index.saturating_add(1), 0x100_u16.saturating_add(index))).collect::<Vec<_>>();
    let mut stream = packetize_section(0, &pat_section(&programs), 0);
    for (program_number, pmt_pid) in programs {
        stream.extend_from_slice(&packetize_section(
            pmt_pid,
            &pmt_section(program_number, 0x400, &[(0x1B, 0x400), (0x0F, 0x401)], 0),
            0,
        ));
    }
    stream.extend_from_slice(&null_packet());
    stream
}

fn found(outcome: HlsTsProbeOutcome) -> HlsTsTrackSignature {
    match outcome {
        HlsTsProbeOutcome::Found(signature) => signature,
        other => panic!("expected signature, got {other:?}"),
    }
}

struct CountingReader {
    inner: Cursor<Vec<u8>>,
    bytes_read: Rc<Cell<usize>>,
}

impl Read for CountingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = std::io::Read::read(&mut self.inner, buffer)?;
        self.bytes_read.set(self.bytes_read.get().saturating_add(read));
        Ok(read)
    }
}

struct VirtualTailReader {
    prefix: Arc<[u8]>,
    logical_len: u64,
    position: u64,
    bytes_read: Arc<AtomicU64>,
}

impl VirtualTailReader {
    fn new(prefix: Arc<[u8]>, logical_len: u64, bytes_read: Arc<AtomicU64>) -> Self {
        Self { prefix, logical_len, position: 0, bytes_read }
    }

    fn read_into(&mut self, output: &mut [u8]) -> usize {
        let remaining = self.logical_len.saturating_sub(self.position);
        let read = usize::try_from(remaining).unwrap_or(usize::MAX).min(output.len());
        if read == 0 {
            return 0;
        }
        let position = usize::try_from(self.position).unwrap_or(usize::MAX);
        let prefix_read = self.prefix.len().saturating_sub(position).min(read);
        if prefix_read > 0 {
            output[..prefix_read].copy_from_slice(&self.prefix[position..position.saturating_add(prefix_read)]);
        }
        output[prefix_read..read].fill(0);
        self.position = self.position.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        self.bytes_read.fetch_add(u64::try_from(read).unwrap_or(u64::MAX), Ordering::Relaxed);
        read
    }
}

impl Read for VirtualTailReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> { Ok(self.read_into(output)) }
}

impl AsyncRead for VirtualTailReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let read = self.as_mut().get_mut().read_into(output.initialize_unfilled());
        output.advance(read);
        Poll::Ready(Ok(()))
    }
}

fn encrypt_aes128_cbc(plaintext: &[u8], key: &[u8; AES_128_BLOCK_BYTES], iv: [u8; AES_128_BLOCK_BYTES]) -> Vec<u8> {
    let cipher = Aes128::new_from_slice(key).expect("valid test key");
    let mut previous = iv;
    let mut output = Vec::with_capacity(plaintext.len());
    for chunk in plaintext.as_chunks::<AES_128_BLOCK_BYTES>().0 {
        let mut block = Block::<Aes128>::default();
        for ((byte, plaintext), previous) in block.iter_mut().zip(chunk).zip(previous) {
            *byte = plaintext ^ previous;
        }
        cipher.encrypt_block(&mut block);
        previous.copy_from_slice(&block);
        output.extend_from_slice(&block);
    }
    output
}

fn encrypt_aes128_cbc_pkcs7(
    plaintext: &[u8],
    key: &[u8; AES_128_BLOCK_BYTES],
    iv: [u8; AES_128_BLOCK_BYTES],
) -> Vec<u8> {
    let padding = AES_128_BLOCK_BYTES - plaintext.len() % AES_128_BLOCK_BYTES;
    let mut padded = plaintext.to_vec();
    padded.resize(padded.len().saturating_add(padding), u8::try_from(padding).unwrap_or(0));
    encrypt_aes128_cbc(&padded, key, iv)
}

mod recovery;
mod storage;
mod terminal;
mod transport;
