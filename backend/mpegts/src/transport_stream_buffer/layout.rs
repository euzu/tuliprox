use super::{
    clock::{decode_pcr_at_location, decode_timestamp_at_location, MAX_PTS_DTS, SYNC_BYTE, TS_PACKET_SIZE},
    pes::{inspect_hls_ts_packet, HlsFiniteTsLayoutError},
    HlsFiniteTsLayout, HlsFiniteTsPacketLayout, HlsPesHeaderAssembler, HlsTsTimestampFieldKind,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// Finds TS alignment by checking for 0x47 sync byte every 188 bytes
pub(super) fn find_ts_alignment(buf: &[u8]) -> Option<usize> {
    for offset in 0..TS_PACKET_SIZE {
        let mut valid = true;
        for i in 0..5 {
            if buf.get(offset + i * TS_PACKET_SIZE) != Some(&SYNC_BYTE) {
                valid = false;
                break;
            }
        }
        if valid {
            return Some(offset);
        }
    }
    None
}

pub(super) fn build_hls_finite_ts_layout(buffer: &[u8]) -> Result<HlsFiniteTsLayout, HlsFiniteTsLayoutError> {
    if buffer.is_empty() || !buffer.len().is_multiple_of(TS_PACKET_SIZE) {
        return Err(HlsFiniteTsLayoutError::InvalidAsset);
    }
    let packet_count = buffer.len() / TS_PACKET_SIZE;
    let mut packet_evidence = Vec::with_capacity(packet_count);
    let mut pcr_fields = Vec::new();
    let mut packet_pcr_indices = Vec::with_capacity(packet_count);
    let mut timestamp_fields = Vec::new();
    let mut assembler = HlsPesHeaderAssembler::new();

    for (packet_index, packet) in buffer.as_chunks::<TS_PACKET_SIZE>().0.iter().enumerate() {
        let packet_start =
            packet_index.checked_mul(TS_PACKET_SIZE).ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?;
        let evidence = inspect_hls_ts_packet(packet, packet_start)?;
        let pcr_field_index = evidence.pcr_field.map(|field| {
            let index = pcr_fields.len();
            pcr_fields.push(field);
            index
        });
        let completed = assembler.push_packet(packet, packet_start, evidence)?;
        timestamp_fields.extend(completed.fields.into_iter().flatten().map(|field| field.location));
        packet_evidence.push(evidence);
        packet_pcr_indices.push(pcr_field_index);
    }
    assembler.finish()?;
    timestamp_fields.sort_unstable_by_key(|field| field.byte_offsets[0]);

    let mut timestamp_field_counts = vec![0usize; packet_count];
    for field in &timestamp_fields {
        let mut previous_packet = None;
        for offset in field.byte_offsets {
            if offset >= buffer.len() {
                return Err(HlsFiniteTsLayoutError::InvalidTimestampLocation);
            }
            let packet_index = offset / TS_PACKET_SIZE;
            if previous_packet != Some(packet_index) {
                timestamp_field_counts[packet_index] = timestamp_field_counts[packet_index]
                    .checked_add(1)
                    .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
                previous_packet = Some(packet_index);
            }
        }
    }
    let mut timestamp_field_starts = Vec::with_capacity(packet_count.saturating_add(1));
    timestamp_field_starts.push(0usize);
    for count in &timestamp_field_counts {
        let next = timestamp_field_starts
            .last()
            .copied()
            .and_then(|start| start.checked_add(*count))
            .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
        timestamp_field_starts.push(next);
    }
    let timestamp_membership_count = timestamp_field_starts.last().copied().unwrap_or(0);
    let mut packet_timestamp_field_indices = vec![0usize; timestamp_membership_count];
    let mut packet_cursors = timestamp_field_starts[..packet_count].to_vec();
    for (field_index, field) in timestamp_fields.iter().enumerate() {
        let mut previous_packet = None;
        for offset in field.byte_offsets {
            let packet_index = offset / TS_PACKET_SIZE;
            if previous_packet == Some(packet_index) {
                continue;
            }
            let cursor = packet_cursors[packet_index];
            packet_timestamp_field_indices[cursor] = field_index;
            packet_cursors[packet_index] = cursor.saturating_add(1);
            previous_packet = Some(packet_index);
        }
    }
    let packets = packet_evidence
        .into_iter()
        .enumerate()
        .map(|(packet_index, evidence)| HlsFiniteTsPacketLayout {
            packet_start: packet_index.saturating_mul(TS_PACKET_SIZE),
            pid: evidence.pid,
            has_payload: evidence.has_payload(),
            timestamp_field_indices_start: timestamp_field_starts[packet_index],
            timestamp_field_indices_end: timestamp_field_starts[packet_index.saturating_add(1)],
            pcr_field_index: packet_pcr_indices[packet_index],
        })
        .collect::<Vec<_>>();
    Ok(HlsFiniteTsLayout {
        packets: Arc::from(packets),
        timestamp_fields: Arc::from(timestamp_fields),
        pcr_fields: Arc::from(pcr_fields),
        packet_timestamp_field_indices: Arc::from(packet_timestamp_field_indices),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsTsPresentationClockSource {
    Pts,
    PcrFallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlsTsPidPresentationTimeline {
    pub pid: u16,
    pub first_pts_90khz: u64,
    pub last_pts_90khz: u64,
    pub cadence_ticks_90khz: u64,
    pub end_exclusive_90khz: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsTsPresentationDuration {
    pub first_presentation_clock_90khz: u64,
    pub end_exclusive_clock_90khz: u64,
    pub duration_ticks_90khz: u64,
    pub timelines: Arc<[HlsTsPidPresentationTimeline]>,
    pub source: HlsTsPresentationClockSource,
}

fn stable_presentation_cadence(mut deltas: Vec<u64>) -> Option<u64> {
    if deltas.is_empty() {
        return None;
    }
    deltas.sort_unstable();
    let mut dominant_delta = deltas[0];
    let mut dominant_count = 1usize;
    let mut current_delta = deltas[0];
    let mut current_count = 1usize;
    for delta in deltas.iter().copied().skip(1) {
        if delta == current_delta {
            current_count = current_count.saturating_add(1);
        } else {
            if current_count > dominant_count {
                dominant_delta = current_delta;
                dominant_count = current_count;
            }
            current_delta = delta;
            current_count = 1;
        }
    }
    if current_count > dominant_count {
        dominant_delta = current_delta;
        dominant_count = current_count;
    }
    if dominant_count.saturating_mul(2) > deltas.len() {
        Some(dominant_delta)
    } else {
        deltas.get(deltas.len() / 2).copied()
    }
}

fn presentation_timeline_for_pid(
    pid: u16,
    timestamps: &[u64],
) -> Result<HlsTsPidPresentationTimeline, HlsFiniteTsLayoutError> {
    let Some(first_raw) = timestamps.first().copied() else {
        return Err(HlsFiniteTsLayoutError::PresentationCadenceUnavailable { pid });
    };
    let cycle = i128::from(MAX_PTS_DTS);
    let mut previous_raw = first_raw % MAX_PTS_DTS;
    let mut previous_unwrapped = i128::from(previous_raw);
    let mut first_unwrapped = previous_unwrapped;
    let mut last_unwrapped = previous_unwrapped;
    let mut seen_raw_timestamps = HashSet::with_capacity(timestamps.len());
    seen_raw_timestamps.insert(previous_raw);
    let mut deltas = Vec::with_capacity(timestamps.len().saturating_sub(1));

    for raw in timestamps.iter().copied().skip(1).map(|value| value % MAX_PTS_DTS) {
        if !seen_raw_timestamps.insert(raw) {
            continue;
        }
        let forward = raw.wrapping_add(MAX_PTS_DTS).wrapping_sub(previous_raw) % MAX_PTS_DTS;
        let signed_delta = if forward <= MAX_PTS_DTS / 2 {
            i128::from(forward)
        } else {
            -i128::from(MAX_PTS_DTS.saturating_sub(forward))
        };
        let unwrapped =
            previous_unwrapped.checked_add(signed_delta).ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
        if unwrapped > previous_unwrapped {
            deltas.push(
                u64::try_from(unwrapped - previous_unwrapped)
                    .map_err(|_| HlsFiniteTsLayoutError::PresentationDurationOverflow)?,
            );
        }
        first_unwrapped = first_unwrapped.min(unwrapped);
        last_unwrapped = last_unwrapped.max(unwrapped);
        previous_raw = raw;
        previous_unwrapped = unwrapped;
    }
    let cadence_ticks_90khz = stable_presentation_cadence(deltas)
        .filter(|cadence| *cadence > 0 && *cadence < MAX_PTS_DTS / 2)
        .ok_or(HlsFiniteTsLayoutError::PresentationCadenceUnavailable { pid })?;
    let normalization = if first_unwrapped < 0 {
        (-first_unwrapped)
            .checked_add(cycle.saturating_sub(1))
            .and_then(|value| value.checked_div(cycle))
            .and_then(|cycles| cycles.checked_mul(cycle))
            .ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?
    } else {
        0
    };
    let first_pts_90khz = u64::try_from(
        first_unwrapped.checked_add(normalization).ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?,
    )
    .map_err(|_| HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
    let last_pts_90khz = u64::try_from(
        last_unwrapped.checked_add(normalization).ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?,
    )
    .map_err(|_| HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
    let end_exclusive_90khz =
        last_pts_90khz.checked_add(cadence_ticks_90khz).ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
    Ok(HlsTsPidPresentationTimeline { pid, first_pts_90khz, last_pts_90khz, cadence_ticks_90khz, end_exclusive_90khz })
}

fn presentation_duration_from_pid_clocks(
    clocks: HashMap<u16, Vec<u64>>,
    source: HlsTsPresentationClockSource,
) -> Result<HlsTsPresentationDuration, HlsFiniteTsLayoutError> {
    if clocks.is_empty() {
        return Err(HlsFiniteTsLayoutError::PresentationClockUnavailable);
    }
    let mut timelines = clocks
        .into_iter()
        .map(|(pid, timestamps)| presentation_timeline_for_pid(pid, &timestamps))
        .collect::<Result<Vec<_>, _>>()?;
    timelines.sort_unstable_by_key(|timeline| timeline.pid);
    let first_presentation_clock_90khz = timelines
        .iter()
        .map(|timeline| timeline.first_pts_90khz)
        .min()
        .ok_or(HlsFiniteTsLayoutError::PresentationClockUnavailable)?;
    let end_exclusive_clock_90khz = timelines
        .iter()
        .map(|timeline| timeline.end_exclusive_90khz)
        .max()
        .ok_or(HlsFiniteTsLayoutError::PresentationClockUnavailable)?;
    let duration_ticks_90khz = end_exclusive_clock_90khz
        .checked_sub(first_presentation_clock_90khz)
        .filter(|duration| *duration > 0 && *duration < MAX_PTS_DTS)
        .ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
    Ok(HlsTsPresentationDuration {
        first_presentation_clock_90khz,
        end_exclusive_clock_90khz,
        duration_ticks_90khz,
        timelines: Arc::from(timelines),
        source,
    })
}

pub(super) fn finite_hls_presentation_duration(
    buffer: &[u8],
    layout: &HlsFiniteTsLayout,
) -> Result<HlsTsPresentationDuration, HlsFiniteTsLayoutError> {
    let mut pts_by_pid = HashMap::<u16, Vec<u64>>::new();
    for location in
        layout.timestamp_fields.iter().copied().filter(|location| location.kind == HlsTsTimestampFieldKind::Pts)
    {
        pts_by_pid.entry(location.pid).or_default().push(decode_timestamp_at_location(buffer, location)?);
    }
    if !pts_by_pid.is_empty() {
        return presentation_duration_from_pid_clocks(pts_by_pid, HlsTsPresentationClockSource::Pts);
    }

    let mut pcr_by_pid = HashMap::<u16, Vec<u64>>::new();
    for location in layout.pcr_fields.iter().copied() {
        pcr_by_pid.entry(location.pid).or_default().push(decode_pcr_at_location(buffer, location)? / 300);
    }
    presentation_duration_from_pid_clocks(pcr_by_pid, HlsTsPresentationClockSource::PcrFallback)
}
