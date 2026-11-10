pub use super::explorer::*;

#[cfg(test)]
mod tests {
    use super::{can_show_failover_group_actions, can_show_record_action, failover_group_target, ChannelSelection};
    use shared::model::{
        ConfigSourceDto, ConfigTargetDto, ConfigTargetOptions, FailoverGroupsTargetConfig, PlaylistRequest,
        PluginChannelContextDto, SourcesConfigDto, VirtualId, XtreamCluster,
    };

    fn plugin_context() -> PluginChannelContextDto {
        PluginChannelContextDto {
            virtual_id: 0,
            name: String::new(),
            title: String::new(),
            group: String::new(),
            item_type: "live".to_string(),
            epg_channel_id: None,
            input_name: None,
        }
    }

    #[test]
    fn record_action_covers_live_vod_and_episode_with_recording_write() {
        let live = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Live,
            downloadable: false,
            url: String::new(),
            title: "Live".to_string(),
            input_name: String::new(),
            input_stream_id: String::new(),
            plugin_context: plugin_context(),
        };
        let vod = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Video,
            downloadable: true,
            url: String::new(),
            title: "VOD".to_string(),
            input_name: String::new(),
            input_stream_id: String::new(),
            plugin_context: plugin_context(),
        };
        let series_container = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Series,
            downloadable: false,
            url: String::new(),
            title: "Series".to_string(),
            input_name: String::new(),
            input_stream_id: String::new(),
            plugin_context: plugin_context(),
        };
        let episode = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Series,
            downloadable: true,
            url: String::new(),
            title: "Episode".to_string(),
            input_name: String::new(),
            input_stream_id: String::new(),
            plugin_context: plugin_context(),
        };

        assert!(!can_show_record_action(false, Some(&live)));
        assert!(!can_show_record_action(false, Some(&vod)));
        assert!(can_show_record_action(true, Some(&live)));
        assert!(can_show_record_action(true, Some(&vod)));
        assert!(!can_show_record_action(true, Some(&series_container)));
        assert!(can_show_record_action(true, Some(&episode)));
    }

    fn selection(cluster: XtreamCluster, input_stream_id: &str) -> ChannelSelection {
        ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster,
            downloadable: cluster == XtreamCluster::Video,
            url: String::new(),
            title: "Channel".to_string(),
            input_name: "input-a".to_string(),
            input_stream_id: input_stream_id.to_string(),
            plugin_context: plugin_context(),
        }
    }

    fn sources_with_failover(enabled: Option<bool>) -> SourcesConfigDto {
        let options = enabled.map(|enabled| ConfigTargetOptions {
            failover_groups: Some(FailoverGroupsTargetConfig { enabled, ..FailoverGroupsTargetConfig::default() }),
            ..ConfigTargetOptions::default()
        });
        SourcesConfigDto {
            sources: vec![ConfigSourceDto {
                inputs: vec!["input-a".to_string().into()],
                targets: vec![ConfigTargetDto { id: 7, name: "Main TV".to_string(), options, ..Default::default() }],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn explorer_failover_group_actions_only_for_live_target_items() {
        let live = selection(XtreamCluster::Live, "42");
        let vod = selection(XtreamCluster::Video, "42");
        let series = selection(XtreamCluster::Series, "42");
        let no_stream_id = selection(XtreamCluster::Live, "");

        assert!(can_show_failover_group_actions(Some(&live), Some("Main TV")));
        assert!(!can_show_failover_group_actions(Some(&live), None), "target without failover groups");
        assert!(!can_show_failover_group_actions(Some(&vod), Some("Main TV")));
        assert!(!can_show_failover_group_actions(Some(&series), Some("Main TV")));
        assert!(!can_show_failover_group_actions(Some(&no_stream_id), Some("Main TV")), "no channel reference");
        assert!(!can_show_failover_group_actions(None, Some("Main TV")));

        let enabled = sources_with_failover(Some(true));
        let disabled = sources_with_failover(Some(false));
        let unset = sources_with_failover(None);
        let target = PlaylistRequest::Target(7);
        let other_target = PlaylistRequest::Target(8);
        let input = PlaylistRequest::Input("input-a".to_string());

        assert_eq!(failover_group_target(Some(&target), Some(&enabled), "input-a").as_deref(), Some("Main TV"));
        assert!(failover_group_target(Some(&target), Some(&disabled), "input-a").is_none());
        assert!(failover_group_target(Some(&target), Some(&unset), "input-a").is_none());
        assert!(failover_group_target(Some(&other_target), Some(&enabled), "input-a").is_none());
        assert!(failover_group_target(Some(&input), Some(&enabled), "input-a").is_none(), "only target playlists");
        assert!(failover_group_target(Some(&target), None, "input-a").is_none());
        assert!(failover_group_target(None, Some(&enabled), "input-a").is_none());
    }
}
