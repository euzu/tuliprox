use crate::{api::api_utils::tests::group_playback_support::GroupPlaybackEnv, model::Epg};
use shared::{
    model::{EpgChannel, EpgProgramme},
    utils::Internable,
};
use std::sync::Arc;

fn test_epg(channel_id: &str, title: &str, programme_title: &str) -> Epg {
    let mut programme = EpgProgramme::new(3600, 7200, channel_id.intern());
    programme.title = Some(programme_title.intern());
    Epg {
        priority: 0,
        logo_override: false,
        attributes: None,
        children: vec![Arc::new(EpgChannel {
            id: channel_id.intern(),
            title: Some(title.intern()),
            icon: None,
            programmes: vec![programme],
        })],
    }
}

#[tokio::test]
async fn failover_groups_outputs_and_epg_sanity() {
    let epg = test_epg("zdf.de", "ZDF", "Heute Journal");
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        .with_member_archives(&[Some(3), Some(7)])
        .with_epg(epg, "zdf.de")
        .with_hdhomerun()
        .build()
        .await;

    let member_solis = env.member_vid("solis");
    let member_korath = env.member_vid("korath");
    let group_vid = env.group_vid;

    // 1. Xtream get_live_streams lists the group vid, no member vid, and tv_archive/tv_archive_duration equals member maximum
    let res = env.get_xtream_live_streams().await;
    assert_eq!(res.status(), 200);
    let body_text = GroupPlaybackEnv::response_text(res).await;
    let streams: serde_json::Value = serde_json::from_str(&body_text).expect("valid JSON array");
    let list = streams.as_array().expect("array of streams");
    assert_eq!(list.len(), 1, "Only group stream should be listed: {body_text}");
    assert_eq!(list[0]["stream_id"], group_vid);
    assert_eq!(list[0]["name"], "ZDF HD");
    assert_eq!(list[0]["epg_channel_id"], "zdf.de");
    assert_eq!(list[0]["tv_archive"], 1);
    assert_eq!(list[0]["tv_archive_duration"], 7); // maximum of 3 and 7
    assert!(!body_text.contains(&member_solis.to_string()), "must not contain member solis vid");
    assert!(!body_text.contains(&member_korath.to_string()), "must not contain member korath vid");

    // 2. M3U output lists one entry with the group EPG id
    let res = env.get_m3u_playlist().await;
    assert_eq!(res.status(), 200);
    let m3u_text = GroupPlaybackEnv::response_text(res).await;
    assert_eq!(m3u_text.matches("#EXTINF:").count(), 1, "M3U must contain exactly one stream: {m3u_text}");
    assert!(m3u_text.contains(r#"tvg-id="zdf.de""#), "M3U must have group EPG id: {m3u_text}");
    assert!(m3u_text.contains(&format!("/{group_vid}")), "M3U stream URL must reference group vid: {m3u_text}");
    assert!(!m3u_text.contains(&format!("/{member_solis}")), "M3U must not contain member solis vid");
    assert!(!m3u_text.contains(&format!("/{member_korath}")), "M3U must not contain member korath vid");

    // 3. HDHomeRun lineup lists one entry
    let res = env.get_hdhomerun_lineup().await;
    assert_eq!(res.status(), 200);
    let lineup_text = GroupPlaybackEnv::response_text(res).await;
    let lineup: serde_json::Value = serde_json::from_str(&lineup_text).expect("valid JSON array");
    let lineup_list = lineup.as_array().expect("array of lineup items");
    assert_eq!(lineup_list.len(), 1, "Lineup must contain exactly one item: {lineup_text}");
    assert_eq!(lineup_list[0]["GuideNumber"], "zdf.de");
    assert_eq!(lineup_list[0]["GuideName"], "ZDF HD");
    let lineup_url = lineup_list[0]["URL"].as_str().unwrap_or("");
    assert!(
        lineup_url.contains(&group_vid.to_string()),
        "Lineup URL must reference group vid: {lineup_url} vs group {group_vid}"
    );
    assert!(
        !lineup_url.ends_with(&format!("/{member_solis}")) && !lineup_url.ends_with(&format!("/{member_solis}.ts")),
        "Lineup must not end with member solis vid: {lineup_url} vs solis {member_solis}"
    );
    assert!(
        !lineup_url.ends_with(&format!("/{member_korath}")) && !lineup_url.ends_with(&format!("/{member_korath}.ts")),
        "Lineup must not end with member korath vid: {lineup_url} vs korath {member_korath}"
    );

    // 4. XMLTV contains the group EPG channel
    let res = env.get_xmltv_epg().await;
    assert_eq!(res.status(), 200);
    let xmltv_text = GroupPlaybackEnv::response_text(res).await;
    assert!(xmltv_text.contains(r#"<channel id="zdf.de">"#), "XMLTV must contain group EPG channel: {xmltv_text}");
    assert!(
        xmltv_text.contains("<display-name>ZDF HD</display-name>"),
        "XMLTV must contain channel display name: {xmltv_text}"
    );
    assert!(
        xmltv_text.contains(r#"channel="zdf.de""#),
        "XMLTV must have programme referencing group EPG channel: {xmltv_text}"
    );
    assert!(xmltv_text.contains("<title>Heute Journal</title>"), "XMLTV must contain programme title: {xmltv_text}");
}
