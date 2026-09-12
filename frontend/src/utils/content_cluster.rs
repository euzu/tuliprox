use shared::model::XtreamCluster;

/// Update UI terminology and order only; never a wire or processing order.
pub(crate) struct ContentClusterPresentation {
    pub label_key: &'static str,
    pub rank: u8,
}

pub(crate) const fn content_cluster_presentation(cluster: XtreamCluster) -> ContentClusterPresentation {
    let (label_key, rank) = match cluster {
        XtreamCluster::Live => ("MESSAGES.PLAYLIST_UPDATE.CONTENT_LIVE", 0),
        XtreamCluster::Series => ("MESSAGES.PLAYLIST_UPDATE.CONTENT_SHOWS", 1),
        XtreamCluster::Video => ("MESSAGES.PLAYLIST_UPDATE.CONTENT_MOVIES", 2),
    };
    ContentClusterPresentation { label_key, rank }
}
