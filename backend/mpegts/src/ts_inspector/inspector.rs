use super::{
    protection::HlsTsSourceDecoder, HlsTsDemuxContext, HlsTsInspector, HlsTsMalformedReason, HlsTsMediaEvidence,
    HlsTsMediaStreamInspector, HlsTsProbeBudget, HlsTsProbeError, HlsTsProbeOutcome, HlsTsProbeProtection,
    HlsTsProtectionReason, HlsTsSpliceEvidence, HlsTsSpliceIncompatibility, HlsTsTrackSignature,
    HlsTsTransportStreamInspector, AES_128_BLOCK_BYTES, TS_ALIGNMENT_CONFIRMATION_PACKETS, TS_PACKET_BYTES,
};
use crate::transport_stream_buffer::{HlsTsTimestampProfile, HlsTsTimestampProfileScanner};
use mpeg2ts_reader::{demultiplex, packet::Packet};
use std::io::Read;
use tokio::io::{AsyncRead, AsyncReadExt};
use zeroize::Zeroizing;

#[derive(Clone, Copy)]
pub(super) enum HlsTsPlaintextRemainder {
    ExactPackets,
    Aes128Pkcs7,
}

impl HlsTsMediaStreamInspector {
    pub(super) fn new(budget: HlsTsProbeBudget, expected_duration_ticks_90khz: u64) -> Self {
        Self {
            budget,
            timestamp_scanner: HlsTsTimestampProfileScanner::new(expected_duration_ticks_90khz),
            transport_scanner: HlsTsTransportStreamInspector::new(),
            pending: Vec::with_capacity(budget.bounded_read_chunk_bytes().saturating_add(TS_PACKET_BYTES)),
            aligned: false,
            packets_examined: 0,
            invalid: false,
        }
    }

    pub(super) fn feed_plaintext(&mut self, bytes: &[u8]) {
        if self.invalid {
            return;
        }
        self.pending.extend_from_slice(bytes);
        if !self.aligned {
            let confirmation_bytes = TS_PACKET_BYTES.saturating_mul(TS_ALIGNMENT_CONFIRMATION_PACKETS);
            if self.pending.len() < confirmation_bytes {
                return;
            }
            let available_search = self.pending.len().saturating_sub(confirmation_bytes);
            let search_end = available_search.min(self.budget.max_resync_bytes);
            let alignment = (0..=search_end).find(|offset| {
                (0..TS_ALIGNMENT_CONFIRMATION_PACKETS).all(|index| {
                    self.pending[offset.saturating_add(index.saturating_mul(TS_PACKET_BYTES))] == Packet::SYNC_BYTE
                })
            });
            let Some(alignment) = alignment else {
                if available_search >= self.budget.max_resync_bytes {
                    self.invalid = true;
                }
                return;
            };
            self.pending.drain(..alignment);
            self.aligned = true;
        }

        let mut consumed = 0usize;
        while self.pending.len().saturating_sub(consumed) >= TS_PACKET_BYTES {
            if self.packets_examined >= self.budget.max_packets {
                self.invalid = true;
                break;
            }
            let packet_end = consumed.saturating_add(TS_PACKET_BYTES);
            let packet = &self.pending[consumed..packet_end];
            self.timestamp_scanner.push_aligned_packet(packet);
            self.transport_scanner.push_packet(packet, self.packets_examined);
            self.packets_examined = self.packets_examined.saturating_add(1);
            consumed = packet_end;
        }
        self.pending.drain(..consumed);
    }

