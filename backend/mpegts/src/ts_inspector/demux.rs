use super::{
    evidence::{is_audio_stream_type, is_video_stream_type},
    sections::HlsTsPacketFilter,
    HlsPatPacketFilter, HlsPmtEvidence, HlsPmtPacketFilter, HlsPmtSectionEvidence, HlsPmtSectionParser,
    HlsTsDemuxContext, HlsTsElementaryStreamBinding, HlsTsMalformedReason, HlsTsTrackSignature,
};
use mpeg2ts_reader::{
    demultiplex,
    demultiplex::{DemuxContext, FilterChangeset, FilterRequest},
    mpegts_crc,
    packet::Pid,
    psi,
    psi::{
        pat::{PatSection, ProgramDescriptor},
        pmt::PmtSection,
        CurrentNext, WholeSectionSyntaxPayloadParser,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

impl HlsTsDemuxContext {
    pub(super) fn new() -> Self {
        Self {
            changeset: FilterChangeset::default(),
            pat_table_identity: None,
            pat_last_section_number: None,
            pat_sections: BTreeMap::new(),
            programs_by_pid: BTreeMap::new(),
            pmt_evidence: BTreeMap::new(),
            malformed: None,
        }
    }

    pub(super) fn record_malformed(&mut self, reason: HlsTsMalformedReason) {
        if self.malformed.is_none() {
            self.malformed = Some(reason);
        }
    }

    pub(super) fn register_program(&mut self, program_number: u16, pid: Pid) {
        let pid_value = u16::from(pid);
        if self.programs_by_pid.get(&pid_value).is_some_and(|registered_program| *registered_program != program_number)
            || self.programs_by_pid.iter().any(|(registered_pid, registered_program)| {
                *registered_program == program_number && *registered_pid != pid_value
            })
        {
            self.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        if self.programs_by_pid.insert(pid_value, program_number).is_none() {
            self.pmt_evidence.insert(pid_value, HlsPmtEvidence { program_number, ..HlsPmtEvidence::default() });
            self.changeset.insert(pid, HlsTsPacketFilter::Pmt(HlsPmtPacketFilter::new(pid, program_number)));
        }
    }

    pub(super) fn restart_pat_version(&mut self, table_identity: (u16, u8)) {
        let mut invalid_registered_pid = false;
        for pid in self.programs_by_pid.keys().copied() {
            match Pid::try_from(pid) {
                Ok(pid) => self.changeset.remove(pid),
                Err(()) => invalid_registered_pid = true,
            }
        }
        self.pat_table_identity = Some(table_identity);
        self.pat_last_section_number = None;
        self.pat_sections.clear();
        self.programs_by_pid.clear();
        self.pmt_evidence.clear();
        if invalid_registered_pid {
            self.record_malformed(HlsTsMalformedReason::InvalidPat);
        }
    }

    pub(super) fn reconcile_pat_programs(&mut self) -> Result<(), HlsTsMalformedReason> {
        let mut programs_by_pid = BTreeMap::new();
        let mut pids_by_program = BTreeMap::new();
        for section in self.pat_sections.values() {
            for (pid, program_number) in section {
                if programs_by_pid.insert(*pid, *program_number).is_some_and(|current| current != *program_number)
                    || pids_by_program.insert(*program_number, *pid).is_some_and(|current| current != *pid)
                {
                    return Err(HlsTsMalformedReason::InvalidPat);
                }
            }
        }
        for (pid, program_number) in programs_by_pid {
            match self.programs_by_pid.get(&pid) {
                Some(current) if *current == program_number => {}
                Some(_) => return Err(HlsTsMalformedReason::InvalidPat),
                None => {
                    let pid = Pid::try_from(pid).map_err(|()| HlsTsMalformedReason::InvalidPat)?;
                    self.register_program(program_number, pid);
                    if self.malformed.is_some() {
                        return Err(HlsTsMalformedReason::InvalidPat);
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn is_psi_pid(&self, pid: u16) -> bool {
        pid == u16::from(psi::pat::PAT_PID) || self.programs_by_pid.contains_key(&pid)
    }

    pub(super) fn signature(&self) -> Option<HlsTsTrackSignature> {
        let pat_complete = self
            .pat_last_section_number
            .is_some_and(|last| (0..=last).all(|section| self.pat_sections.contains_key(&section)));
        if !pat_complete || self.programs_by_pid.is_empty() || self.pmt_evidence.values().any(|pmt| !pmt.complete()) {
            return None;
        }
        let transport_stream_id = self.pat_table_identity?.0;
        let programs = self
            .pmt_evidence
            .iter()
            .map(|(pmt_pid, pmt)| pmt.topology(transport_stream_id, *pmt_pid))
            .collect::<Option<Vec<_>>>()?;
        let stream_types = self
            .pmt_evidence
            .values()
            .flat_map(|pmt| pmt.stream_types.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if !stream_types
            .iter()
            .copied()
            .any(|stream_type| is_audio_stream_type(stream_type) || is_video_stream_type(stream_type))
        {
            return None;
        }
        Some(HlsTsTrackSignature {
            program_count: u16::try_from(self.programs_by_pid.len()).unwrap_or(u16::MAX),
            has_pcr: self.pmt_evidence.values().any(|pmt| pmt.has_pcr),
            stream_types: Arc::from(stream_types),
            programs: programs.into(),
        })
    }
}

impl DemuxContext for HlsTsDemuxContext {
    type F = HlsTsPacketFilter;

    fn filter_changeset(&mut self) -> &mut FilterChangeset<Self::F> { &mut self.changeset }

    fn construct(&mut self, request: FilterRequest<'_, '_>) -> Self::F {
        match request {
            FilterRequest::ByPid(pid) if pid == psi::pat::PAT_PID => {
                HlsTsPacketFilter::Pat(HlsPatPacketFilter::default())
            }
            FilterRequest::Pmt { pid, program_number } => {
                HlsTsPacketFilter::Pmt(HlsPmtPacketFilter::new(pid, program_number))
            }
            FilterRequest::ByPid(_) | FilterRequest::ByStream { .. } | FilterRequest::Nit { .. } => {
                HlsTsPacketFilter::Ignore(demultiplex::NullPacketFilter::default())
            }
        }
    }
}

pub(super) struct HlsPatSectionParser;

impl WholeSectionSyntaxPayloadParser for HlsPatSectionParser {
    type Context = HlsTsDemuxContext;

    fn section<'a>(
        &mut self,
        ctx: &mut Self::Context,
        header: &psi::SectionCommonHeader,
        table_header: &psi::TableSyntaxHeader<'a>,
        data: &'a [u8],
    ) {
        if header.table_id != 0x00 || data.len() < 12 {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        if mpegts_crc::sum32(data) != 0 {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPsiCrc);
            return;
        }
        if table_header.current_next_indicator() == CurrentNext::Next {
            return;
        }
        if table_header.section_number() > table_header.last_section_number() {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        let table_identity = (table_header.id(), table_header.version());
        match ctx.pat_table_identity {
            Some((table_id, _)) if table_id != table_header.id() => {
                ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
                return;
            }
            Some(current) if current != table_identity => ctx.restart_pat_version(table_identity),
            Some(_) => {}
            None => ctx.pat_table_identity = Some(table_identity),
        }
        if ctx.pat_last_section_number.is_some_and(|last| last != table_header.last_section_number()) {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        let payload_start = psi::SectionCommonHeader::SIZE + psi::TableSyntaxHeader::SIZE;
        let payload_end = data.len().saturating_sub(4);
        let Some(payload) = data.get(payload_start..payload_end) else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        };
        if !payload.len().is_multiple_of(4) {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        let mut programs = BTreeMap::new();
        let mut pids_by_program = BTreeMap::new();
        for descriptor in PatSection::new(payload).programs() {
            if let ProgramDescriptor::Program { program_number, pid } = descriptor {
                let pid = u16::from(pid);
                if programs.insert(pid, program_number).is_some_and(|current| current != program_number)
                    || pids_by_program.insert(program_number, pid).is_some_and(|current| current != pid)
                {
                    ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
                    return;
                }
            }
        }
        if ctx.pat_sections.get(&table_header.section_number()).is_some_and(|current| current != &programs) {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        ctx.pat_last_section_number = Some(table_header.last_section_number());
        ctx.pat_sections.insert(table_header.section_number(), programs);
        if let Err(reason) = ctx.reconcile_pat_programs() {
            ctx.record_malformed(reason);
        }
    }
}

impl WholeSectionSyntaxPayloadParser for HlsPmtSectionParser {
    type Context = HlsTsDemuxContext;

    fn section<'a>(
        &mut self,
        ctx: &mut Self::Context,
        header: &psi::SectionCommonHeader,
        table_header: &psi::TableSyntaxHeader<'a>,
        data: &'a [u8],
    ) {
        if header.table_id != 0x02 || data.len() < 16 || table_header.id() != self.program_number {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        }
        if mpegts_crc::sum32(data) != 0 {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPsiCrc);
            return;
        }
        if table_header.current_next_indicator() == CurrentNext::Next {
            return;
        }
        if table_header.section_number() > table_header.last_section_number() {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        }
        let payload_start = psi::SectionCommonHeader::SIZE + psi::TableSyntaxHeader::SIZE;
        let payload_end = data.len().saturating_sub(4);
        let Some(payload) = data.get(payload_start..payload_end) else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        };
        if !pmt_stream_loop_is_well_formed(payload) {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        }
        let Ok(section) = PmtSection::from_bytes(payload) else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        };
        let pid = u16::from(self.pid);
        let Some(evidence) = ctx.pmt_evidence.get_mut(&pid) else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        };
        if evidence.program_number != self.program_number {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        }
        match evidence.table_version {
            Some(version) if version != table_header.version() => {
                evidence.restart_version(table_header.version(), table_header.last_section_number());
            }
            Some(_) => {
                if evidence.last_section_number != Some(table_header.last_section_number()) {
                    ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
                    return;
                }
            }
            None => evidence.restart_version(table_header.version(), table_header.last_section_number()),
        }
        let section_evidence = HlsPmtSectionEvidence {
            pcr_pid: u16::from(section.pcr_pid()),
            streams: section
                .streams()
                .map(|stream| HlsTsElementaryStreamBinding {
                    stream_type: stream.stream_type().0,
                    elementary_pid: u16::from(stream.elementary_pid()),
                })
                .collect(),
        };
        if let Err(reason) = evidence.record_section(table_header.section_number(), section_evidence) {
            ctx.record_malformed(reason);
        }
    }
}

/// `mpeg2ts-reader` intentionally stops its stream iterator at a truncated
/// descriptor. Validate both descriptor loops and ES framing so truncation is
/// a typed malformed outcome instead of a partial, falsely compatible signature.
fn pmt_stream_loop_is_well_formed(payload: &[u8]) -> bool {
    let Some(program_info_length_bytes) = payload.get(2..4) else {
        return false;
    };
    let program_info_length =
        (usize::from(program_info_length_bytes[0] & 0x0F) << 8) | usize::from(program_info_length_bytes[1]);
    let Some(mut offset) = 4usize.checked_add(program_info_length).filter(|offset| *offset <= payload.len()) else {
        return false;
    };
    if !descriptor_loop_is_well_formed(&payload[4..offset]) {
        return false;
    }
    while offset < payload.len() {
        let Some(header) = payload.get(offset..offset.saturating_add(5)) else {
            return false;
        };
        let es_info_length = (usize::from(header[3] & 0x0F) << 8) | usize::from(header[4]);
        let Some(next) = offset.checked_add(5).and_then(|value| value.checked_add(es_info_length)) else {
            return false;
        };
        if next > payload.len() {
            return false;
        }
        if !descriptor_loop_is_well_formed(&payload[offset.saturating_add(5)..next]) {
            return false;
        }
        offset = next;
    }
    true
}

fn descriptor_loop_is_well_formed(mut descriptors: &[u8]) -> bool {
    while !descriptors.is_empty() {
        let Some(header) = descriptors.get(..2) else {
            return false;
        };
        let descriptor_length = usize::from(header[1]);
        let Some(next) = 2usize.checked_add(descriptor_length) else {
            return false;
        };
        let Some(remaining) = descriptors.get(next..) else {
            return false;
        };
        descriptors = remaining;
    }
    true
}
