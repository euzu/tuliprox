//! `type: plugin` inputs and outputs against the built `provider-probe`
//! fixture. Skips, with a note, when the fixtures are not built (`bin/build_plugin_fixture.sh`).

mod common;

use common::{load_probe, probe_entry};
use shared::model::{HookCtx, PlaylistItemType, PluginHost, PluginInputRequest, PluginOutputRequest, XtreamCluster};
use std::path::Path;

fn request(options: &[(String, String)]) -> PluginInputRequest<'_> {
    PluginInputRequest { input_name: "feed", url: "http://host/feed", username: Some("u"), password: None, options }
}

async fn render(
    system: &impl PluginHost,
    plugin: &str,
    format: &str,
    options: &[(String, String)],
    path: &Path,
    items: &[shared::model::PlaylistItem],
) -> Result<(), String> {
    let refs: Vec<_> = items.iter().collect();
    let request = PluginOutputRequest { target: "target", format, options, path };
    system.render_output(plugin, &HookCtx::new("target").with_view("base"), &request, &refs).await
}

#[tokio::test]
async fn fetch_playlist_returns_the_default_five_deterministic_channels() {
    let Some(probe) = load_probe(vec![probe_entry("provider-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let first = probe.system.fetch_playlist("provider-probe", &ctx, &request(&[])).await.expect("fetch-playlist");
    let second = probe.system.fetch_playlist("provider-probe", &ctx, &request(&[])).await.expect("fetch-playlist");

    assert_eq!(first.len(), 5);
    assert_eq!(common::snapshot(&first), common::snapshot(&second), "the same call gives the same channels");
    let header = &first[1].header;
    assert_eq!(&*header.name, "ch-001");
    assert_eq!(&*header.group, "Group 1");
    assert_eq!(&*header.url, "http://provider.example/live/1.ts");
    assert_eq!(header.epg_channel_id.as_deref(), Some("epg.ch-001"));
    assert_eq!(header.item_type, PlaylistItemType::Live);
    assert_eq!(header.xtream_cluster, XtreamCluster::Live);
    // Host-issued: the input name is the request's, the ordinal counts from 1 in order returned.
    assert_eq!(&*header.input_name, "feed");
    assert_eq!(header.source_ordinal, 2);
    assert_eq!(&*header.id, "1001");
}

#[tokio::test]
async fn the_count_setting_sets_the_number_of_channels() {
    let Some(probe) =
        load_probe(vec![probe_entry("provider-probe", serde_json::json!({ "count": 3 }))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let items = probe.system.fetch_playlist("provider-probe", &ctx, &request(&[])).await.expect("fetch-playlist");
    assert_eq!(items.len(), 3);
}

#[tokio::test]
async fn fetch_epg_is_an_empty_ok_for_a_provider_without_epg() {
    let Some(probe) = load_probe(vec![probe_entry("provider-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let epg = probe.system.fetch_epg("provider-probe", &ctx, &request(&[])).await.expect("fetch-epg");
    assert!(epg.is_empty());
}

#[tokio::test]
async fn an_unknown_or_missing_plugin_is_an_err_not_a_panic() {
    let Some(probe) = load_probe(vec![probe_entry("provider-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let err = probe.system.fetch_playlist("no-such-plugin", &ctx, &request(&[])).await.unwrap_err();
    assert!(err.contains("does not provide an input type"), "{err}");

    // A plugin that runs other hooks only is no provider either.
    let err = probe.system.fetch_playlist("probe-full", &ctx, &request(&[])).await.unwrap_err();
    assert!(err.contains("does not provide an input type"), "{err}");
}

#[tokio::test]
async fn render_output_writes_header_lines_and_trailer_and_leaves_no_temp_file() {
    let Some(probe) = load_probe(vec![probe_entry("provider-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let dir = tempfile::tempdir().expect("temporary dir");
    let path = dir.path().join("list.txt");
    let items = common::items(3);

    render(
        &probe.system,
        "provider-probe",
        "probe-txt",
        &[("title".to_string(), "My list".to_string())],
        &path,
        &items,
    )
    .await
    .expect("render-output");

    let text = std::fs::read_to_string(&path).expect("the output file");
    assert_eq!(
        text,
        "# My list\nchannel 0|News|http://provider/live/0.ts\nchannel 1|News|http://provider/live/1.ts\n\
         channel 2|News|http://provider/live/2.ts\n# end 3\n"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(leftovers, ["list.txt"], "the temp file was renamed away");
}

#[tokio::test]
async fn render_output_is_byte_identical_for_the_same_items() {
    let Some(probe) = load_probe(vec![probe_entry("provider-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let dir = tempfile::tempdir().expect("temporary dir");
    let items = common::items(4);
    let mut contents = Vec::new();
    for name in ["one.txt", "two.txt"] {
        let path = dir.path().join(name);
        render(&probe.system, "provider-probe", "", &[], &path, &items).await.expect("render-output");
        contents.push(std::fs::read(&path).unwrap());
    }
    assert_eq!(contents[0], contents[1]);
}

#[tokio::test]
async fn a_configured_format_that_is_not_the_plugins_fails_the_output_and_keeps_the_old_file() {
    let Some(probe) = load_probe(vec![probe_entry("provider-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let dir = tempfile::tempdir().expect("temporary dir");
    let path = dir.path().join("list.txt");
    std::fs::write(&path, "previous run").unwrap();

    let err = render(&probe.system, "provider-probe", "other-format", &[], &path, &common::items(2)).await.unwrap_err();

    assert!(err.contains("probe-txt"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "previous run");
    assert!(!dir.path().join("list.txt.tmp").exists());
    assert_eq!(probe.system.output_format("provider-probe").as_deref(), Some("probe-txt"));
    assert_eq!(probe.system.output_format("probe-full"), None);
}
