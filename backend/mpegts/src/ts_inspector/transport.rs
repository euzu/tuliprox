use super::{
    HlsTsCompatibleSpliceEvidence, HlsTsPidBoundaryEvidence, HlsTsPidContinuityState, HlsTsSpliceEvidence,
    HlsTsSpliceIncompatibility, HlsTsTrackSignature, HlsTsTransportStreamInspector, TS_PACKET_BYTES,
};
use mpeg2ts_reader::packet::Packet;

pub(super) enum HlsTsPesInspectionState {
    Header { bytes: [u8; 6], len: usize, started_at_packet_index: u64 },
    Finite { declared_bytes: u16, observed_bytes: u64, started_at_packet_index: u64 },
    UnboundedVideo,
}

fn validated_adaptation_length(packet: &Packet<'_>, bytes: &[u8]) -> Result<Option<usize>, ()> {
    let adaptation_control = packet.adaptation_control();
    if !adaptation_control.has_payload() && !adaptation_control.has_adaptation_field() {
        return Err(());
    }
    let adaptation_length = adaptation_control.has_adaptation_field().then(|| usize::from(bytes[4]));
    match (adaptation_control.has_adaptation_field(), adaptation_control.has_payload(), adaptation_length) {
        (false, true, None) | (true, false, Some(183)) => Ok(adaptation_length),
        (true, true, Some(length)) if length <= 182 => Ok(adaptation_length),
        _ => Err(()),
    }
}

impl HlsTsTransportStreamInspector {
    pub(super) fn new() -> Self {
        let pid_count = usize::from(u16::from(mpeg2ts_reader::STUFFING_PID)).saturating_add(1);
        Self {
            continuity: vec![None; pid_count],
            pes: (0..pid_count).map(|_| None).collect(),
            invalid_pes_starts: vec![None; pid_count],
            incompatible: None,
        }
    }

    pub(super) fn push_packet(&mut self, bytes: &[u8], packet_index: u64) {
        if self.incompatible.is_some() {
            return;
        }
        if bytes.len() != TS_PACKET_BYTES {
            self.incompatible = Some(HlsTsSpliceIncompatibility::InvalidPacket { packet_index });
            return;
        }
        let Some(packet) = Packet::try_new(bytes) else {
            self.incompatible = Some(HlsTsSpliceIncompatibility::InvalidPacket { packet_index });
            return;
        };
        let pid = u16::from(packet.pid());
        if packet.transport_error_indicator() {
            self.incompatible = Some(HlsTsSpliceIncompatibility::TransportError { pid, packet_index });
            return;
        }
        if packet.transport_scrambling_control().is_scrambled() {
            self.incompatible = Some(HlsTsSpliceIncompatibility::InvalidPacket { packet_index });
            return;
        }
        let Ok(adaptation_length) = validated_adaptation_length(&packet, bytes) else {
            self.incompatible = Some(HlsTsSpliceIncompatibility::InvalidPacket { packet_index });
            return;
        };
        let has_payload = packet.adaptation_control().has_payload();
        let discontinuity = adaptation_length.is_some_and(|length| length > 0 && bytes[5] & 0x80 != 0);
        let continuity_counter = packet.continuity_counter().count();
        if let Err(reason) = self.inspect_continuity(pid, packet_index, continuity_counter, has_payload, discontinuity)
        {
            self.incompatible = Some(reason);
            return;
        }
        if discontinuity {
            if let Some(reason) = self.take_discontinuity_pes_incompatibility(pid) {
                self.incompatible = Some(reason);
                return;
            }
        }
        let Some(payload) = packet.payload() else {
            return;
        };
        let previous = self.pes[usize::from(pid)].take();
        match advance_ts_pes_inspection(
            pid,
            packet.payload_unit_start_indicator(),
            payload,
            packet_index,
            previous,
            &mut self.invalid_pes_starts[usize::from(pid)],
        ) {
            Ok(next) => self.pes[usize::from(pid)] = next,
            Err(reason) => self.incompatible = Some(reason),
        }
    }

    pub(super) fn inspect_continuity(
        &mut self,
        pid: u16,
        packet_index: u64,
        continuity_counter: u8,
        has_payload: bool,
        discontinuity: bool,
    ) -> Result<(), HlsTsSpliceIncompatibility> {
        if pid == u16::from(mpeg2ts_reader::STUFFING_PID) {
            return Ok(());
        }
        let state = &mut self.continuity[usize::from(pid)];
        let Some(previous) = state else {
            *state = Some(HlsTsPidContinuityState {
                first_packet_index: packet_index,
                first_continuity_counter: continuity_counter,
                first_has_payload: has_payload,
                first_discontinuity: discontinuity,
                last_continuity_counter: continuity_counter,
            });
            return Ok(());
        };
        let expected = if has_payload {
            previous.last_continuity_counter.wrapping_add(1) & 0x0F
        } else {
            previous.last_continuity_counter
        };
        if !discontinuity && continuity_counter != expected {
            return Err(HlsTsSpliceIncompatibility::ContinuityFailure {
                pid,
                packet_index,
                expected,
                actual: continuity_counter,
            });
        }
        previous.last_continuity_counter = continuity_counter;
        Ok(())
    }

