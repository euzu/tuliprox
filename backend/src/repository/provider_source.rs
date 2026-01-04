use crate::repository::bplustree::BPlusTreeQuery;
use crate::utils::FileReadGuard;
use shared::model::{M3uPlaylistItem, PlaylistGroup, XtreamPlaylistItem};

pub enum ProviderPlaylistSource {
    Memory(Box<Vec<PlaylistGroup>>),
    XtreamDisk {
        live: Box<Option<BPlusTreeQuery<u32, XtreamPlaylistItem>>>,
        vod: Box<Option<BPlusTreeQuery<u32, XtreamPlaylistItem>>>,
        series: Box<Option<BPlusTreeQuery<u32, XtreamPlaylistItem>>>,
        guards: Vec<FileReadGuard>,
    },
    M3uDisk {
        query: BPlusTreeQuery<u32, M3uPlaylistItem>,
        guard: FileReadGuard,
    },
}

// Debug manual impl because BPlusTreeQuery might not be Debug
impl std::fmt::Debug for ProviderPlaylistSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory(arg0) => f.debug_tuple("Memory").field(arg0).finish(),
            Self::XtreamDisk { live, vod, series, .. } => f
                .debug_struct("XtreamDisk")
                .field("live", &live.is_some())
                .field("vod", &vod.is_some())
                .field("series", &series.is_some())
                .finish(),
            Self::M3uDisk { .. } => f.debug_struct("M3uDisk").finish(),
        }
    }
}
