//! The `type: plugin` outputs of a target.
//!
//! `persist_playlist_views` lives in `tuliprox-repository`, which has no plugin
//! hook, so it treats `TargetOutput::Plugin` as a no-op and the render driver
//! runs here, right after it, over the same finalized base view.

use log::{debug, error};
use shared::{
    error::TuliproxError,
    model::{HookCtx, PlaylistGroup, PlaylistItem, PlaylistItemType, PluginHost, PluginOutputRequest},
};
use std::{path::Path, sync::Arc};
use tuliprox_core::model::{is_valid, AppConfig, ConfigTarget, PluginTargetOutput};
use tuliprox_repository::ensure_target_storage_path;

/// Render every `type: plugin` output of `target` from `playlist` (the base
/// view `persist_playlist_views` just persisted) and fold the failures into
/// `persisted`, the result of that persist.
///
/// Per output: the output filter is applied, then the plugin renders the items
/// into `<target dir>/<filename>` (`PluginHost::render_output`, temp file then
/// rename). One output failing is logged and collected, and the next one still
/// runs; so does a failure of the built-in outputs before it. An output whose
/// filtered view is empty is skipped, as the M3U writer skips an empty
/// playlist: the last good file stays.
///
/// Series info entries have no stream of their own and are not sent, as the M3U
/// output leaves them out too.
pub(crate) async fn persist_plugin_outputs<P: PluginHost>(
    plugins: &P,
    app_config: &Arc<AppConfig>,
    target: &ConfigTarget,
    playlist: &[PlaylistGroup],
    persisted: Result<(), Vec<TuliproxError>>,
) -> Result<(), Vec<TuliproxError>> {
    let mut outputs = target.plugin_outputs().peekable();
    if outputs.peek().is_none() {
        return persisted;
    }
    let mut errors = persisted.err().unwrap_or_default();
    let target_dir = {
        let config = app_config.config.load();
        ensure_target_storage_path(&config, &target.name).await
    };
    match target_dir {
        Ok(dir) => render_all(plugins, target, playlist, &dir, &mut errors).await,
        Err(err) => errors.push(err),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Every plugin output of `target` into `target_dir`, one failure never stopping the next.
async fn render_all<P: PluginHost>(
    plugins: &P,
    target: &ConfigTarget,
    playlist: &[PlaylistGroup],
    target_dir: &Path,
    errors: &mut Vec<TuliproxError>,
) {
    for output in target.plugin_outputs() {
        if let Err(err) = render_one(plugins, target, output, playlist, target_dir).await {
            error!("Target '{}': plugin output of {} failed: {err}", target.name, output.plugin);
            errors.push(TuliproxError::Plugin(format!(
                "target '{}': plugin output of {} failed: {err}",
                target.name, output.plugin
            )));
        }
    }
}

async fn render_one<P: PluginHost>(
    plugins: &P,
    target: &ConfigTarget,
    output: &PluginTargetOutput,
    playlist: &[PlaylistGroup],
    target_dir: &Path,
) -> Result<(), String> {
    let items = output_items(playlist, output);
    if items.is_empty() {
        debug!("Target '{}': nothing to render for the {} output; keeping its file", target.name, output.plugin);
        return Ok(());
    }
    let path = target_dir.join(output.file_name());
    let options = shared::model::plugin_options(&output.plugin_config);
    let request = PluginOutputRequest {
        target: &target.name,
        format: output.format.as_deref().unwrap_or_default(),
        options: &options,
        path: &path,
    };
    let ctx = HookCtx::new(&target.name).with_view("base");
    plugins.render_output(&output.plugin, &ctx, &request, &items).await
}

/// The items of `playlist` the output renders: those its filter lets through,
/// without series info entries, in playlist order.
fn output_items<'a>(playlist: &'a [PlaylistGroup], output: &PluginTargetOutput) -> Vec<&'a PlaylistItem> {
    playlist
        .iter()
        .flat_map(|group| &group.channels)
        .filter(|item| {
            !matches!(item.header.item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo)
        })
        .filter(|item| output.filter.as_ref().is_none_or(|filter| is_valid(item, filter, false)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use shared::{
        model::{ConfigTargetDto, PlaylistItemHeader, XtreamCluster},
        utils::Internable,
    };
    use std::{future::Future, path::PathBuf};

    /// One `render_output` call: plugin, path, item names, format.
    type Call = (String, PathBuf, Vec<String>, String);

    #[derive(Clone, Default)]
    struct Recorder {
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl PluginHost for Recorder {
        fn render_output(
            &self,
            plugin: &str,
            _ctx: &HookCtx<'_>,
            request: &PluginOutputRequest<'_>,
            items: &[&PlaylistItem],
        ) -> impl Future<Output = Result<(), String>> + Send {
            let names = items.iter().map(|item| item.header.name.to_string()).collect();
            let result = if plugin == "bad-plugin" { Err("render failed".to_string()) } else { Ok(()) };
            self.calls.lock().push((plugin.to_string(), request.path.to_path_buf(), names, request.format.to_string()));
            async move { result }
        }
    }

    fn item(name: &str, group: &str, item_type: PlaylistItemType) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                title: name.intern(),
                group: group.intern(),
                item_type,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn playlist() -> Vec<PlaylistGroup> {
        vec![
            PlaylistGroup {
                id: 1,
                title: "News".intern(),
                channels: vec![
                    item("a", "News", PlaylistItemType::Live),
                    item("series", "News", PlaylistItemType::SeriesInfo),
                ],
                xtream_cluster: XtreamCluster::Live,
            },
            PlaylistGroup {
                id: 2,
                title: "Sport".intern(),
                channels: vec![item("b", "Sport", PlaylistItemType::Live)],
                xtream_cluster: XtreamCluster::Live,
            },
        ]
    }

    fn target(outputs: &serde_json::Value) -> ConfigTarget {
        let mut dto: ConfigTargetDto =
            serde_json::from_value(serde_json::json!({"name": "t1", "output": outputs})).unwrap();
        dto.prepare(1, None, None).unwrap();
        ConfigTarget::from(&dto)
    }

    #[test]
    fn items_skip_series_info_and_follow_the_output_filter() {
        let all = target(&serde_json::json!([{"type": "plugin", "plugin": "my-plugin"}]));
        let output = all.plugin_outputs().next().unwrap();
        let names: Vec<_> = output_items(&playlist(), output).iter().map(|i| i.header.name.to_string()).collect();
        assert_eq!(names, ["a", "b"]);

        let filtered =
            target(&serde_json::json!([{"type": "plugin", "plugin": "my-plugin", "filter": "Group ~ \"Sport\""}]));
        let output = filtered.plugin_outputs().next().unwrap();
        let names: Vec<_> = output_items(&playlist(), output).iter().map(|i| i.header.name.to_string()).collect();
        assert_eq!(names, ["b"]);
    }

    #[tokio::test]
    async fn a_failing_output_is_collected_and_the_next_one_still_runs() {
        let dir = Path::new("/targets/t1");
        let target = target(&serde_json::json!([
            {"type": "plugin", "plugin": "bad-plugin", "filename": "bad.txt"},
            {"type": "plugin", "plugin": "good-plugin", "format": "my-format", "filename": "good.txt"},
        ]));
        let recorder = Recorder::default();
        let mut errors = vec![TuliproxError::Config("m3u write failed".to_string())];

        render_all(&recorder, &target, &playlist(), dir, &mut errors).await;

        assert_eq!(errors.len(), 2, "the earlier persist error stays, the bad output adds one");
        assert!(errors[1].to_string().contains("bad-plugin"), "{}", errors[1]);
        let calls = recorder.calls.lock();
        assert_eq!(calls.len(), 2, "the second output ran after the first failed");
        assert_eq!(calls[1].0, "good-plugin");
        assert_eq!(calls[1].1, dir.join("good.txt"));
        assert_eq!(calls[1].2, ["a", "b"]);
        assert_eq!(calls[1].3, "my-format");
    }

    #[tokio::test]
    async fn the_default_file_name_is_the_plugin_id() {
        let dir = Path::new("/targets/t1");
        let target = target(&serde_json::json!([{"type": "plugin", "plugin": "good-plugin"}]));
        let recorder = Recorder::default();
        let mut errors = Vec::new();

        render_all(&recorder, &target, &playlist(), dir, &mut errors).await;

        assert!(errors.is_empty());
        assert_eq!(recorder.calls.lock()[0].1, dir.join("good-plugin.out"));
    }

    #[tokio::test]
    async fn an_empty_view_keeps_the_last_file() {
        let target =
            target(&serde_json::json!([{"type": "plugin", "plugin": "good-plugin", "filter": "Group ~ \"Nothing\""}]));
        let recorder = Recorder::default();
        let mut errors = Vec::new();

        render_all(&recorder, &target, &playlist(), Path::new("/targets/t1"), &mut errors).await;

        assert!(errors.is_empty());
        assert!(recorder.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn a_target_without_plugin_outputs_renders_nothing() {
        let target = target(&serde_json::json!([{"type": "m3u"}]));
        let recorder = Recorder::default();
        let mut errors = Vec::new();
        render_all(&recorder, &target, &playlist(), Path::new("/targets/t1"), &mut errors).await;
        assert!(errors.is_empty());
        assert!(recorder.calls.lock().is_empty());
    }
}
