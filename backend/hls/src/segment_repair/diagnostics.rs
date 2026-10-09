use super::{
    window::RepairVideoCodec, HlsSegmentRepairDecision, HlsSegmentRepairObjectContext, RepairRemuxDroppedStream,
};
#[cfg(any(test, feature = "test-support"))]
use super::{RepairRemuxStreamSelection, SegmentProbe};
use log::debug;
use shared::model::HlsSegmentRepairMode;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[allow(dead_code)]
pub(super) enum HlsSegmentRepairWarningKind {
    MissingPat,
    MissingPmt,
    MissingPmtPid,
    MissingVideoPidInPmt,
    MissingAudioPidInPmt,
    MultiplePrograms,
    PesPacketSizeMismatch,
    PacketCorrupt,
    ContinuityCheckFailed,
    MissingVps,
    MissingSps,
    MissingPps,
    VpsOutOfRange,
    SpsOutOfRange,
    PpsOutOfRange,
    NoFrame,
    DecodeSliceHeaderError,
    MissingPicture,
    InvalidNal,
    InvalidData,
    CodecParametersMissing,
    MmcoUnrefShortFailure,
    ReorderBufferIncrease,
    PpsIdOutOfRange,
    NoStartCode,
    NalSplitError,
    NalParseError,
    InvalidVclNalu,
    InvalidMetadataNalu,
    MultipleDolbyVisionRpus,
    AvDesync,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum HlsSegmentRepairTriggerSource {
    Off,
    CommonMpegTsLow,
    H264Medium,
    H264High,
    HevcMedium,
    HevcHigh,
    UnsupportedCodec,
}

impl HlsSegmentRepairTriggerSource {
    pub(super) const fn as_log_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::CommonMpegTsLow => "common-mpegts-low",
            Self::H264Medium => "h264-medium",
            Self::H264High => "h264-high",
            Self::HevcMedium => "hevc-medium",
            Self::HevcHigh => "hevc-high",
            Self::UnsupportedCodec => "unsupported-codec",
        }
    }
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct WarningCounters {
    pub missing_pat: u32,
    pub missing_pmt: u32,
    pub missing_pmt_pid: u32,
    pub missing_video_pid_in_pmt: u32,
    pub missing_audio_pid_in_pmt: u32,
    pub multiple_programs: u32,
    pub pes_packet_size_mismatch: u32,
    pub packet_corrupt: u32,
    pub continuity_check_failed: u32,
    pub missing_vps: u32,
    pub missing_sps: u32,
    pub missing_pps: u32,
    pub vps_out_of_range: u32,
    pub sps_out_of_range: u32,
    pub pps_out_of_range: u32,
    pub no_frame: u32,
    pub decode_slice_header_error: u32,
    pub missing_picture: u32,
    pub invalid_nal: u32,
    pub invalid_data: u32,
    pub codec_parameters_missing: u32,
    pub mmco_unref_short_failure: u32,
    pub reorder_buffer: u32,
    pub pps_id_out_of_range: u32,
    pub no_start_code: u32,
    pub nal_split_error: u32,
    pub nal_parse_error: u32,
    pub invalid_undecodable_nalu_total: u32,
    pub invalid_undecodable_nalu_non_metadata: u32,
    pub invalid_undecodable_nalu_keyframe: u32,
    pub invalid_undecodable_nalu_metadata: u32,
    pub dolby_vision_rpu: u32,
    pub av_desync: u32,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum SegmentProbeStreamType {
    Video,
    Audio,
    Other,
}

#[cfg(any(test, feature = "test-support"))]
impl RepairRemuxStreamSelection {
    pub(super) fn preserve_all(probe: &SegmentProbe) -> Self {
        Self { mapped_streams: probe.streams.iter().map(|stream| stream.index).collect(), dropped_streams: Vec::new() }
    }
}

pub fn parse_ffmpeg_warnings(stderr: &str) -> WarningCounters {
    let mut counters = WarningCounters::default();
    let mut last_increment: Option<fn(&mut WarningCounters, u32)> = None;
    for line in stderr.lines() {
        let trimmed = line.trim();
        if let Some(repeated) = parse_repeated_count(trimmed) {
            if let Some(increment) = last_increment {
                increment(&mut counters, repeated);
            }
            continue;
        }
        if let Some(increment) = warning_increment(trimmed) {
            increment(&mut counters, 1);
            last_increment = Some(increment);
        }
    }
    counters
}

fn parse_repeated_count(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("Last message repeated ")?;
    let count = rest.strip_suffix(" times")?.parse().ok()?;
    Some(count)
}

#[allow(clippy::too_many_lines)]
fn warning_increment(line: &str) -> Option<fn(&mut WarningCounters, u32)> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("missing pat") {
        return Some(|counters, count| counters.missing_pat = counters.missing_pat.saturating_add(count));
    }
    if lower.contains("missing pmt pid") {
        return Some(|counters, count| counters.missing_pmt_pid = counters.missing_pmt_pid.saturating_add(count));
    }
    if lower.contains("missing pmt") {
        return Some(|counters, count| counters.missing_pmt = counters.missing_pmt.saturating_add(count));
    }
    if lower.contains("missing video pid") && lower.contains("pmt") {
        return Some(|counters, count| {
            counters.missing_video_pid_in_pmt = counters.missing_video_pid_in_pmt.saturating_add(count);
        });
    }
    if lower.contains("missing audio pid") && lower.contains("pmt") {
        return Some(|counters, count| {
            counters.missing_audio_pid_in_pmt = counters.missing_audio_pid_in_pmt.saturating_add(count);
        });
    }
    if lower.contains("multiple mpeg-ts programs") || lower.contains("multiple programs") {
        return Some(|counters, count| counters.multiple_programs = counters.multiple_programs.saturating_add(count));
    }
    if lower.contains("pes packet size mismatch") {
        return Some(|counters, count| {
            counters.pes_packet_size_mismatch = counters.pes_packet_size_mismatch.saturating_add(count);
        });
    }
    if lower.contains("packet corrupt") {
        return Some(|counters, count| counters.packet_corrupt = counters.packet_corrupt.saturating_add(count));
    }
    if lower.contains("continuity check failed") {
        return Some(|counters, count| {
            counters.continuity_check_failed = counters.continuity_check_failed.saturating_add(count);
        });
    }
    if lower.contains("non-existing vps") || lower.contains("missing vps") {
        return Some(|counters, count| counters.missing_vps = counters.missing_vps.saturating_add(count));
    }
    if lower.contains("non-existing sps") || lower.contains("missing sps") {
        return Some(|counters, count| counters.missing_sps = counters.missing_sps.saturating_add(count));
    }
    if lower.contains("non-existing pps") || lower.contains("missing pps") {
        return Some(|counters, count| counters.missing_pps = counters.missing_pps.saturating_add(count));
    }
    if lower.contains("vps id out of range") {
        return Some(|counters, count| counters.vps_out_of_range = counters.vps_out_of_range.saturating_add(count));
    }
    if lower.contains("sps id out of range") {
        return Some(|counters, count| counters.sps_out_of_range = counters.sps_out_of_range.saturating_add(count));
    }
    if lower.contains("pps id out of range") {
        return Some(|counters, count| {
            counters.pps_id_out_of_range = counters.pps_id_out_of_range.saturating_add(count);
            counters.pps_out_of_range = counters.pps_out_of_range.saturating_add(count);
        });
    }
    if lower.contains("vps") && lower.contains("out of range") {
        return Some(|counters, count| counters.vps_out_of_range = counters.vps_out_of_range.saturating_add(count));
    }
    if lower.contains("sps") && lower.contains("out of range") {
        return Some(|counters, count| counters.sps_out_of_range = counters.sps_out_of_range.saturating_add(count));
    }
    if lower.contains("pps") && lower.contains("out of range") {
        return Some(|counters, count| counters.pps_out_of_range = counters.pps_out_of_range.saturating_add(count));
    }
    if lower.contains("no frame") {
        return Some(|counters, count| counters.no_frame = counters.no_frame.saturating_add(count));
    }
    if lower.contains("decode_slice_header error") {
        return Some(|counters, count| {
            counters.decode_slice_header_error = counters.decode_slice_header_error.saturating_add(count);
        });
    }
    if lower.contains("missing picture") {
        return Some(|counters, count| counters.missing_picture = counters.missing_picture.saturating_add(count));
    }
    if lower.contains("invalid nal") {
        return Some(|counters, count| counters.invalid_nal = counters.invalid_nal.saturating_add(count));
    }
    if lower.contains("invalid data found") {
        return Some(|counters, count| counters.invalid_data = counters.invalid_data.saturating_add(count));
    }
    if lower.contains("could not find codec parameters") {
        return Some(|counters, count| {
            counters.codec_parameters_missing = counters.codec_parameters_missing.saturating_add(count);
        });
    }
    if lower.contains("mmco: unref short failure") {
        return Some(|counters, count| {
            counters.mmco_unref_short_failure = counters.mmco_unref_short_failure.saturating_add(count);
        });
    }
    if lower.contains("increasing reorder buffer") {
        return Some(|counters, count| counters.reorder_buffer = counters.reorder_buffer.saturating_add(count));
    }
    if lower.contains("no start code is found") {
        return Some(|counters, count| counters.no_start_code = counters.no_start_code.saturating_add(count));
    }
    if lower.contains("error splitting") && lower.contains("nal") {
        return Some(|counters, count| counters.nal_split_error = counters.nal_split_error.saturating_add(count));
    }
    if lower.contains("error parsing nal unit") {
        return Some(|counters, count| counters.nal_parse_error = counters.nal_parse_error.saturating_add(count));
    }
    if let Some(nalu_type) = parse_invalid_undecodable_nalu_type(line) {
        return Some(match nalu_type {
            0..=20 | 22..=31 => |counters: &mut WarningCounters, count| {
                counters.invalid_undecodable_nalu_total = counters.invalid_undecodable_nalu_total.saturating_add(count);
                counters.invalid_undecodable_nalu_non_metadata =
                    counters.invalid_undecodable_nalu_non_metadata.saturating_add(count);
            },
            21 => |counters: &mut WarningCounters, count| {
                counters.invalid_undecodable_nalu_total = counters.invalid_undecodable_nalu_total.saturating_add(count);
                counters.invalid_undecodable_nalu_keyframe =
                    counters.invalid_undecodable_nalu_keyframe.saturating_add(count);
            },
            39 => |counters: &mut WarningCounters, count| {
                counters.invalid_undecodable_nalu_total = counters.invalid_undecodable_nalu_total.saturating_add(count);
                counters.invalid_undecodable_nalu_metadata =
                    counters.invalid_undecodable_nalu_metadata.saturating_add(count);
            },
            _ => |counters: &mut WarningCounters, count| {
                counters.invalid_undecodable_nalu_total = counters.invalid_undecodable_nalu_total.saturating_add(count);
            },
        });
    }
    if lower.contains("multiple dolby vision rpus found in one au") {
        return Some(|counters, count| counters.dolby_vision_rpu = counters.dolby_vision_rpu.saturating_add(count));
    }
    if lower.contains("audio/video desynchronisation detected") {
        return Some(|counters, count| counters.av_desync = counters.av_desync.saturating_add(count));
    }
    None
}

