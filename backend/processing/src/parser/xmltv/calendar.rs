use super::{
    xml::{transform_in_batches, GuideHooks},
    EpgMergeAccumulator, IcsPersistedSource, TVGuide,
};
use crate::processor::EpgIdCache;
use shared::model::{HookCtx, PluginEpgChannel, PluginHost, PluginRunReport};
use std::sync::Arc;
use tuliprox_core::model::{IcsDummyPolicy, PersistedEpgSource};
use tuliprox_parser::ics;

impl TVGuide {
    /// An ICS source is one calendar feeding one virtual channel, so
    /// `on-epg-loaded` sees a channel list of one: the channel id with the
    /// calendar's title and match names as its names. The names it returns
    /// replace the title and match names as the matching candidates (the channel
    /// id stays one). `transform-programmes` runs over the parsed programmes in
    /// bounded batches. Both only while `EpgIdCache::hooks` is enabled and a
    /// plugin wants them.
    pub(super) async fn process_ics_file<P: PluginHost>(
        plugins: &P,
        id_cache: &mut EpgIdCache,
        epg_source: &PersistedEpgSource,
        source_order: usize,
        accumulator: &mut EpgMergeAccumulator,
        source: IcsPersistedSource<'_>,
    ) -> bool {
        let IcsPersistedSource { channel_id, channel_title, match_names, config } = source;
        let hooks = GuideHooks::probe(plugins, id_cache);
        let input = id_cache.hooks.input.clone();
        let ctx = HookCtx::new("").with_input(&input);
        let mut report = PluginRunReport::default();

        let mut candidates = Vec::with_capacity(2 + match_names.len());
        candidates.push(channel_id.to_string());
        if let Some(title) = channel_title {
            candidates.push(title.to_string());
        }
        candidates.extend(match_names.iter().map(ToString::to_string));
        if hooks.loaded {
            let names = candidates.iter().skip(1).cloned().collect();
            let mut view = vec![PluginEpgChannel { id: channel_id.to_string(), names, icon: None }];
            plugins.on_epg_loaded(&ctx, &mut view, &mut report).await;
            if let Some(view) = view.into_iter().find(|view| view.id.eq_ignore_ascii_case(channel_id)) {
                candidates.truncate(1);
                candidates.extend(view.names);
            }
        }
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
            id_cache.hooks.report.merge(report);
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
            Ok(mut channel) => {
                if hooks.transform {
                    let programmes = std::mem::take(&mut channel.programmes);
                    channel.programmes = transform_in_batches(plugins, &ctx, programmes, &mut report).await;
                }
                id_cache.hooks.report.merge(report);
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
                id_cache.hooks.report.merge(report);
                log::warn!("Failed to process ICS EPG file {}: {err}", epg_source.file_path.display());
                false
            }
        }
    }
}
