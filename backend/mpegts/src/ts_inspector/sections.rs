use super::{
    demux::HlsPatSectionParser, HlsPatPacketFilter, HlsPmtEvidence, HlsPmtPacketFilter, HlsPmtSectionEvidence,
    HlsPmtSectionParser, HlsPsiSectionConsumer, HlsTsDemuxContext, HlsTsMalformedReason, HlsTsProgramTopology,
    PSI_SYNTAX_HEADER_BYTES, TS_PACKET_BYTES,
};
use mpeg2ts_reader::{
    demultiplex,
    demultiplex::PacketFilter,
    packet::{Packet, Pid},
    psi,
    psi::{BufferSectionSyntaxParser, SectionPacketConsumer, SectionProcessor, SectionSyntaxSectionProcessor},
};
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
pub(super) enum HlsPsiTableKind {
    Pat,
    Pmt,
}

impl HlsPsiTableKind {
    pub(super) const fn malformed_reason(self) -> HlsTsMalformedReason {
        match self {
            Self::Pat => HlsTsMalformedReason::InvalidPat,
            Self::Pmt => HlsTsMalformedReason::InvalidPmt,
        }
    }

    pub(super) const fn expected_table_id(self) -> u8 {
        match self {
            Self::Pat => 0x00,
            Self::Pmt => 0x02,
        }
    }

    pub(super) const fn minimum_section_length(self) -> usize {
        match self {
            Self::Pat => 9,
            Self::Pmt => 13,
        }
    }

    pub(super) fn validate_common_header(self, header: &[u8]) -> bool {
        header.len() == psi::SectionCommonHeader::SIZE
            && header[0] == self.expected_table_id()
            && header[1] & 0x80 != 0
            && header[1] & 0x30 == 0x30
            && (self.minimum_section_length()..=1_021).contains(&psi_section_length(header))
    }
}

fn psi_section_length(header: &[u8]) -> usize { (usize::from(header[1] & 0x0F) << 8) | usize::from(header[2]) }

