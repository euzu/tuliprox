use super::{
    clock::{
        ts_packet_pid, ADAPTATION_FIELD_FLAG_PCR, HLS_TS_MAX_PENDING_PES_HEADERS, HLS_TS_TIMESTAMP_HEADER_BYTES,
        PES_DTS_OFFSET, PES_PTS_OFFSET, SYNC_BYTE, TS_PACKET_SIZE,
    },
    HlsCompletedPesTimestamps, HlsCompletedTimestampField, HlsPendingPesHeader, HlsPesHeaderAssembler,
    HlsTsPacketEvidence,
};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsTsTimestampFieldKind {
    Pts,
    Dts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlsTsTimestampFieldLocation {
    pub pid: u16,
    pub kind: HlsTsTimestampFieldKind,
    /// Absolute byte offsets in the immutable aligned TS buffer. One field may span packets.
    pub byte_offsets: [usize; 5],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlsTsPcrFieldLocation {
    pub pid: u16,
    pub byte_offset: usize,
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsFiniteTsLayoutError {
    #[error("transport stream asset is empty or invalid")]
    InvalidAsset,
    #[error("transport stream packet layout is invalid")]
    InvalidTransportPacket,
    #[error("transport stream timestamp location is invalid")]
    InvalidTimestampLocation,
    #[error("PES timestamp header for PID {pid} uses an unsupported layout")]
    UnsupportedPesTimestampHeader { pid: u16 },
    #[error("PES {kind:?} field for PID {pid} is invalid")]
    InvalidPesTimestampField { pid: u16, kind: HlsTsTimestampFieldKind },
    #[error("PES timestamp header for PID {pid} was interrupted by a new payload unit")]
    PesTimestampHeaderInterrupted { pid: u16 },
    #[error("PES timestamp header continuity failed for PID {pid}: expected {expected:?}, actual {actual}")]
    PesTimestampContinuityDiscontinuity { pid: u16, expected: Option<u8>, actual: u8 },
    #[error("PES timestamp header for PID {pid} is incomplete")]
    IncompletePesTimestampHeader { pid: u16 },
    #[error("too many concurrent split PES timestamp headers")]
    TooManyPendingPesTimestampHeaders,
    #[error("transport stream contains no presentation clock")]
    PresentationClockUnavailable,
    #[error("PID {pid} has no defensible presentation cadence")]
    PresentationCadenceUnavailable { pid: u16 },
    #[error("transport stream presentation duration overflows its clock domain")]
    PresentationDurationOverflow,
}

impl HlsPendingPesHeader {
    pub(super) fn new(pid: u16, continuity_counter: u8) -> Self {
        Self {
            pid,
            bytes: [0; HLS_TS_TIMESTAMP_HEADER_BYTES],
            byte_offsets: [0; HLS_TS_TIMESTAMP_HEADER_BYTES],
            len: 0,
            expected_len: None,
            last_payload_continuity_counter: continuity_counter,
        }
    }
}

impl HlsTsPacketEvidence {
    pub(super) const fn has_payload(self) -> bool { self.payload_offset.is_some() }
}

pub(super) fn inspect_hls_ts_packet(
    packet: &[u8],
    packet_start: usize,
) -> Result<HlsTsPacketEvidence, HlsFiniteTsLayoutError> {
    if packet.len() != TS_PACKET_SIZE || packet[0] != SYNC_BYTE || packet[1] & 0x80 != 0 || packet[3] & 0xC0 != 0 {
        return Err(HlsFiniteTsLayoutError::InvalidTransportPacket);
    }
    let pid = ts_packet_pid(packet);
    let adaptation_field_control = (packet[3] >> 4) & 0b11;
    let continuity_counter = packet[3] & 0x0F;
    let (payload_offset, adaptation_length) = match adaptation_field_control {
        0b01 => (Some(4), None),
        0b10 if packet[4] == 183 => (None, Some(183)),
        0b11 if packet[4] <= 182 => {
            let adaptation_length = usize::from(packet[4]);
            (Some(5usize.saturating_add(adaptation_length)), Some(adaptation_length))
        }
        _ => return Err(HlsFiniteTsLayoutError::InvalidTransportPacket),
    };
    let mut discontinuity = false;
    let mut pcr_field = None;
    if let Some(adaptation_length) = adaptation_length {
        if adaptation_length > 0 {
            discontinuity = packet[5] & 0x80 != 0;
            if packet[5] & ADAPTATION_FIELD_FLAG_PCR != 0 {
                if adaptation_length < 7 {
                    return Err(HlsFiniteTsLayoutError::InvalidTransportPacket);
                }
                pcr_field = Some(HlsTsPcrFieldLocation {
                    pid,
                    byte_offset: packet_start.checked_add(6).ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?,
                });
            }
        }
    }
    Ok(HlsTsPacketEvidence {
        pid,
        payload_unit_start: packet[1] & 0x40 != 0,
        payload_offset,
        continuity_counter,
        discontinuity,
        pcr_field,
    })
}

fn pes_stream_id_has_optional_header(stream_id: u8) -> bool {
    !matches!(
        stream_id,
        0xBC | // program_stream_map
        0xBE | // padding_stream
        0xBF | // private_stream_2
        0xF0 | // ECM
        0xF1 | // EMM
        0xFF | // program_stream_directory
        0xF2 | // DSM-CC
        0xF8 // ITU-T Rec. H.222.1 type E
    )
}

fn timestamp_field_from_pending(
    pending: &HlsPendingPesHeader,
    kind: HlsTsTimestampFieldKind,
    start: usize,
    prefix: u8,
) -> Result<HlsCompletedTimestampField, HlsFiniteTsLayoutError> {
    let end =
        start.checked_add(5).ok_or(HlsFiniteTsLayoutError::InvalidPesTimestampField { pid: pending.pid, kind })?;
    let bytes: [u8; 5] = pending
        .bytes
        .get(start..end)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(HlsFiniteTsLayoutError::InvalidPesTimestampField { pid: pending.pid, kind })?;
    let byte_offsets: [usize; 5] = pending
        .byte_offsets
        .get(start..end)
        .and_then(|offsets| offsets.try_into().ok())
        .ok_or(HlsFiniteTsLayoutError::InvalidPesTimestampField { pid: pending.pid, kind })?;
    if bytes[0] >> 4 != prefix || bytes[0] & 1 == 0 || bytes[2] & 1 == 0 || bytes[4] & 1 == 0 {
        return Err(HlsFiniteTsLayoutError::InvalidPesTimestampField { pid: pending.pid, kind });
    }
    Ok(HlsCompletedTimestampField {
        location: HlsTsTimestampFieldLocation { pid: pending.pid, kind, byte_offsets },
        bytes,
    })
}

fn complete_pes_timestamps(pending: &HlsPendingPesHeader) -> Result<HlsCompletedPesTimestamps, HlsFiniteTsLayoutError> {
    let pts_dts_flags = (pending.bytes[7] >> 6) & 0b11;
    match pts_dts_flags {
        0b00 => Ok(HlsCompletedPesTimestamps::default()),
        0b10 => Ok(HlsCompletedPesTimestamps {
            fields: [
                Some(timestamp_field_from_pending(pending, HlsTsTimestampFieldKind::Pts, PES_PTS_OFFSET, 0x02)?),
                None,
            ],
        }),
        0b11 => Ok(HlsCompletedPesTimestamps {
            fields: [
                Some(timestamp_field_from_pending(pending, HlsTsTimestampFieldKind::Pts, PES_PTS_OFFSET, 0x03)?),
                Some(timestamp_field_from_pending(pending, HlsTsTimestampFieldKind::Dts, PES_DTS_OFFSET, 0x01)?),
            ],
        }),
        _ => Err(HlsFiniteTsLayoutError::UnsupportedPesTimestampHeader { pid: pending.pid }),
    }
}

fn append_pending_pes_header(
    pending: &mut HlsPendingPesHeader,
    payload: &[u8],
    payload_start: usize,
) -> Result<Option<HlsCompletedPesTimestamps>, HlsFiniteTsLayoutError> {
    for (payload_index, byte) in payload.iter().copied().enumerate() {
        if pending.expected_len.is_some_and(|expected| pending.len >= expected) {
            break;
        }
        if pending.len >= HLS_TS_TIMESTAMP_HEADER_BYTES {
            return Err(HlsFiniteTsLayoutError::UnsupportedPesTimestampHeader { pid: pending.pid });
        }
        pending.bytes[pending.len] = byte;
        pending.byte_offsets[pending.len] =
            payload_start.checked_add(payload_index).ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?;
        pending.len = pending.len.saturating_add(1);

        if pending.len == 4 && !pes_stream_id_has_optional_header(pending.bytes[3]) {
            pending.expected_len = Some(4);
        }
        if pending.len == 7 && (pending.bytes[6] & 0xC0) != 0x80 {
            return Err(HlsFiniteTsLayoutError::UnsupportedPesTimestampHeader { pid: pending.pid });
        }
        if pending.len == PES_PTS_OFFSET {
            let header_data_length = usize::from(pending.bytes[8]);
            pending.expected_len = match (pending.bytes[7] >> 6) & 0b11 {
                0b00 => Some(PES_PTS_OFFSET),
                0b10 if header_data_length >= 5 => Some(PES_PTS_OFFSET + 5),
                0b11 if header_data_length >= 10 => Some(PES_DTS_OFFSET + 5),
                _ => return Err(HlsFiniteTsLayoutError::UnsupportedPesTimestampHeader { pid: pending.pid }),
            };
        }
    }

    if pending.expected_len.is_some_and(|expected| pending.len >= expected) {
        return complete_pes_timestamps(pending).map(Some);
    }
    Ok(None)
}

impl HlsPesHeaderAssembler {
    pub(super) fn new() -> Self { Self { pending: HashMap::with_capacity(8) } }

    pub(super) fn push_packet(
        &mut self,
        packet: &[u8],
        packet_start: usize,
        evidence: HlsTsPacketEvidence,
    ) -> Result<HlsCompletedPesTimestamps, HlsFiniteTsLayoutError> {
        if evidence.payload_unit_start && self.pending.contains_key(&evidence.pid) {
            return Err(HlsFiniteTsLayoutError::PesTimestampHeaderInterrupted { pid: evidence.pid });
        }
        if evidence.discontinuity && self.pending.contains_key(&evidence.pid) {
            return Err(HlsFiniteTsLayoutError::PesTimestampContinuityDiscontinuity {
                pid: evidence.pid,
                expected: None,
                actual: evidence.continuity_counter,
            });
        }
        let Some(payload_offset) = evidence.payload_offset else {
            return Ok(HlsCompletedPesTimestamps::default());
        };
        let payload = packet.get(payload_offset..).ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?;
        let payload_start =
            packet_start.checked_add(payload_offset).ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?;

        if let Some(mut pending) = self.pending.remove(&evidence.pid) {
            let expected = pending.last_payload_continuity_counter.wrapping_add(1) & 0x0F;
            if evidence.continuity_counter != expected {
                return Err(HlsFiniteTsLayoutError::PesTimestampContinuityDiscontinuity {
                    pid: evidence.pid,
                    expected: Some(expected),
                    actual: evidence.continuity_counter,
                });
            }
            pending.last_payload_continuity_counter = evidence.continuity_counter;
            if let Some(completed) = append_pending_pes_header(&mut pending, payload, payload_start)? {
                return Ok(completed);
            }
            self.pending.insert(evidence.pid, pending);
            return Ok(HlsCompletedPesTimestamps::default());
        }

        if !evidence.payload_unit_start || payload.len() < 3 || !payload.starts_with(&[0x00, 0x00, 0x01]) {
            return Ok(HlsCompletedPesTimestamps::default());
        }
        if self.pending.len() >= HLS_TS_MAX_PENDING_PES_HEADERS {
            return Err(HlsFiniteTsLayoutError::TooManyPendingPesTimestampHeaders);
        }
        let mut pending = HlsPendingPesHeader::new(evidence.pid, evidence.continuity_counter);
        if let Some(completed) = append_pending_pes_header(&mut pending, payload, payload_start)? {
            return Ok(completed);
        }
        self.pending.insert(evidence.pid, pending);
        Ok(HlsCompletedPesTimestamps::default())
    }

    pub(super) fn finish(self) -> Result<(), HlsFiniteTsLayoutError> {
        match self.pending.keys().copied().min() {
            Some(pid) => Err(HlsFiniteTsLayoutError::IncompletePesTimestampHeader { pid }),
            None => Ok(()),
        }
    }
}