fn parse_invalid_undecodable_nalu_type(line: &str) -> Option<u8> {
    let (_, rest) = line.split_once("Skipping invalid undecodable NALU:")?;
    rest.trim().split(|ch: char| !ch.is_ascii_digit()).next().filter(|value| !value.is_empty())?.parse().ok()
}

fn warning_count(warnings: &WarningCounters, kind: HlsSegmentRepairWarningKind) -> u32 {
    match kind {
        HlsSegmentRepairWarningKind::MissingPat => warnings.missing_pat,
        HlsSegmentRepairWarningKind::MissingPmt => warnings.missing_pmt,
        HlsSegmentRepairWarningKind::MissingPmtPid => warnings.missing_pmt_pid,
        HlsSegmentRepairWarningKind::MissingVideoPidInPmt => warnings.missing_video_pid_in_pmt,
        HlsSegmentRepairWarningKind::MissingAudioPidInPmt => warnings.missing_audio_pid_in_pmt,
        HlsSegmentRepairWarningKind::MultiplePrograms => warnings.multiple_programs,
        HlsSegmentRepairWarningKind::PesPacketSizeMismatch => warnings.pes_packet_size_mismatch,
        HlsSegmentRepairWarningKind::PacketCorrupt => warnings.packet_corrupt,
        HlsSegmentRepairWarningKind::ContinuityCheckFailed => warnings.continuity_check_failed,
        HlsSegmentRepairWarningKind::MissingVps => warnings.missing_vps,
        HlsSegmentRepairWarningKind::MissingSps => warnings.missing_sps,
        HlsSegmentRepairWarningKind::MissingPps => warnings.missing_pps,
        HlsSegmentRepairWarningKind::VpsOutOfRange => warnings.vps_out_of_range,
        HlsSegmentRepairWarningKind::SpsOutOfRange => warnings.sps_out_of_range,
        HlsSegmentRepairWarningKind::PpsOutOfRange => warnings.pps_out_of_range,
        HlsSegmentRepairWarningKind::NoFrame => warnings.no_frame,
        HlsSegmentRepairWarningKind::DecodeSliceHeaderError => warnings.decode_slice_header_error,
        HlsSegmentRepairWarningKind::MissingPicture => warnings.missing_picture,
        HlsSegmentRepairWarningKind::InvalidNal => warnings.invalid_nal,
        HlsSegmentRepairWarningKind::InvalidData => warnings.invalid_data,
        HlsSegmentRepairWarningKind::CodecParametersMissing => warnings.codec_parameters_missing,
        HlsSegmentRepairWarningKind::MmcoUnrefShortFailure => warnings.mmco_unref_short_failure,
        HlsSegmentRepairWarningKind::ReorderBufferIncrease => warnings.reorder_buffer,
        HlsSegmentRepairWarningKind::PpsIdOutOfRange => warnings.pps_id_out_of_range,
        HlsSegmentRepairWarningKind::NoStartCode => warnings.no_start_code,
        HlsSegmentRepairWarningKind::NalSplitError => warnings.nal_split_error,
        HlsSegmentRepairWarningKind::NalParseError => warnings.nal_parse_error,
        HlsSegmentRepairWarningKind::InvalidVclNalu => {
            warnings.invalid_undecodable_nalu_non_metadata.saturating_add(warnings.invalid_undecodable_nalu_keyframe)
        }
        HlsSegmentRepairWarningKind::InvalidMetadataNalu => warnings.invalid_undecodable_nalu_metadata,
        HlsSegmentRepairWarningKind::MultipleDolbyVisionRpus => warnings.dolby_vision_rpu,
        HlsSegmentRepairWarningKind::AvDesync => warnings.av_desync,
    }
}

