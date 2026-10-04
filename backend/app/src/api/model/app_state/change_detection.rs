use super::AppState;
use crate::model::{Config, ConfigProvider, ConfigTarget, HdHomeRunConfig, ScheduleConfig, SourcesConfig};
use shared::{create_bitset, model::RecordingConfigDto, utils::small_vecs_equal_unordered};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum TargetStatus {
    Old,
    New,
    Keep,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum TargetCacheState {
    UnchangedFalse,
    UnchangedTrue,
    ChangedToTrue,
    ChangedToFalse,
}

pub(super) struct TargetChanges {
    pub(super) name: String,
    pub(super) status: TargetStatus,
    pub(super) cache_status: TargetCacheState,
    pub(super) target: Arc<ConfigTarget>,
}

create_bitset!(
    u8,
    UpdateChangesFlags,
    Scheduler,
    Hdhomerun,
    FileWatch,
    Geoip,
    ProviderDns,
    Metadata,
    QosAggregation,
    Downloads
);

pub(in crate::api) struct UpdateChanges {
    pub(super) flags: UpdateChangesFlagsSet,
    pub(super) targets: Option<HashMap<String, TargetChanges>>,
}

impl UpdateChanges {
    pub(in crate::api) fn modified(&self) -> bool { !self.flags.is_empty() }

    fn set_flag_if(&mut self, condition: bool, flag: UpdateChangesFlags) {
        if condition {
            self.flags.set(flag);
        }
    }
}

macro_rules! change_detect {
    ($fn_name:ident, $a:expr, $b: expr) => {
        match ($a, $b) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(o), Some(n)) => $fn_name(o, n),
        }
    };
}

pub(super) fn recording_changed(a: &crate::model::RecordingConfig, b: &crate::model::RecordingConfig) -> bool {
    RecordingConfigDto::from(a) != RecordingConfigDto::from(b)
}

impl AppState {
    pub(super) fn detect_changes_for_config(&self, config: &Config) -> UpdateChanges {
        let old_config = self.app_config.config.load();
        let changed_schedules =
            change_detect!(schedules_changed, old_config.schedules.as_ref(), config.schedules.as_ref());
        let library_enabled = config.library.as_ref().is_some_and(|library| library.enabled);
        let old_library_enabled = old_config.library.as_ref().is_some_and(|library| library.enabled);
        let changed_library_enabled = library_enabled != old_library_enabled;
        let changed_hdhomerun =
            change_detect!(hdhomerun_changed, old_config.hdhomerun.as_ref(), config.hdhomerun.as_ref());
        let changed_file_watch =
            change_detect!(string_changed, old_config.mapping_path.as_ref(), config.mapping_path.as_ref())
                || change_detect!(string_changed, old_config.template_path.as_ref(), config.template_path.as_ref());

        let geoip_enabled = config.is_geoip_enabled();
        let geoip_enabled_old = old_config.is_geoip_enabled();
        let changed_storage_dir = old_config.storage_dir != config.storage_dir;
        let changed_qos_aggregation = qos_aggregation_changed(&old_config, config);
        let changed_recording = change_detect!(
            recording_changed,
            old_config.video.as_ref().and_then(|video| video.recording.as_ref()),
            config.video.as_ref().and_then(|video| video.recording.as_ref())
        );

        let mut changes = UpdateChanges { flags: UpdateChangesFlagsSet::new(), targets: None };
        changes.set_flag_if(
            changed_schedules || changed_library_enabled || geoip_enabled != geoip_enabled_old,
            UpdateChangesFlags::Scheduler,
        );
        changes.set_flag_if(changed_hdhomerun, UpdateChangesFlags::Hdhomerun);
        changes.set_flag_if(changed_file_watch, UpdateChangesFlags::FileWatch);
        changes.set_flag_if(geoip_enabled != geoip_enabled_old, UpdateChangesFlags::Geoip);
        changes.set_flag_if(changed_storage_dir, UpdateChangesFlags::Metadata);
        changes.set_flag_if(changed_qos_aggregation || changed_storage_dir, UpdateChangesFlags::QosAggregation);
        changes.set_flag_if(changed_recording, UpdateChangesFlags::Downloads);
        changes
    }

