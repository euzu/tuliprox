use super::{HlsTerminalSpliceDiagnostic, HlsTerminalTailCompatibility};
use log::debug;

impl HlsTerminalSpliceDiagnostic {
    pub(super) fn from_compatibility(compatibility: HlsTerminalTailCompatibility) -> Option<Self> {
        let empty = |result| Self {
            result,
            pid: None,
            packet_index: None,
            expected_cc: None,
            actual_cc: None,
            declared_pes_bytes: None,
            observed_pes_bytes: None,
        };
        match compatibility {
            HlsTerminalTailCompatibility::Compatible => Some(empty("compatible")),
            HlsTerminalTailCompatibility::MissingTrackSignature
            | HlsTerminalTailCompatibility::TrackLayoutMismatch
            | HlsTerminalTailCompatibility::MissingSpliceEvidence
            | HlsTerminalTailCompatibility::SpliceTopologyMismatch => Some(empty("topology-mismatch")),
            HlsTerminalTailCompatibility::SpliceTransportFailure(reason) => {
                let mut diagnostic = empty(reason.result_code());
                match reason {
                    super::super::HlsTsSpliceIncompatibility::InvalidPacket { packet_index }
                    | super::super::HlsTsSpliceIncompatibility::TransportError { packet_index, .. } => {
                        diagnostic.packet_index = Some(packet_index);
                        if let super::super::HlsTsSpliceIncompatibility::TransportError { pid, .. } = reason {
                            diagnostic.pid = Some(pid);
                        }
                    }
                    super::super::HlsTsSpliceIncompatibility::ContinuityFailure {
                        pid,
                        packet_index,
                        expected,
                        actual,
                    } => {
                        diagnostic.pid = Some(pid);
                        diagnostic.packet_index = Some(packet_index);
                        diagnostic.expected_cc = Some(expected);
                        diagnostic.actual_cc = Some(actual);
                    }
                    super::super::HlsTsSpliceIncompatibility::IncompletePes {
                        pid,
                        packet_index,
                        declared_bytes,
                        observed_bytes,
                    } => {
                        diagnostic.pid = Some(pid);
                        diagnostic.packet_index = Some(packet_index);
                        diagnostic.declared_pes_bytes = declared_bytes;
                        diagnostic.observed_pes_bytes = Some(observed_bytes);
                    }
                    super::super::HlsTsSpliceIncompatibility::InvalidPes { pid, packet_index } => {
                        diagnostic.pid = Some(pid);
                        diagnostic.packet_index = Some(packet_index);
                    }
                    super::super::HlsTsSpliceIncompatibility::InspectionBudgetExhausted
                    | super::super::HlsTsSpliceIncompatibility::TopologyUnavailable => {}
                }
                Some(diagnostic)
            }
            HlsTerminalTailCompatibility::MissingAsset
            | HlsTerminalTailCompatibility::TerminalMediaNotReady
            | HlsTerminalTailCompatibility::InvalidAsset
            | HlsTerminalTailCompatibility::AssetRevisionMismatch
            | HlsTerminalTailCompatibility::MissingSafeBase
            | HlsTerminalTailCompatibility::TargetDurationExceeded { .. }
            | HlsTerminalTailCompatibility::ActiveMapRequiresCompatibleFallback
            | HlsTerminalTailCompatibility::UnsupportedEncryptionTransition
            | HlsTerminalTailCompatibility::ContainerMismatch
            | HlsTerminalTailCompatibility::MissingTimestampAnchor
            | HlsTerminalTailCompatibility::InvalidTimestampTransition
            | HlsTerminalTailCompatibility::TransientPassthroughUnsupported
            | HlsTerminalTailCompatibility::ProtectionCapacityExceeded
            | HlsTerminalTailCompatibility::InvalidLeaseRoute
            | HlsTerminalTailCompatibility::ManifestRenderFailed => None,
        }
    }
}

fn format_optional_diagnostic<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "none".to_string(), |value| value.to_string())
}

pub(super) fn log_terminal_splice_compatibility(base_proxy_seq: u64, compatibility: HlsTerminalTailCompatibility) {
    let Some(diagnostic) = HlsTerminalSpliceDiagnostic::from_compatibility(compatibility) else {
        return;
    };
    debug!(
        "HLS terminal TS splice eligibility: base_proxy_seq={} result={} pid={} packet_index={} \
         expected_cc={} actual_cc={} declared_pes_bytes={} observed_pes_bytes={}",
        base_proxy_seq,
        diagnostic.result,
        format_optional_diagnostic(diagnostic.pid),
        format_optional_diagnostic(diagnostic.packet_index),
        format_optional_diagnostic(diagnostic.expected_cc),
        format_optional_diagnostic(diagnostic.actual_cc),
        format_optional_diagnostic(diagnostic.declared_pes_bytes),
        format_optional_diagnostic(diagnostic.observed_pes_bytes),
    );
}