fn has_any_warning(warnings: &WarningCounters, kinds: &[HlsSegmentRepairWarningKind]) -> bool {
    kinds.iter().any(|kind| warning_count(warnings, *kind) > 0)
}

fn common_mpegts_low_trigger(warnings: &WarningCounters) -> bool {
    has_any_warning(
        warnings,
        &[
            HlsSegmentRepairWarningKind::MissingPat,
            HlsSegmentRepairWarningKind::MissingPmt,
            HlsSegmentRepairWarningKind::MissingPmtPid,
            HlsSegmentRepairWarningKind::MissingVideoPidInPmt,
            HlsSegmentRepairWarningKind::MissingAudioPidInPmt,
            HlsSegmentRepairWarningKind::MultiplePrograms,
            HlsSegmentRepairWarningKind::PesPacketSizeMismatch,
            HlsSegmentRepairWarningKind::PacketCorrupt,
            HlsSegmentRepairWarningKind::ContinuityCheckFailed,
            HlsSegmentRepairWarningKind::CodecParametersMissing,
        ],
    )
}

fn h264_parameter_trigger(warnings: &WarningCounters) -> bool {
    has_any_warning(
        warnings,
        &[
            HlsSegmentRepairWarningKind::MissingSps,
            HlsSegmentRepairWarningKind::MissingPps,
            HlsSegmentRepairWarningKind::SpsOutOfRange,
            HlsSegmentRepairWarningKind::PpsOutOfRange,
        ],
    )
}

