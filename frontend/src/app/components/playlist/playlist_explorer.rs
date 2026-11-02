pub use super::explorer::*;

#[cfg(test)]
mod tests {
    use super::{can_show_record_action, ChannelSelection};
    use shared::model::{VirtualId, XtreamCluster};

    #[test]
    fn record_action_covers_live_vod_and_episode_with_recording_write() {
        let live = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Live,
            downloadable: false,
            url: String::new(),
            title: "Live".to_string(),
            input_name: String::new(),
        };
        let vod = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Video,
            downloadable: true,
            url: String::new(),
            title: "VOD".to_string(),
            input_name: String::new(),
        };
        let series_container = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Series,
            downloadable: false,
            url: String::new(),
            title: "Series".to_string(),
            input_name: String::new(),
        };
        let episode = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Series,
            downloadable: true,
            url: String::new(),
            title: "Episode".to_string(),
            input_name: String::new(),
        };

        assert!(!can_show_record_action(false, Some(&live)));
        assert!(!can_show_record_action(false, Some(&vod)));
        assert!(can_show_record_action(true, Some(&live)));
        assert!(can_show_record_action(true, Some(&vod)));
        assert!(!can_show_record_action(true, Some(&series_container)));
        assert!(can_show_record_action(true, Some(&episode)));
    }
}