    pub(super) fn finish(
        self,
        remainder: HlsTsPlaintextRemainder,
        topology: Option<HlsTsTrackSignature>,
    ) -> (Option<HlsTsTimestampProfile>, HlsTsSpliceEvidence) {
        let valid_remainder = match remainder {
            HlsTsPlaintextRemainder::ExactPackets => self.pending.is_empty(),
            HlsTsPlaintextRemainder::Aes128Pkcs7 => {
                let Some(&padding) = self.pending.last() else {
                    return (
                        None,
                        HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::InvalidPacket {
                            packet_index: self.packets_examined,
                        }),
                    );
                };
                let padding = usize::from(padding);
                (1..=AES_128_BLOCK_BYTES).contains(&padding)
                    && self.pending.len() == padding
                    && self.pending.iter().all(|byte| usize::from(*byte) == padding)
            }
        };
        if self.invalid || !self.aligned || !valid_remainder {
            return (
                None,
                HlsTsSpliceEvidence::Incompatible(if self.packets_examined >= self.budget.max_packets {
                    HlsTsSpliceIncompatibility::InspectionBudgetExhausted
                } else {
                    HlsTsSpliceIncompatibility::InvalidPacket { packet_index: self.packets_examined }
                }),
            );
        }
        (self.timestamp_scanner.finish(), self.transport_scanner.finish(topology))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsTsInspectionMode {
    Prefix,
    Complete,
}

impl HlsTsInspector {
    pub(super) fn new(budget: HlsTsProbeBudget) -> Self {
        let mut context = HlsTsDemuxContext::new();
        let demux = demultiplex::Demultiplex::new(&mut context);
        Self {
            budget,
            demux,
            context,
            pending: Vec::with_capacity(budget.bounded_read_chunk_bytes().saturating_add(TS_PACKET_BYTES)),
            aligned: false,
            source_bytes_examined: 0,
            packets_examined: 0,
        }
    }

    pub(super) fn record_source_bytes(&mut self, bytes: usize) {
        self.source_bytes_examined =
            self.source_bytes_examined.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    pub(super) fn budget_exhausted(&self) -> HlsTsProbeOutcome {
        HlsTsProbeOutcome::ProbeBudgetExhausted {
            bytes_examined: self.source_bytes_examined,
            packets_examined: self.packets_examined,
        }
    }

    pub(super) fn feed_plaintext(&mut self, bytes: &[u8]) -> Option<HlsTsProbeOutcome> {
        self.feed_plaintext_with_mode(bytes, HlsTsInspectionMode::Prefix)
    }

    pub(super) fn feed_plaintext_complete(&mut self, bytes: &[u8]) -> Option<HlsTsProbeOutcome> {
        self.feed_plaintext_with_mode(bytes, HlsTsInspectionMode::Complete)
    }

    pub(super) fn feed_plaintext_with_mode(
        &mut self,
        bytes: &[u8],
        mode: HlsTsInspectionMode,
    ) -> Option<HlsTsProbeOutcome> {
        self.pending.extend_from_slice(bytes);
        if !self.aligned {
            let confirmation_bytes = TS_PACKET_BYTES.saturating_mul(TS_ALIGNMENT_CONFIRMATION_PACKETS);
            if self.pending.len() < confirmation_bytes {
                return None;
            }
            let available_search = self.pending.len().saturating_sub(confirmation_bytes);
            let search_end = available_search.min(self.budget.max_resync_bytes);
            let alignment = (0..=search_end).find(|offset| {
                (0..TS_ALIGNMENT_CONFIRMATION_PACKETS).all(|index| {
                    self.pending[offset.saturating_add(index.saturating_mul(TS_PACKET_BYTES))] == Packet::SYNC_BYTE
                })
            });
            let Some(alignment) = alignment else {
                if available_search >= self.budget.max_resync_bytes {
                    return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization));
                }
                return None;
            };
            self.pending.drain(..alignment);
            self.aligned = true;
        }

        let mut consumed = 0usize;
        while self.pending.len().saturating_sub(consumed) >= TS_PACKET_BYTES {
            if self.packets_examined >= self.budget.max_packets {
                self.pending.drain(..consumed);
                return Some(self.budget_exhausted());
            }
            let mut packet_bytes = [0_u8; TS_PACKET_BYTES];
            packet_bytes.copy_from_slice(&self.pending[consumed..consumed.saturating_add(TS_PACKET_BYTES)]);
            consumed = consumed.saturating_add(TS_PACKET_BYTES);
            self.packets_examined = self.packets_examined.saturating_add(1);
            if let Some(outcome) = self.validate_packet(&packet_bytes) {
                self.pending.drain(..consumed);
                return Some(outcome);
            }
            self.demux.push(&mut self.context, &packet_bytes);
            if let Some(reason) = self.context.malformed {
                self.pending.drain(..consumed);
                return Some(HlsTsProbeOutcome::Malformed(reason));
            }
            if mode == HlsTsInspectionMode::Prefix {
                if let Some(signature) = self.context.signature() {
                    self.pending.drain(..consumed);
                    return Some(HlsTsProbeOutcome::Found(signature));
                }
            }
        }
        self.pending.drain(..consumed);
        None
    }