fn h264_medium_trigger(warnings: &WarningCounters) -> bool {
    h264_parameter_trigger(warnings)
        || warnings.decode_slice_header_error > 0
        || warnings.no_frame > 0
        || (warnings.missing_picture > 0
            && (warnings.missing_sps > 0 || warnings.missing_pps > 0 || warnings.decode_slice_header_error > 0))
        || (warnings.invalid_nal > 0 && h264_parameter_trigger(warnings))
        || (warnings.no_start_code > 0 && (warnings.missing_sps > 0 || warnings.missing_pps > 0))
        || (warnings.nal_split_error > 0 && (warnings.missing_sps > 0 || warnings.missing_pps > 0))
}

fn hevc_missing_parameter_trigger(warnings: &WarningCounters) -> bool {
    warnings.missing_vps > 0 || warnings.missing_sps > 0 || warnings.missing_pps > 0
}

fn hevc_parameter_trigger(warnings: &WarningCounters) -> bool {
    hevc_missing_parameter_trigger(warnings)
        || warnings.vps_out_of_range > 0
        || warnings.sps_out_of_range > 0
        || warnings.pps_out_of_range > 0
        || warnings.pps_id_out_of_range > 0
}

fn hevc_invalid_vcl_nalu(warnings: &WarningCounters) -> bool {
    warning_count(warnings, HlsSegmentRepairWarningKind::InvalidVclNalu) > 0
}