    pub(super) fn take_discontinuity_pes_incompatibility(&mut self, pid: u16) -> Option<HlsTsSpliceIncompatibility> {
        match self.pes[usize::from(pid)].take() {
            Some(HlsTsPesInspectionState::Header { started_at_packet_index, .. }) => {
                Some(HlsTsSpliceIncompatibility::IncompletePes {
                    pid,
                    packet_index: started_at_packet_index,
                    declared_bytes: None,
                    observed_bytes: 0,
                })
            }
            Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }) => {
                Some(HlsTsSpliceIncompatibility::IncompletePes {
                    pid,
                    packet_index: started_at_packet_index,
                    declared_bytes: Some(declared_bytes),
                    observed_bytes,
                })
            }
            Some(HlsTsPesInspectionState::UnboundedVideo) | None => None,
        }
    }

    pub(super) fn finish(mut self, topology: Option<HlsTsTrackSignature>) -> HlsTsSpliceEvidence {
        if let Some(reason) = self.incompatible {
            return HlsTsSpliceEvidence::Incompatible(reason);
        }
        for (pid, state) in self.pes.drain(..).enumerate() {
            let Some(state) = state else {
                continue;
            };
            let pid = u16::try_from(pid).unwrap_or(u16::MAX);
            match state {
                HlsTsPesInspectionState::Header { started_at_packet_index, .. } => {
                    return HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::IncompletePes {
                        pid,
                        packet_index: started_at_packet_index,
                        declared_bytes: None,
                        observed_bytes: 0,
                    });
                }
                HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index } => {
                    return HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::IncompletePes {
                        pid,
                        packet_index: started_at_packet_index,
                        declared_bytes: Some(declared_bytes),
                        observed_bytes,
                    });
                }
                HlsTsPesInspectionState::UnboundedVideo => {}
            }
        }
        let Some(topology) = topology else {
            return HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::TopologyUnavailable);
        };
        for stream in topology.programs.iter().flat_map(|program| program.streams.iter()) {
            if let Some(packet_index) = self.invalid_pes_starts[usize::from(stream.elementary_pid)] {
                return HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::InvalidPes {
                    pid: stream.elementary_pid,
                    packet_index,
                });
            }
        }
        let pid_boundaries = self
            .continuity
            .into_iter()
            .enumerate()
            .filter_map(|(pid, state)| {
                let state = state?;
                Some(HlsTsPidBoundaryEvidence {
                    pid: u16::try_from(pid).unwrap_or(u16::MAX),
                    first_packet_index: state.first_packet_index,
                    first_continuity_counter: state.first_continuity_counter,
                    first_has_payload: state.first_has_payload,
                    first_discontinuity: state.first_discontinuity,
                    last_continuity_counter: state.last_continuity_counter,
                })
            })
            .collect::<Vec<_>>();
        HlsTsSpliceEvidence::Compatible(HlsTsCompatibleSpliceEvidence {
            topology,
            pid_boundaries: pid_boundaries.into(),
        })
    }
}

fn advance_ts_pes_inspection(
    pid: u16,
    payload_unit_start: bool,
    payload: &[u8],
    packet_index: u64,
    previous: Option<HlsTsPesInspectionState>,
    invalid_pes_start: &mut Option<u64>,
) -> Result<Option<HlsTsPesInspectionState>, HlsTsSpliceIncompatibility> {
    let state = if payload_unit_start {
        match previous {
            Some(HlsTsPesInspectionState::Header { started_at_packet_index, .. }) => {
                return Err(HlsTsSpliceIncompatibility::IncompletePes {
                    pid,
                    packet_index: started_at_packet_index,
                    declared_bytes: None,
                    observed_bytes: 0,
                });
            }
            Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }) => {
                return Err(HlsTsSpliceIncompatibility::IncompletePes {
                    pid,
                    packet_index: started_at_packet_index,
                    declared_bytes: Some(declared_bytes),
                    observed_bytes,
                });
            }
            Some(HlsTsPesInspectionState::UnboundedVideo) | None => {}
        }
        Some(HlsTsPesInspectionState::Header { bytes: [0; 6], len: 0, started_at_packet_index: packet_index })
    } else {
        previous
    };
    let (mut bytes, mut len, started_at_packet_index) = match state {
        Some(HlsTsPesInspectionState::Header { bytes, len, started_at_packet_index }) => {
            (bytes, len, started_at_packet_index)
        }
        Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }) => {
            let available = u64::try_from(payload.len()).unwrap_or(u64::MAX);
            let observed_bytes = observed_bytes.saturating_add(available).min(u64::from(declared_bytes));
            return if observed_bytes == u64::from(declared_bytes) {
                Ok(None)
            } else {
                Ok(Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }))
            };
        }
        Some(HlsTsPesInspectionState::UnboundedVideo) => {
            return Ok(Some(HlsTsPesInspectionState::UnboundedVideo));
        }
        None => return Ok(None),
    };
    let copied = payload.len().min(6usize.saturating_sub(len));
    bytes[len..len.saturating_add(copied)].copy_from_slice(&payload[..copied]);
    len = len.saturating_add(copied);
    if len >= 3 && bytes[..3] != [0, 0, 1] {
        if invalid_pes_start.is_none() {
            *invalid_pes_start = Some(started_at_packet_index);
        }
        return Ok(None);
    }
    if len < 6 {
        return Ok(Some(HlsTsPesInspectionState::Header { bytes, len, started_at_packet_index }));
    }
    let stream_id = bytes[3];
    let declared_bytes = u16::from_be_bytes([bytes[4], bytes[5]]);
    if declared_bytes == 0 {
        return if (0xE0..=0xEF).contains(&stream_id) {
            Ok(Some(HlsTsPesInspectionState::UnboundedVideo))
        } else {
            Err(HlsTsSpliceIncompatibility::InvalidPes { pid, packet_index })
        };
    }
    let observed_bytes =
        u64::try_from(payload.len().saturating_sub(copied)).unwrap_or(u64::MAX).min(u64::from(declared_bytes));
    if observed_bytes == u64::from(declared_bytes) {
        Ok(None)
    } else {
        Ok(Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }))
    }
}
