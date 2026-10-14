use super::*;
use crate::{
    prepared_terminal_bundle::{HlsPreparedTerminalBundleKey, HlsPreparedTerminalSegment},
    terminal_tail::HlsTerminalAssetIdentity,
};

fn bundle() -> HlsPreparedTerminalBundle {
    HlsPreparedTerminalBundle {
        key: HlsPreparedTerminalBundleKey {
            asset: HlsTerminalAssetIdentity { revision: 7, fingerprint: [7; 32] },
            target_duration_ms: 12_000,
            segment_count: 12,
        },
        source_asset_duration_ms: 10_027,
        source_asset_duration_ticks_90khz: 902_400,
        segments: (0..12)
            .map(|index| HlsPreparedTerminalSegment {
                index,
                timestamp_offset_ticks_90khz: u64::from(index) * 902_400,
                bytes: bytes::Bytes::from_static(b"prepared terminal segment"),
            })
            .collect(),
    }
}

#[test]
fn late_manifest_keeps_target_and_sequences_and_contains_only_custom_media() {
    let bundle = bundle();
    let manifest = render_late_manifest(131, 9, "/iptv", &HlsAccessLeaseId("lease".into()), &bundle)
        .unwrap_or_else(|_| panic!("valid late manifest"));
    assert!(manifest.contains("#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:131\n#EXT-X-DISCONTINUITY-SEQUENCE:9\n"));
    assert_eq!(manifest.matches("#EXTINF:10.027,").count(), 12);
    assert_eq!(manifest.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    assert!(manifest.contains("#EXT-X-KEY:METHOD=NONE\n"));
    assert!(manifest.contains("/iptv/cvs/hls/lease/0000000000000007/11.ts"));
    assert!(!manifest.contains("/hls/shared/live/"));
    assert!(manifest.ends_with("#EXT-X-ENDLIST\n"));
}

#[test]
fn late_manifest_rejects_sequence_overflow_instead_of_reusing_live_sequences() {
    let bundle = bundle();
    assert!(matches!(
        render_late_manifest(u64::MAX - 1, 0, "", &HlsAccessLeaseId("lease".into()), &bundle,),
        Err(HlsLateCustomResponseError::Unavailable(HlsTerminalTailCompatibility::ManifestRenderFailed))
    ));
}