fn hevc_medium_trigger(warnings: &WarningCounters) -> bool {
    hevc_parameter_trigger(warnings)
        || (hevc_invalid_vcl_nalu(warnings) && hevc_parameter_trigger(warnings))
        || (warnings.nal_parse_error > 0 && hevc_parameter_trigger(warnings))
        || (warnings.invalid_nal > 0 && hevc_parameter_trigger(warnings))
        || (warnings.no_start_code > 0 && hevc_missing_parameter_trigger(warnings))
        || (warnings.nal_split_error > 0 && hevc_missing_parameter_trigger(warnings))
}

pub(super) fn decide_repair(codec: RepairVideoCodec, warnings: &WarningCounters) -> HlsSegmentRepairDecision {
    let common_low_trigger = common_mpegts_low_trigger(warnings);
    let codec_parameters_missing = warnings.codec_parameters_missing > 0;
    let codec_medium_trigger = match codec {
        RepairVideoCodec::H264 => h264_medium_trigger(warnings),
        RepairVideoCodec::Hevc => hevc_medium_trigger(warnings),
        RepairVideoCodec::Unsupported => false,
    };
    match codec {
        RepairVideoCodec::H264 if codec_medium_trigger && (common_low_trigger || codec_parameters_missing) => {
            HlsSegmentRepairDecision {
                codec,
                required_level: HlsSegmentRepairMode::High,
                trigger_source: HlsSegmentRepairTriggerSource::H264High,
                common_low_trigger,
                codec_medium_trigger,
            }
        }
        RepairVideoCodec::H264 if codec_medium_trigger => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Medium,
            trigger_source: HlsSegmentRepairTriggerSource::H264Medium,
            common_low_trigger,
            codec_medium_trigger,
        },
        RepairVideoCodec::Hevc
            if (common_low_trigger && (codec_medium_trigger || hevc_invalid_vcl_nalu(warnings)))
                || (codec_parameters_missing && codec_medium_trigger) =>
        {
            HlsSegmentRepairDecision {
                codec,
                required_level: HlsSegmentRepairMode::High,
                trigger_source: HlsSegmentRepairTriggerSource::HevcHigh,
                common_low_trigger,
                codec_medium_trigger,
            }
        }
        RepairVideoCodec::Hevc if codec_medium_trigger => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Medium,
            trigger_source: HlsSegmentRepairTriggerSource::HevcMedium,
            common_low_trigger,
            codec_medium_trigger,
        },
        RepairVideoCodec::H264 | RepairVideoCodec::Hevc if common_low_trigger => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Low,
            trigger_source: HlsSegmentRepairTriggerSource::CommonMpegTsLow,
            common_low_trigger,
            codec_medium_trigger,
        },
        RepairVideoCodec::Unsupported => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Off,
            trigger_source: HlsSegmentRepairTriggerSource::UnsupportedCodec,
            common_low_trigger,
            codec_medium_trigger,
        },
        _ => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Off,
            trigger_source: HlsSegmentRepairTriggerSource::Off,
            common_low_trigger,
            codec_medium_trigger,
        },
    }
}

