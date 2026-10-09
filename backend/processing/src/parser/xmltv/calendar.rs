use super::{EpgMergeAccumulator, IcsPersistedSource, TVGuide};
use crate::processor::EpgIdCache;
use std::sync::Arc;
use tuliprox_core::model::{IcsDummyPolicy, PersistedEpgSource};
use tuliprox_parser::ics;

impl TVGuide {
    pub(super) async fn process_ics_file(
        id_cache: &mut EpgIdCache,
        epg_source: &PersistedEpgSource,
        source_order: usize,
        accumulator: &mut EpgMergeAccumulator,
        source: IcsPersistedSource<'_>,
    ) -> bool {
        let IcsPersistedSource { channel_id, channel_title, match_names, config } = source;
        let mut candidates = Vec::with_capacity(2 + match_names.len());
        candidates.push(channel_id.to_string());
        if let Some(title) = channel_title {
            candidates.push(title.to_string());
        }
        candidates.extend(match_names.iter().map(ToString::to_string));
        let normalized_candidates = id_cache.normalize_candidates(candidates);

        let add_channel = if id_cache.smart_match_enabled {
            let direct_match = id_cache.contains_channel_epg_id(channel_id);
            let normalized_match = id_cache.match_epg_channel_candidates(
                channel_id,
                &normalized_candidates,
                epg_source.priority,
                source_order,
            );
            direct_match || normalized_match
        } else {
            id_cache.contains_channel_epg_id(channel_id)
        };

        if !add_channel {
            return false;
        }

        match ics::parse_ics_file_to_channel(
            &epg_source.file_path,
            Arc::clone(channel_id),
            channel_title.cloned(),
            config,
        )
        .await
        {
            Ok(channel) => {
                id_cache.insert_processed_epg_id(channel_id);
                accumulator.add_channel_with_programmes(
                    epg_source.priority,
                    source_order,
                    epg_source.logo_override,
                    channel,
                );
                accumulator.register_dummy_policy(
                    channel_id,
                    epg_source.priority,
                    source_order,
                    IcsDummyPolicy { timezone: config.timezone.clone(), config: config.dummy.clone() },
                );
                true
            }
            Err(err) => {
                log::warn!("Failed to process ICS EPG file {}: {err}", epg_source.file_path.display());
                false
            }
        }
    }
}
