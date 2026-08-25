//! Value types describing queued metadata-update work.
//!
//! Plain data - an identifier, a reason set, a task description. They are named
//! by the manager that queues them and by the processing code that produces
//! them, so they belong below both rather than inside `api`.

use shared::create_bitset;
use shared::model::PlaylistItemType;
use std::sync::Arc;

create_bitset!(u8, ResolveReason, Info, Tmdb, Date, Probe, MissingDetails);

/// `PlaylistItemIdType` ID can be either a String (M3U) or u32 (Xtream/TargetDB)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProviderIdType {
    Text(Arc<str>),
    Id(u32),
}

impl std::fmt::Display for ProviderIdType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderIdType::Text(s) => write!(f, "{s}"),
            ProviderIdType::Id(id) => write!(f, "{id}"),
        }
    }
}

impl From<u32> for ProviderIdType {
    fn from(id: u32) -> Self { ProviderIdType::Id(id) }
}

impl From<&str> for ProviderIdType {
    fn from(s: &str) -> Self { ProviderIdType::Text(Arc::from(s)) }
}

impl From<String> for ProviderIdType {
    fn from(s: String) -> Self { ProviderIdType::Text(Arc::from(s.as_str())) }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UpdateTask {
    ResolveVod {
        id: ProviderIdType,
        reason: ResolveReasonSet,
        delay: u16,
        source_last_modified: Option<u64>,
    },
    ResolveSeries {
        id: ProviderIdType,
        reason: ResolveReasonSet,
        delay: u16,
        source_last_modified: Option<u64>,
    },
    ProbeLive {
        id: ProviderIdType,
        reason: ResolveReasonSet,
        delay: u16,
        interval: u64,
    },
    // Generic probe for M3U/Library/etc.
    ProbeStream {
        probe_scope: Arc<str>,
        unique_id: String,
        url: String,
        item_type: PlaylistItemType,
        reason: ResolveReasonSet,
        delay: u16,
    },
}