    pub(super) fn validate_packet(&self, bytes: &[u8; TS_PACKET_BYTES]) -> Option<HlsTsProbeOutcome> {
        if bytes[0] != Packet::SYNC_BYTE {
            return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization));
        }
        if bytes[1] & 0x80 != 0 {
            return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::TransportError));
        }
        if bytes[3] & 0xC0 != 0 {
            return Some(HlsTsProbeOutcome::UnsupportedProtection(HlsTsProtectionReason::TransportScrambling));
        }
        let adaptation_control = (bytes[3] >> 4) & 0b11;
        let payload_offset = match adaptation_control {
            0b01 => 4,
            0b10 if bytes[4] == 183 => TS_PACKET_BYTES,
            0b11 if bytes[4] <= 182 => 5usize.saturating_add(usize::from(bytes[4])),
            0b00 | 0b10 | 0b11 => {
                return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPacketHeader));
            }
            _ => return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPacketHeader)),
        };
        let pid = (u16::from(bytes[1] & 0x1F) << 8) | u16::from(bytes[2]);
        if self.context.is_psi_pid(pid) && bytes[1] & 0x40 != 0 {
            let Some(payload) = bytes.get(payload_offset..) else {
                return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiPointer));
            };
            let Some(pointer) = payload.first().map(|value| usize::from(*value)) else {
                return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiPointer));
            };
            let section_start = pointer.saturating_add(1);
            if section_start >= payload.len() {
                return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiPointer));
            }
        }
        None
    }

    pub(super) fn finish(self) -> HlsTsProbeOutcome {
        if let Some(reason) = self.context.malformed {
            return HlsTsProbeOutcome::Malformed(reason);
        }
        if let Some(signature) = self.context.signature() {
            return HlsTsProbeOutcome::Found(signature);
        }
        if !self.aligned {
            return HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization);
        }
        if !self.pending.is_empty() {
            return HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::IncompletePacket);
        }
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::IncompleteProgramMetadata)
    }

    pub(super) fn finish_complete(mut self, remainder: HlsTsPlaintextRemainder) -> HlsTsProbeOutcome {
        if matches!(remainder, HlsTsPlaintextRemainder::Aes128Pkcs7) {
            if let Some(&padding) = self.pending.last() {
                let padding = usize::from(padding);
                if (1..=AES_128_BLOCK_BYTES).contains(&padding)
                    && self.pending.len() == padding
                    && self.pending.iter().all(|byte| usize::from(*byte) == padding)
                {
                    self.pending.clear();
                }
            }
        }
        self.finish()
    }
}

/// Inspects a blocking reader without retaining or mutating source media bytes.
pub fn inspect_mpeg_ts<R: Read>(
    mut reader: R,
    protection: HlsTsProbeProtection<'_>,
    budget: HlsTsProbeBudget,
) -> Result<HlsTsProbeOutcome, HlsTsProbeError> {
    let mut decoder = HlsTsSourceDecoder::new(protection)?;
    let chunk_bytes = budget.bounded_read_chunk_bytes();
    let mut buffer = Zeroizing::new(vec![0_u8; chunk_bytes]);
    let mut inspector = HlsTsInspector::new(budget);
    loop {
        let remaining = budget.max_bytes.saturating_sub(inspector.source_bytes_examined);
        if remaining == 0 {
            return Ok(inspector.budget_exhausted());
        }
        let read_limit = usize::try_from(remaining).unwrap_or(usize::MAX).min(buffer.len());
        let read = match reader.read(&mut buffer[..read_limit]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(HlsTsProbeError::Io(error)),
        };
        inspector.record_source_bytes(read);
        if let Some(outcome) = decoder.with_plaintext(&buffer[..read], |plaintext| inspector.feed_plaintext(plaintext))
        {
            return Ok(outcome);
        }
    }
    decoder.finish()?;
    Ok(inspector.finish())
}