    pub(super) fn detect_changes_for_sources(&self, sources: &SourcesConfig) -> UpdateChanges {
        let (file_watch_changed, provider_dns_changed, target_changes) = {
            let old_sources = self.app_config.sources.load();
            let file_watch_changed = old_sources.get_input_files() != sources.get_input_files();
            let provider_dns_changed = providers_changed(&old_sources.provider, &sources.provider);

            let mut target_changes = HashMap::new();
            for source in &old_sources.sources {
                for target in &source.targets {
                    target_changes.insert(
                        target.name.clone(),
                        TargetChanges {
                            name: target.name.clone(),
                            status: TargetStatus::Old,
                            cache_status: if target.use_memory_cache {
                                TargetCacheState::UnchangedTrue
                            } else {
                                TargetCacheState::UnchangedFalse
                            },
                            target: Arc::clone(target),
                        },
                    );
                }
            }
            for source in &sources.sources {
                for target in &source.targets {
                    match target_changes.get_mut(&target.name) {
                        None => {
                            target_changes.insert(
                                target.name.clone(),
                                TargetChanges {
                                    name: target.name.clone(),
                                    status: TargetStatus::New,
                                    cache_status: if target.use_memory_cache {
                                        TargetCacheState::ChangedToTrue
                                    } else {
                                        TargetCacheState::ChangedToFalse
                                    },
                                    target: Arc::clone(target),
                                },
                            );
                        }
                        Some(changes) => {
                            changes.status = TargetStatus::Keep;
                            changes.cache_status = match (changes.cache_status, target.use_memory_cache) {
                                (TargetCacheState::UnchangedFalse, true) => TargetCacheState::ChangedToTrue,
                                (TargetCacheState::UnchangedTrue, false) => TargetCacheState::ChangedToFalse,
                                (x, _) => x,
                            };
                        }
                    }
                }
            }

            (file_watch_changed, provider_dns_changed, target_changes)
        };

        let mut changes = UpdateChanges { flags: UpdateChangesFlagsSet::new(), targets: Some(target_changes) };
        changes.set_flag_if(file_watch_changed, UpdateChangesFlags::FileWatch);
        changes.set_flag_if(provider_dns_changed, UpdateChangesFlags::ProviderDns);
        changes
    }
}

pub(super) fn schedules_changed(a: &[ScheduleConfig], b: &[ScheduleConfig]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    let mut used = vec![false; b.len()];

    for schedule in a {
        let Some(found_idx) = b.iter().enumerate().find_map(|(idx, candidate)| {
            if used[idx] || candidate.schedule != schedule.schedule || candidate.task_type != schedule.task_type {
                return None;
            }
            let targets_match = match (schedule.targets.as_ref(), candidate.targets.as_ref()) {
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
                (Some(a_targets), Some(b_targets)) => small_vecs_equal_unordered(a_targets, b_targets),
            };
            if targets_match {
                Some(idx)
            } else {
                None
            }
        }) else {
            return true;
        };
        used[found_idx] = true;
    }
    false
}

pub(super) fn hdhomerun_changed(a: &HdHomeRunConfig, b: &HdHomeRunConfig) -> bool {
    a.flags != b.flags || !small_vecs_equal_unordered(a.devices.as_ref(), b.devices.as_ref())
}

pub(super) fn string_changed(a: &str, b: &str) -> bool { a != b }

pub(super) fn providers_changed(a: &[Arc<ConfigProvider>], b: &[Arc<ConfigProvider>]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    for lhs in a {
        let Some(rhs) = b.iter().find(|candidate| candidate.name == lhs.name) else {
            return true;
        };
        if lhs.urls != rhs.urls || lhs.dns != rhs.dns {
            return true;
        }
    }
    false
}

fn stream_history_tuple(cfg: Option<&crate::model::StreamHistoryConfig>) -> Option<(bool, &str, u16, usize)> {
    cfg.map(|history| {
        (
            history.stream_history_enabled,
            history.stream_history_directory.as_str(),
            history.stream_history_retention_days,
            history.stream_history_batch_size,
        )
    })
}

fn qos_tuple(cfg: Option<&crate::model::QosAggregationConfig>) -> Option<(bool, u64, u64)> {
    cfg.map(|qos| (qos.enabled, qos.interval_secs, qos.compaction_interval_secs))
}

pub(super) fn qos_aggregation_changed(old_config: &Config, new_config: &Config) -> bool {
    let old_reverse_proxy = old_config.reverse_proxy.as_ref();
    let new_reverse_proxy = new_config.reverse_proxy.as_ref();

    let old_stream_history = old_reverse_proxy.and_then(|rp| rp.stream_history.as_ref());
    let new_stream_history = new_reverse_proxy.and_then(|rp| rp.stream_history.as_ref());
    let old_qos = old_reverse_proxy.and_then(|rp| rp.qos_aggregation.as_ref());
    let new_qos = new_reverse_proxy.and_then(|rp| rp.qos_aggregation.as_ref());

    stream_history_tuple(old_stream_history) != stream_history_tuple(new_stream_history)
        || qos_tuple(old_qos) != qos_tuple(new_qos)
}