pub(super) fn debug_repair_analysis(
    context: &HlsSegmentRepairObjectContext,
    configured_max_level: HlsSegmentRepairMode,
    decision: HlsSegmentRepairDecision,
    executed_level: Option<HlsSegmentRepairMode>,
    warnings: &WarningCounters,
) {
    let executed_level = executed_level.map_or("off", HlsSegmentRepairMode::as_log_value);
    match decision.codec {
        RepairVideoCodec::H264 => debug!(
            "HLS segment repair analysis completed: {} source={} resource={} configured_max_level={} required_level={} executed_level={} codec={} trigger={} common_low={} codec_medium={} missing_sps={} missing_pps={} sps_out_of_range={} pps_out_of_range={} no_frame={} decode_slice_header_error={} missing_picture={} invalid_nal={} no_start_code={} nal_split_error={} packet_corrupt={} continuity_check_failed={} codec_parameters_missing={}",
            context.log_identity_fields(),
            context.source.as_log_value(),
            context.resource_id,
            configured_max_level.as_log_value(),
            decision.required_level.as_log_value(),
            executed_level,
            decision.codec.as_log_value(),
            decision.trigger_source.as_log_value(),
            decision.common_low_trigger,
            decision.codec_medium_trigger,
            warnings.missing_sps,
            warnings.missing_pps,
            warnings.sps_out_of_range,
            warnings.pps_out_of_range,
            warnings.no_frame,
            warnings.decode_slice_header_error,
            warnings.missing_picture,
            warnings.invalid_nal,
            warnings.no_start_code,
            warnings.nal_split_error,
            warnings.packet_corrupt,
            warnings.continuity_check_failed,
            warnings.codec_parameters_missing
        ),
        RepairVideoCodec::Hevc => debug!(
            "HLS segment repair analysis completed: {} source={} resource={} configured_max_level={} required_level={} executed_level={} codec={} trigger={} common_low={} codec_medium={} missing_vps={} missing_sps={} missing_pps={} vps_out_of_range={} sps_out_of_range={} pps_out_of_range={} pps_id_out_of_range={} invalid_vcl_nalu={} nal_parse_error={} invalid_nal={} no_start_code={} nal_split_error={} packet_corrupt={} continuity_check_failed={} codec_parameters_missing={}",
            context.log_identity_fields(),
            context.source.as_log_value(),
            context.resource_id,
            configured_max_level.as_log_value(),
            decision.required_level.as_log_value(),
            executed_level,
            decision.codec.as_log_value(),
            decision.trigger_source.as_log_value(),
            decision.common_low_trigger,
            decision.codec_medium_trigger,
            warnings.missing_vps,
            warnings.missing_sps,
            warnings.missing_pps,
            warnings.vps_out_of_range,
            warnings.sps_out_of_range,
            warnings.pps_out_of_range,
            warnings.pps_id_out_of_range,
            warning_count(warnings, HlsSegmentRepairWarningKind::InvalidVclNalu),
            warnings.nal_parse_error,
            warnings.invalid_nal,
            warnings.no_start_code,
            warnings.nal_split_error,
            warnings.packet_corrupt,
            warnings.continuity_check_failed,
            warnings.codec_parameters_missing
        ),
        RepairVideoCodec::Unsupported => debug!(
            "HLS segment repair analysis completed: {} source={} resource={} configured_max_level={} required_level={} executed_level={} codec={} trigger={}",
            context.log_identity_fields(),
            context.source.as_log_value(),
            context.resource_id,
            configured_max_level.as_log_value(),
            decision.required_level.as_log_value(),
            executed_level,
            decision.codec.as_log_value(),
            decision.trigger_source.as_log_value()
        ),
    }
}

pub(super) fn debug_repair_event(
    context: &HlsSegmentRepairObjectContext,
    mode: HlsSegmentRepairMode,
    event: &'static str,
    reason: Option<&str>,
) {
    if let Some(reason) = reason {
        debug!(
            "HLS segment repair {event}: {} source={} resource={} mode={} reason={}",
            context.log_identity_fields(),
            context.source.as_log_value(),
            context.resource_id,
            mode.as_log_value(),
            reason
        );
    } else {
        debug!(
            "HLS segment repair {event}: {} source={} resource={} mode={}",
            context.log_identity_fields(),
            context.source.as_log_value(),
            context.resource_id,
            mode.as_log_value()
        );
    }
}

pub(super) fn debug_repair_stream_dropped(context: &HlsSegmentRepairObjectContext, dropped: &RepairRemuxDroppedStream) {
    debug!(
        "HLS segment repair stream dropped: {} source={} resource={} stream={} reason={}",
        context.log_identity_fields(),
        context.source.as_log_value(),
        context.resource_id,
        dropped.index,
        dropped.reason
    );
}
