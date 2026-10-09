use super::*;

#[test]
fn provisioning_asset_uses_exact_per_pid_presentation_duration() {
    let buffer = TransportStreamBuffer::new(
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/panel_api_provisioning.ts"))
            .to_vec(),
    );

    assert_eq!(buffer.duration_ticks_90khz(), Some(182_400));
    assert_eq!(buffer.duration_ms(), Some(2_027));
}

#[test]
fn per_pid_presentation_duration_unwraps_33_bit_audio_and_video_clocks() {
    let video_pid = 0x0101;
    let audio_pid = 0x0102;
    let packets = [
        build_pts_only_payload_packet(video_pid, 0, MAX_PTS_DTS - 1_080),
        build_pts_only_payload_packet(audio_pid, 0, MAX_PTS_DTS - 1_000),
        build_pts_only_payload_packet(video_pid, 1, 1_920),
        build_pts_only_payload_packet(audio_pid, 1, 920),
        build_pts_only_payload_packet(video_pid, 2, 4_920),
        build_pts_only_payload_packet(audio_pid, 2, 2_840),
    ];
    let raw = packets.into_iter().flatten().collect::<Vec<_>>();
    let buffer = TransportStreamBuffer::new(raw);
    let duration = buffer.finite_hls_presentation_duration.as_ref().expect("per-PID wrapped duration");

    assert_eq!(duration.source, HlsTsPresentationClockSource::Pts);
    assert_eq!(duration.first_presentation_clock_90khz, MAX_PTS_DTS - 1_080);
    assert_eq!(duration.end_exclusive_clock_90khz, MAX_PTS_DTS + 7_920);
    assert_eq!(duration.duration_ticks_90khz, 9_000);
    assert_eq!(
        duration.timelines.as_ref(),
        [
            HlsTsPidPresentationTimeline {
                pid: video_pid,
                first_pts_90khz: MAX_PTS_DTS - 1_080,
                last_pts_90khz: MAX_PTS_DTS + 4_920,
                cadence_ticks_90khz: 3_000,
                end_exclusive_90khz: MAX_PTS_DTS + 7_920,
            },
            HlsTsPidPresentationTimeline {
                pid: audio_pid,
                first_pts_90khz: MAX_PTS_DTS - 1_000,
                last_pts_90khz: MAX_PTS_DTS + 2_840,
                cadence_ticks_90khz: 1_920,
                end_exclusive_90khz: MAX_PTS_DTS + 4_760,
            },
        ]
    );
}
