use super::TVGuide;
use crate::parser::xmltv::{
    flatten_tvguide, merge_epg_channels_by_priority, merge_epg_channels_by_priority_with_dummy_policies,
    normalize_channel_name, EpgDummyPolicySource, EpgMergeAccumulator,
};
use shared::model::{EpgCategory, EpgChannel, EpgProgramme};
use std::{collections::HashSet, fs, path::PathBuf, sync::Arc};
use tempfile::tempdir;
use tuliprox_core::{
    model::{Epg, EpgSmartMatchConfig, IcsDummyConfig, IcsEpgSourceConfig, PersistedEpgSource, PersistedEpgSourceKind},
    utils::FileLockManager,
};

/// Run an async test body on a freshly-created multi-threaded tokio
/// runtime. Centralizes the `Runtime::new()...block_on(...)` boilerplate
/// shared by every test in this module that exercises async EPG code.
fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    tokio::runtime::Runtime::new().unwrap().block_on(future);
}

fn xmltv_source(file_path: PathBuf, priority: i16, logo_override: bool) -> PersistedEpgSource {
    PersistedEpgSource { file_path, priority, logo_override, kind: PersistedEpgSourceKind::Xmltv }
}

fn dummy_policy_source(priority: i16, source_order: usize, title: &str) -> EpgDummyPolicySource {
    EpgDummyPolicySource {
        priority,
        source_order,
        channel_id: "f1.calendar".intern(),
        policy: tuliprox_core::model::IcsDummyPolicy {
            timezone: "UTC".to_string(),
            config: IcsDummyConfig {
                enabled: true,
                title: title.to_string(),
                description: String::new(),
                days_past: 0,
                days_future: 0,
                block_hours: 24,
                min_gap_minutes: 1,
            },
        },
    }
}

fn epg_channel(id: &str, title: Option<&str>, icon: Option<&str>, programmes: Vec<EpgProgramme>) -> EpgChannel {
    EpgChannel { id: id.intern(), title: title.map(Internable::intern), icon: icon.map(Internable::intern), programmes }
}

fn epg_programme(id: &str, start: i64, stop: i64, title: Option<&str>, desc: Option<&str>) -> EpgProgramme {
    EpgProgramme::new_all(start, stop, id.intern(), title.map(Internable::intern), desc.map(Internable::intern), None)
}

use crate::processor::EpgIdCache;
use rphonetic::{Encoder, Metaphone};
use shared::{
    model::{EpgNamePrefix, EpgSmartMatchConfigDto},
    utils::Internable,
};

mod lifecycle;
mod playlist;
mod policy;
mod storage;
mod transport;