impl<P> HlsPsiSectionConsumer<P>
where
    P: SectionProcessor<Context = HlsTsDemuxContext>,
{
    pub(super) fn new(consumer: SectionPacketConsumer<P>, table_kind: HlsPsiTableKind) -> Self {
        Self { consumer, table_kind, pending_syntax_header: None }
    }

    pub(super) fn consume(&mut self, ctx: &mut HlsTsDemuxContext, packet: &Packet<'_>) {
        if let Some(mut pending) = self.pending_syntax_header.take() {
            let Some(payload) = packet.payload() else {
                self.pending_syntax_header = Some(pending);
                return;
            };
            if packet.payload_unit_start_indicator() {
                let Some((&pointer, section_data)) = payload.split_first() else {
                    ctx.record_malformed(HlsTsMalformedReason::InvalidPsiPointer);
                    return;
                };
                let pointer = usize::from(pointer);
                if pointer >= section_data.len() {
                    ctx.record_malformed(HlsTsMalformedReason::InvalidPsiPointer);
                    return;
                }
                let (previous_section, section_start) = section_data.split_at(pointer);
                pending.extend_from_slice(previous_section);
                if pending.len() < psi::SectionCommonHeader::SIZE
                    || !self.table_kind.validate_common_header(&pending[..psi::SectionCommonHeader::SIZE])
                {
                    ctx.record_malformed(self.table_kind.malformed_reason());
                    return;
                }
                let section_bytes = psi::SectionCommonHeader::SIZE
                    .saturating_add(psi_section_length(&pending[..psi::SectionCommonHeader::SIZE]));
                if pending.len() < section_bytes {
                    ctx.record_malformed(self.table_kind.malformed_reason());
                    return;
                }
                self.consume_synthetic_payload(ctx, packet.pid(), &pending[..section_bytes], true);
                if ctx.malformed.is_none() {
                    self.consume_new_section(ctx, packet.pid(), section_start);
                }
                return;
            }
            pending.extend_from_slice(payload);
            if pending.len() < psi::SectionCommonHeader::SIZE {
                self.pending_syntax_header = Some(pending);
                return;
            }
            if !self.table_kind.validate_common_header(&pending[..psi::SectionCommonHeader::SIZE]) {
                ctx.record_malformed(self.table_kind.malformed_reason());
                return;
            }
            if pending.len() < PSI_SYNTAX_HEADER_BYTES {
                self.pending_syntax_header = Some(pending);
                return;
            }
            let section_bytes = psi::SectionCommonHeader::SIZE
                .saturating_add(psi_section_length(&pending[..psi::SectionCommonHeader::SIZE]))
                .min(pending.len());
            self.consume_synthetic_payload(ctx, packet.pid(), &pending[..section_bytes], true);
            return;
        }

        let Some(payload) = packet.payload() else {
            self.consumer.consume(ctx, packet);
            return;
        };
        if !packet.payload_unit_start_indicator() {
            self.consumer.consume(ctx, packet);
            return;
        }
        let Some((&pointer, section_data)) = payload.split_first() else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPsiPointer);
            return;
        };
        let pointer = usize::from(pointer);
        if pointer >= section_data.len() {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPsiPointer);
            return;
        }
        let section_start = &section_data[pointer..];
        if section_start.len() < PSI_SYNTAX_HEADER_BYTES {
            if let Some(previous_section) = section_data.get(..pointer).filter(|bytes| !bytes.is_empty()) {
                self.consume_synthetic_payload(ctx, packet.pid(), previous_section, false);
            }
            if ctx.malformed.is_none() {
                self.consume_new_section(ctx, packet.pid(), section_start);
            }
            return;
        }
        if !self.table_kind.validate_common_header(&section_start[..psi::SectionCommonHeader::SIZE]) {
            ctx.record_malformed(self.table_kind.malformed_reason());
            return;
        }
        self.consumer.consume(ctx, packet);
    }

    pub(super) fn consume_new_section(&mut self, ctx: &mut HlsTsDemuxContext, pid: Pid, section_start: &[u8]) {
        if section_start.len() < PSI_SYNTAX_HEADER_BYTES {
            if section_start.len() >= psi::SectionCommonHeader::SIZE
                && !self.table_kind.validate_common_header(&section_start[..psi::SectionCommonHeader::SIZE])
            {
                ctx.record_malformed(self.table_kind.malformed_reason());
                return;
            }
            self.pending_syntax_header = Some(section_start.to_vec());
            return;
        }
        if !self.table_kind.validate_common_header(&section_start[..psi::SectionCommonHeader::SIZE]) {
            ctx.record_malformed(self.table_kind.malformed_reason());
            return;
        }
        self.consume_synthetic_payload(ctx, pid, section_start, true);
    }

    pub(super) fn consume_synthetic_payload(
        &mut self,
        ctx: &mut HlsTsDemuxContext,
        pid: Pid,
        bytes: &[u8],
        start: bool,
    ) {
        let mut offset = 0usize;
        let mut first = true;
        while offset < bytes.len() {
            let starts_section = start && first;
            let capacity = if starts_section { 183 } else { 184 };
            let copied = bytes.len().saturating_sub(offset).min(capacity);
            let payload = &bytes[offset..offset.saturating_add(copied)];
            let packet_bytes = synthetic_psi_packet(pid, starts_section, payload);
            if let Some(packet) = Packet::try_new(&packet_bytes) {
                self.consumer.consume(ctx, &packet);
            }
            offset = offset.saturating_add(copied);
            first = false;
        }
    }
}

pub(super) fn synthetic_psi_packet(pid: Pid, starts_section: bool, payload: &[u8]) -> [u8; TS_PACKET_BYTES] {
    let pointer_bytes = usize::from(starts_section);
    let payload_length = payload.len().saturating_add(pointer_bytes).min(184);
    let pid = u16::from(pid);
    let mut packet = [0xFF_u8; TS_PACKET_BYTES];
    packet[0] = Packet::SYNC_BYTE;
    packet[1] = (u8::try_from(pid >> 8).unwrap_or(0) & 0x1F) | if starts_section { 0x40 } else { 0 };
    packet[2] = pid.to_be_bytes()[1];
    let payload_offset = if payload_length == 184 {
        packet[3] = 0x10;
        4
    } else {
        packet[3] = 0x30;
        let adaptation_length = 183usize.saturating_sub(payload_length);
        packet[4] = u8::try_from(adaptation_length).unwrap_or(182);
        if adaptation_length > 0 {
            packet[5] = 0;
        }
        5usize.saturating_add(adaptation_length)
    };
    let data_offset = if starts_section {
        packet[payload_offset] = 0;
        payload_offset.saturating_add(1)
    } else {
        payload_offset
    };
    let copy_length = payload.len().min(TS_PACKET_BYTES.saturating_sub(data_offset));
    packet[data_offset..data_offset.saturating_add(copy_length)].copy_from_slice(&payload[..copy_length]);
    packet
}

