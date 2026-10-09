use super::*;

#[test]
fn epg_priority_merge_filter_accepts_later_source_for_already_processed_channel() {
    run_async_test(async move {
        let dir = tempdir().unwrap();
        let high_path = dir.path().join("high.xml");
        let low_path = dir.path().join("low.xml");

        fs::write(
            &high_path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>High Channel</display-name>
  </channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="demo.channel">
    <title>High Source</title>
  </programme>
</tv>"#,
        )
        .unwrap();
        fs::write(
            &low_path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Low Channel</display-name>
  </channel>
  <programme start="20260425010000 +0000" stop="20260425020000 +0000" channel="demo.channel">
    <title>Low Source</title>
  </programme>
</tv>"#,
        )
        .unwrap();

        let tv_guide = TVGuide::new(vec![xmltv_source(high_path, 0, false), xmltv_source(low_path, 10, false)]);
        let mut id_cache = EpgIdCache::new(None);
        id_cache.insert_channel_epg_id("demo.channel");

        let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

        assert!(id_cache.contains_processed_epg_id("demo.channel"));
        assert_eq!(merged.children.len(), 1);
        assert_eq!(merged.children[0].title.as_deref(), Some("High Channel"));
        assert_eq!(merged.children[0].programmes.len(), 2);
        assert_eq!(
            merged.children[0].programmes.iter().filter_map(|programme| programme.title.as_deref()).collect::<Vec<_>>(),
            vec!["High Source", "Low Source"],
        );
    });
}
