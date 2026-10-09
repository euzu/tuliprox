use shared::model::{EpgChannel, EpgProgramme};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tuliprox_core::{
    model::{IcsDummyPolicy, IcsEpgSourceConfig, PersistedEpgSource},
    utils::FileLockManager,
};

struct IcsPersistedSource<'a> {
    channel_id: &'a Arc<str>,
    channel_title: Option<&'a Arc<str>>,
    match_names: &'a [Arc<str>],
    config: &'a IcsEpgSourceConfig,
}

/// A set of persisted EPG sources, read and merged into one guide.
///
/// The type lives beside the XMLTV parsing that gives it behaviour: its
/// `impl` needs `EpgIdCache` and the tag types from this module, so keeping the
/// struct in the configuration model would have split a type from its methods
/// across a crate boundary.
#[derive(Debug, Clone)]
pub struct TVGuide {
    epg_sources: Vec<PersistedEpgSource>,
    file_locks: Option<Arc<FileLockManager>>,
}

impl TVGuide {
    pub fn new(mut epg_sources: Vec<PersistedEpgSource>) -> Self {
        epg_sources.sort_by_key(|a| a.priority);
        Self { epg_sources, file_locks: None }
    }

    /// Uses the shared cache lock manager while reading persisted EPG sources.
    pub fn with_file_locks(mut self, file_locks: Arc<FileLockManager>) -> Self {
        self.file_locks = Some(file_locks);
        self
    }

    #[inline]
    pub fn get_epg_sources(&self) -> &Vec<PersistedEpgSource> { &self.epg_sources }

    pub fn get_file_locks(&self) -> Option<&FileLockManager> { self.file_locks.as_deref() }
}

#[derive(Debug)]
struct PreferredAttributes {
    priority: i16,
    source_order: usize,
    attributes: HashMap<Arc<str>, Arc<str>>,
}

#[derive(Debug)]
struct PreferredDummyPolicy {
    priority: i16,
    source_order: usize,
    policy: IcsDummyPolicy,
}

#[derive(Debug)]
struct ProgrammeMergeEntry {
    priority: i16,
    source_order: usize,
    programme: EpgProgramme,
}

#[derive(Debug)]
struct ChannelMergeAcc {
    priority: i16,
    source_order: usize,
    logo_override: bool,
    icon_logo_override: bool,
    needs_programme_merge: bool,
    channel: EpgChannel,
    programmes: Vec<ProgrammeMergeEntry>,
}

#[derive(Debug, Default)]
pub struct EpgMergeAccumulator {
    attributes: Option<PreferredAttributes>,
    channels: HashMap<Arc<str>, ChannelMergeAcc>,
    dummy_policies: HashMap<Arc<str>, PreferredDummyPolicy>,
}

/// Removes the wrapped file path on drop unless `take()` was called first.
/// Mirrors the cleanup logic of `DiskEpgSource::Drop` for the fallible window
/// inside `finish_into_disk` where no `DiskEpgSource` exists yet.
struct TempFileGuard(Option<PathBuf>);

#[cfg(test)]
mod tests;

mod calendar;
mod disk;
mod filter;
mod merge;
mod normalize;
mod xml;

pub use disk::{DiskEpgSource, EpgDiskChannelKey};
#[cfg(test)]
pub use merge::merge_epg_channels_by_priority;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use merge::{
    flatten_tvguide, merge_epg_channels_by_priority_with_dummy_policies, merge_epg_trees, EpgDummyPolicySource,
    MergedEpgWithIconOverrides,
};
pub use normalize::normalize_channel_name;
#[cfg(test)]
use normalize::strip_markers;
pub use xml::parse_tvguide;
