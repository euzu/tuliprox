use super::{EpgMergeAccumulator, IcsPersistedSource, MergedEpgWithIconOverrides, TVGuide};
use crate::processor::EpgIdCache;
#[cfg(test)]
use tuliprox_core::model::Epg;
use tuliprox_core::model::PersistedEpgSourceKind;

impl TVGuide {
    // Exercised only by this module's own tests.
    #[cfg(test)]
    pub async fn filter(&self, id_cache: &mut EpgIdCache) -> Option<Vec<Epg>> {
        self.filter_merged(id_cache).await.map(|epg| vec![epg])
    }

    // Exercised only by this module's own tests.
    #[cfg(test)]
    pub async fn filter_merged(&self, id_cache: &mut EpgIdCache) -> Option<Epg> {
        self.filter_merged_with_icon_overrides(id_cache).await.map(|(epg, _)| epg)
    }

    pub async fn filter_merged_with_icon_overrides(
        &self,
        id_cache: &mut EpgIdCache,
    ) -> Option<MergedEpgWithIconOverrides> {
        if id_cache.channel_epg_id.is_empty() && id_cache.normalized.is_empty() {
            return None;
        }
        let mut accumulator = EpgMergeAccumulator::new();
        for (source_order, epg_source) in self.get_epg_sources().iter().enumerate() {
            let _source_read_lock = match self.get_file_locks() {
                Some(file_locks) => Some(file_locks.read_lock(&epg_source.file_path).await),
                None => None,
            };
            match &epg_source.kind {
                PersistedEpgSourceKind::Xmltv => {
                    Self::process_epg_file(id_cache, epg_source, source_order, &mut accumulator).await;
                }
                PersistedEpgSourceKind::Ics { channel_id, channel_title, match_names, config } => {
                    Self::process_ics_file(
                        id_cache,
                        epg_source,
                        source_order,
                        &mut accumulator,
                        IcsPersistedSource {
                            channel_id,
                            channel_title: channel_title.as_ref(),
                            match_names,
                            config: config.as_ref(),
                        },
                    )
                    .await;
                }
            }
        }
        let available_epg_ids = accumulator.channel_ids_with_programmes();
        id_cache.finalize_matches(&available_epg_ids);
        let mut selected_epg_ids = id_cache.selected_epg_ids(&available_epg_ids);
        selected_epg_ids.extend(id_cache.channel_epg_id.iter().cloned());
        let retained_epg_ids = accumulator.retain_channels(&selected_epg_ids);
        id_cache.replace_processed_epg_ids(retained_epg_ids.intersection(&available_epg_ids).cloned().collect());
        accumulator.finish_epg_with_icon_overrides()
    }
}