/// Async cache-file entry point backed by the same parser and CBC state machine.
pub async fn inspect_mpeg_ts_async<R: AsyncRead + Unpin>(
    mut reader: R,
    protection: HlsTsProbeProtection<'_>,
    budget: HlsTsProbeBudget,
) -> Result<HlsTsProbeOutcome, HlsTsProbeError> {
    let mut decoder = HlsTsSourceDecoder::new(protection)?;
    let chunk_bytes = budget.bounded_read_chunk_bytes();
    let mut buffer = Zeroizing::new(vec![0_u8; chunk_bytes]);
    let mut inspector = HlsTsInspector::new(budget);
    loop {
        let remaining = budget.max_bytes.saturating_sub(inspector.source_bytes_examined);
        if remaining == 0 {
            return Ok(inspector.budget_exhausted());
        }
        let read_limit = usize::try_from(remaining).unwrap_or(usize::MAX).min(buffer.len());
        let read = reader.read(&mut buffer[..read_limit]).await.map_err(HlsTsProbeError::Io)?;
        if read == 0 {
            break;
        }
        inspector.record_source_bytes(read);
        if let Some(outcome) = decoder.with_plaintext(&buffer[..read], |plaintext| inspector.feed_plaintext(plaintext))
        {
            return Ok(outcome);
        }
    }
    decoder.finish()?;
    Ok(inspector.finish())
}

/// Collects track compatibility and complete clock evidence from one exact cache reader.
///
/// PAT/PMT discovery may settle from the prefix, while timestamp collection continues to
/// EOF in bounded chunks. Valid AES-CBC padding is excluded from the aligned TS stream.
pub async fn inspect_mpeg_ts_media_evidence_async<R: AsyncRead + Unpin>(
    mut reader: R,
    protection: HlsTsProbeProtection<'_>,
    budget: HlsTsProbeBudget,
    expected_duration_ticks_90khz: u64,
) -> Result<HlsTsMediaEvidence, HlsTsProbeError> {
    let mut decoder = HlsTsSourceDecoder::new(protection)?;
    let plaintext_remainder = decoder.plaintext_remainder();
    let chunk_bytes = budget.bounded_read_chunk_bytes();
    let mut buffer = Zeroizing::new(vec![0_u8; chunk_bytes]);
    let mut track_inspector = HlsTsInspector::new(budget);
    let mut media_inspector = HlsTsMediaStreamInspector::new(budget, expected_duration_ticks_90khz);
    let mut track_outcome = None;
    let mut source_bytes_examined = 0_u64;
    loop {
        let remaining = budget.max_bytes.saturating_sub(source_bytes_examined);
        if remaining == 0 {
            return Ok(HlsTsMediaEvidence {
                track_outcome: track_outcome.unwrap_or_else(|| track_inspector.budget_exhausted()),
                timestamp_profile: None,
                splice_evidence: HlsTsSpliceEvidence::Incompatible(
                    HlsTsSpliceIncompatibility::InspectionBudgetExhausted,
                ),
            });
        }
        let read_limit = usize::try_from(remaining).unwrap_or(usize::MAX).min(buffer.len());
        let read = reader.read(&mut buffer[..read_limit]).await.map_err(HlsTsProbeError::Io)?;
        if read == 0 {
            break;
        }
        source_bytes_examined = source_bytes_examined.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if track_outcome.is_none() {
            track_inspector.record_source_bytes(read);
        }
        decoder.with_plaintext(&buffer[..read], |plaintext| {
            media_inspector.feed_plaintext(plaintext);
            if track_outcome.is_none() {
                track_outcome = track_inspector.feed_plaintext_complete(plaintext);
            }
        });
    }
    decoder.finish()?;
    let track_outcome = track_outcome.unwrap_or_else(|| track_inspector.finish_complete(plaintext_remainder));
    let topology = match &track_outcome {
        HlsTsProbeOutcome::Found(signature) => Some(signature.clone()),
        HlsTsProbeOutcome::ProbeBudgetExhausted { .. }
        | HlsTsProbeOutcome::Malformed(_)
        | HlsTsProbeOutcome::UnsupportedProtection(_) => None,
    };
    let (timestamp_profile, splice_evidence) = media_inspector.finish(plaintext_remainder, topology);
    Ok(HlsTsMediaEvidence { track_outcome, timestamp_profile, splice_evidence })
}