impl Default for HlsPatPacketFilter {
    fn default() -> Self {
        Self {
            consumer: HlsPsiSectionConsumer::new(
                SectionPacketConsumer::new(SectionSyntaxSectionProcessor::new(BufferSectionSyntaxParser::new(
                    HlsPatSectionParser,
                ))),
                HlsPsiTableKind::Pat,
            ),
        }
    }
}

impl PacketFilter for HlsPatPacketFilter {
    type Ctx = HlsTsDemuxContext;

    fn consume(&mut self, ctx: &mut Self::Ctx, packet: &Packet<'_>) { self.consumer.consume(ctx, packet); }
}

impl HlsPmtPacketFilter {
    pub(super) fn new(pid: Pid, program_number: u16) -> Self {
        Self {
            consumer: HlsPsiSectionConsumer::new(
                SectionPacketConsumer::new(SectionSyntaxSectionProcessor::new(BufferSectionSyntaxParser::new(
                    HlsPmtSectionParser { pid, program_number },
                ))),
                HlsPsiTableKind::Pmt,
            ),
        }
    }
}

impl PacketFilter for HlsPmtPacketFilter {
    type Ctx = HlsTsDemuxContext;

    fn consume(&mut self, ctx: &mut Self::Ctx, packet: &Packet<'_>) { self.consumer.consume(ctx, packet); }
}

pub(super) enum HlsTsPacketFilter {
    Pat(HlsPatPacketFilter),
    Pmt(HlsPmtPacketFilter),
    Ignore(demultiplex::NullPacketFilter<HlsTsDemuxContext>),
}

impl PacketFilter for HlsTsPacketFilter {
    type Ctx = HlsTsDemuxContext;

    fn consume(&mut self, ctx: &mut Self::Ctx, packet: &Packet<'_>) {
        match self {
            Self::Pat(filter) => filter.consume(ctx, packet),
            Self::Pmt(filter) => filter.consume(ctx, packet),
            Self::Ignore(filter) => filter.consume(ctx, packet),
        }
    }
}

impl HlsPmtEvidence {
    pub(super) fn complete(&self) -> bool {
        self.last_section_number.is_some_and(|last| (0..=last).all(|section| self.sections.contains_key(&section)))
    }

    pub(super) fn restart_version(&mut self, version: u8, last_section_number: u8) {
        self.table_version = Some(version);
        self.last_section_number = Some(last_section_number);
        self.sections.clear();
        self.has_pcr = false;
        self.stream_types.clear();
    }

    pub(super) fn record_section(
        &mut self,
        section_number: u8,
        section: HlsPmtSectionEvidence,
    ) -> Result<(), HlsTsMalformedReason> {
        if self.sections.get(&section_number).is_some_and(|current| current != &section) {
            return Err(HlsTsMalformedReason::InvalidPmt);
        }
        self.sections.insert(section_number, section);
        self.has_pcr = self.sections.values().any(|section| section.pcr_pid != u16::from(mpeg2ts_reader::STUFFING_PID));
        self.stream_types = self
            .sections
            .values()
            .flat_map(|section| section.streams.iter().map(|stream| stream.stream_type))
            .collect();
        Ok(())
    }

    pub(super) fn topology(&self, transport_stream_id: u16, pmt_pid: u16) -> Option<HlsTsProgramTopology> {
        if !self.complete() {
            return None;
        }
        let mut sections = self.sections.values();
        let pcr_pid = sections.next()?.pcr_pid;
        if self.sections.values().any(|section| section.pcr_pid != pcr_pid) {
            return None;
        }
        let streams = self.sections.values().flat_map(|section| section.streams.iter().copied()).collect::<Vec<_>>();
        if streams.is_empty() {
            return None;
        }
        let mut elementary_pids = BTreeSet::new();
        if streams.iter().any(|stream| !elementary_pids.insert(stream.elementary_pid)) {
            return None;
        }
        Some(HlsTsProgramTopology {
            transport_stream_id,
            program_number: self.program_number,
            pmt_pid,
            pcr_pid,
            streams: streams.into(),
        })
    }
}
