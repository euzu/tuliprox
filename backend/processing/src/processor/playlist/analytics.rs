use crate::{epg::epg_source_label, input_cache::InputStatus};
use log::warn;
use shared::model::{
    ClusterAnalytics, EpgAnalytics, EpgSourceAnalytics, EpgSourceTypeDto, GroupCount, PlaylistGroup, PlaylistItem,
    PlaylistItemType, StreamProperties, TargetAnalyticsDto, XtreamCluster, TARGET_ANALYTICS_VERSION,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};
use tuliprox_core::{
    model::{Config, Epg, EpgConfig},
    utils::with_folded_epg_id,
};
use tuliprox_repository::{
    get_target_storage_path, load_target_analytics, save_target_analytics, save_target_analytics_unmapped_csv,
};

#[derive(Debug, Clone, Default)]
pub struct TargetInputInfo {
    pub name: String,
    pub epg: Option<EpgConfig>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TargetAnalyticsRawCounts {
    pub live: usize,
    pub vod: usize,
    pub series: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SourceAnalyticsInfo {
    pub inputs: Vec<TargetInputInfo>,
    pub raw_counts: TargetAnalyticsRawCounts,
    pub input_statuses: Arc<HashMap<String, InputStatus>>,
}

pub fn collect_source_analytics_info(
    playlists: &mut [crate::fetched_playlist::FetchedPlaylist<'_>],
) -> SourceAnalyticsInfo {
    let mut raw_counts = TargetAnalyticsRawCounts::default();
    let mut inputs = Vec::with_capacity(playlists.len());
    for provider_fpl in playlists.iter_mut() {
        inputs.push(TargetInputInfo { name: provider_fpl.input.name.to_string(), epg: provider_fpl.input.epg.clone() });
        raw_counts.live += provider_fpl.get_cluster_count(XtreamCluster::Live);
        raw_counts.vod += provider_fpl.get_cluster_count(XtreamCluster::Video);
        raw_counts.series += provider_fpl.get_cluster_count(XtreamCluster::Series);
    }
    SourceAnalyticsInfo { inputs, raw_counts, input_statuses: Arc::default() }
}

#[derive(Debug, Clone)]
pub struct TargetAnalyticsInput {
    pub started: Instant,
    pub source_info: Arc<SourceAnalyticsInfo>,
}

impl Default for TargetAnalyticsInput {
    fn default() -> Self { Self { started: Instant::now(), source_info: Arc::new(SourceAnalyticsInfo::default()) } }
}

impl TargetAnalyticsInput {
    pub fn new(source_info: Arc<SourceAnalyticsInfo>) -> Self { Self { started: Instant::now(), source_info } }

    pub fn input_names(&self) -> Vec<String> { self.source_info.inputs.iter().map(|i| i.name.clone()).collect() }
}

/// Quotes a field per RFC 4180: if it contains commas, double quotes, CR, or LF,
/// it is wrapped in double quotes and inner quotes are doubled.
/// Fields starting with `=`, `+`, `-` or `@` get a leading `'` so spreadsheets do not evaluate them as formulas.
pub fn push_csv_field(out: &mut String, field: &str) {
    let formula_guard = field.starts_with(['=', '+', '-', '@']);
    if field.contains([',', '"', '\r', '\n']) {
        out.push('"');
        if formula_guard {
            out.push('\'');
        }
        out.push_str(&field.replace('"', "\"\""));
        out.push('"');
    } else {
        if formula_guard {
            out.push('\'');
        }
        out.push_str(field);
    }
}

pub fn push_unmapped_csv_row(out: &mut String, group: &str, name: &str, epg_channel_id: Option<&str>, input: &str) {
    push_csv_field(out, group);
    out.push(',');
    push_csv_field(out, name);
    out.push(',');
    push_csv_field(out, epg_channel_id.unwrap_or(""));
    out.push(',');
    push_csv_field(out, input);
    out.push('\n');
}

struct EpgGuideChannelRef<'a> {
    input_name: &'a str,
    source_idx: u16,
    max_stop: Option<i64>,
}

fn build_epg_lookup(new_epg: &[Epg]) -> HashMap<String, EpgGuideChannelRef<'_>> {
    let total: usize = new_epg.iter().map(|epg| epg.children.len()).sum();
    let mut epg_map = HashMap::with_capacity(total);
    for epg in new_epg {
        let input_name = epg.input_name.as_deref().unwrap_or("");
        for (i, channel) in epg.children.iter().enumerate() {
            // Channels without programmes are not written to the published EPG, so they do not count as mapped.
            if channel.programmes.is_empty() {
                continue;
            }
            let source_idx = epg.channel_sources.as_ref().and_then(|sources| sources.get(i).copied()).unwrap_or(0);
            let max_stop = channel.programmes.iter().map(|p| p.stop).max();
            let folded_id = with_folded_epg_id(&channel.id, ToString::to_string);
            epg_map.insert(folded_id, EpgGuideChannelRef { input_name, source_idx, max_stop });
        }
    }
    epg_map
}

/// Sorts group counts descending by count, then ascending by name.
/// Returns the top `limit` items, and the sum of remaining items in the other bucket.
fn top_n_groups(counts: HashMap<&str, usize>, limit: usize) -> (Vec<GroupCount>, usize) {
    let mut vec: Vec<GroupCount> =
        counts.into_iter().map(|(name, count)| GroupCount { name: name.to_string(), count }).collect();
    vec.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));

    let mut top = Vec::new();
    let mut other = 0usize;
    for (i, gc) in vec.into_iter().enumerate() {
        if i < limit {
            top.push(gc);
        } else {
            other += gc.count;
        }
    }
    (top, other)
}

#[derive(Default)]
struct PlaylistItemsStats<'a> {
    live_count: usize,
    vod_count: usize,
    live_groups: HashSet<&'a str>,
    vod_groups: HashSet<&'a str>,
    series_groups: HashSet<&'a str>,
    live_group_counts: HashMap<&'a str, usize>,
    missing_group_counts: HashMap<&'a str, usize>,
    catchup_channels: usize,
    catchup_max_days: Option<u32>,
    episode_items_count: usize,
    show_items_count: usize,
    show_details_episodes_sum: usize,
    shows_with_episodes: usize,
    distinct_episode_series_names: HashSet<&'a str>,
    epg_mapped: usize,
    overall_guide_until: Option<i64>,
    source_mapped_counts: HashMap<(&'a str, u16), usize>,
    source_max_stops: HashMap<(&'a str, u16), i64>,
    unmapped_csv: String,
}

impl<'a> PlaylistItemsStats<'a> {
    fn new() -> Self {
        Self {
            live_count: 0,
            vod_count: 0,
            live_groups: HashSet::new(),
            vod_groups: HashSet::new(),
            series_groups: HashSet::new(),
            live_group_counts: HashMap::new(),
            missing_group_counts: HashMap::new(),
            catchup_channels: 0,
            catchup_max_days: None,
            episode_items_count: 0,
            show_items_count: 0,
            show_details_episodes_sum: 0,
            shows_with_episodes: 0,
            distinct_episode_series_names: HashSet::new(),
            epg_mapped: 0,
            overall_guide_until: None,
            source_mapped_counts: HashMap::new(),
            source_max_stops: HashMap::new(),
            unmapped_csv: String::from("group,name,epg_channel_id,input\n"),
        }
    }

    fn accumulate_live_item(
        &mut self,
        item: &PlaylistItem,
        group_title: &'a str,
        epg_map: &HashMap<String, EpgGuideChannelRef<'a>>,
    ) {
        if let Some(StreamProperties::Live(ref l)) = item.header.additional_properties {
            if l.tv_archive.is_some_and(|a| a > 0) {
                self.catchup_channels += 1;
                if let Some(duration) = l.tv_archive_duration {
                    if duration > 0 {
                        let current_max = self.catchup_max_days.unwrap_or(0);
                        self.catchup_max_days = Some(current_max.max(duration.unsigned_abs()));
                    }
                }
            }
        }

        let mut is_mapped = false;
        if let Some(epg_id) = item.header.epg_channel_id.as_deref() {
            if !epg_id.is_empty() {
                if let Some(guide_ref) = with_folded_epg_id(epg_id, |folded| epg_map.get(folded)) {
                    is_mapped = true;
                    self.epg_mapped += 1;
                    let key = (guide_ref.input_name, guide_ref.source_idx);
                    *self.source_mapped_counts.entry(key).or_default() += 1;
                    if let Some(stop) = guide_ref.max_stop {
                        let current_source_max = self.source_max_stops.entry(key).or_insert(stop);
                        *current_source_max = (*current_source_max).max(stop);

                        let current_overall = self.overall_guide_until.unwrap_or(stop);
                        self.overall_guide_until = Some(current_overall.max(stop));
                    }
                }
            }
        }

        if !is_mapped {
            *self.missing_group_counts.entry(group_title).or_default() += 1;
            push_unmapped_csv_row(
                &mut self.unmapped_csv,
                group_title,
                &item.header.get_name(),
                item.header.epg_channel_id.as_deref(),
                &item.header.input_name,
            );
        }
    }

    fn accumulate_series_item(&mut self, item: &'a PlaylistItem) {
        match item.header.item_type {
            PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
                self.episode_items_count += 1;
                let series_key = if item.header.parent_code.is_empty() {
                    item.header.name.as_ref()
                } else {
                    item.header.parent_code.as_ref()
                };
                self.distinct_episode_series_names.insert(series_key);
            }
            PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo => {
                self.show_items_count += 1;
                if let Some(StreamProperties::Series(ref s)) = item.header.additional_properties {
                    if let Some(ref details) = s.details {
                        if let Some(ref episodes) = details.episodes {
                            self.shows_with_episodes += 1;
                            self.show_details_episodes_sum += episodes.len();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Returns `(series, episodes, episodes_complete)`.
    fn series_and_episodes_counts(&self) -> (usize, Option<usize>, bool) {
        if self.show_items_count == 0 {
            return (self.distinct_episode_series_names.len(), Some(self.episode_items_count), true);
        }
        if self.shows_with_episodes == self.show_items_count {
            return (self.show_items_count, Some(self.show_details_episodes_sum), true);
        }
        let known = self.episode_items_count.max(self.show_details_episodes_sum);
        (self.show_items_count, (known > 0).then_some(known), false)
    }
}

fn build_epg_source_rows(
    analytics_input: &TargetAnalyticsInput,
    now: u64,
    source_mapped_counts: &HashMap<(&str, u16), usize>,
    source_max_stops: &HashMap<(&str, u16), i64>,
) -> Vec<EpgSourceAnalytics> {
    let mut sources = Vec::new();
    let default_status = InputStatus::default();
    for input_info in &analytics_input.source_info.inputs {
        if let Some(ref epg_config) = input_info.epg {
            let input_status =
                analytics_input.source_info.input_statuses.get(&input_info.name).unwrap_or(&default_status);

            for (idx, epg_source) in epg_config.sources.iter().enumerate() {
                let label = epg_source_label(&epg_source.url);
                let kind = EpgSourceTypeDto::from(epg_source.source_type);

                let fetch_status = input_status
                    .epg_sources
                    .get(idx)
                    .filter(|e| e.label == label)
                    .or_else(|| input_status.epg_sources.iter().find(|e| e.label == label));

                let fetched_at = fetch_status.and_then(|s| s.fetched_at);
                let failed_at = fetch_status.and_then(|s| if s.failed_at > s.fetched_at { s.failed_at } else { None });
                let error = if failed_at.is_some() { fetch_status.and_then(|s| s.error.clone()) } else { None };

                #[allow(clippy::cast_possible_truncation)]
                let mapped_channels =
                    source_mapped_counts.get(&(input_info.name.as_str(), idx as u16)).copied().unwrap_or(0);

                #[allow(clippy::cast_possible_truncation)]
                let guide_days = source_max_stops.get(&(input_info.name.as_str(), idx as u16)).copied().map(|stop| {
                    let now_i64 = i64::try_from(now).unwrap_or(0);
                    if stop > now_i64 {
                        let diff = u64::try_from(stop - now_i64).unwrap_or(0);
                        u16::try_from(diff.div_ceil(86_400)).unwrap_or(u16::MAX)
                    } else {
                        0
                    }
                });

                sources.push(EpgSourceAnalytics {
                    input: input_info.name.clone(),
                    label,
                    kind,
                    mapped_channels,
                    guide_days,
                    fetched_at,
                    failed_at,
                    error,
                });
            }
        }
    }
    sources
}

pub fn compute_target_analytics(
    _storage_dir: &str,
    analytics_input: &TargetAnalyticsInput,
    watch_playlist: &[PlaylistGroup],
    new_epg: &[Epg],
) -> (TargetAnalyticsDto, String) {
    let now = shared::utils::current_time_secs();
    let took_secs = analytics_input.started.elapsed().as_secs();

    let epg_map = build_epg_lookup(new_epg);

    let mut stats = PlaylistItemsStats::new();
    for group in watch_playlist {
        let group_title: &str = &group.title;
        let mut live_in_group = 0usize;
        let mut vod_in_group = 0usize;
        let mut has_series = false;

        for item in &group.channels {
            match item.header.xtream_cluster {
                XtreamCluster::Live => {
                    live_in_group += 1;
                    stats.accumulate_live_item(item, group_title, &epg_map);
                }
                XtreamCluster::Video => {
                    vod_in_group += 1;
                }
                XtreamCluster::Series => {
                    has_series = true;
                    stats.accumulate_series_item(item);
                }
            }
        }

        if live_in_group > 0 {
            stats.live_count += live_in_group;
            stats.live_groups.insert(group_title);
            *stats.live_group_counts.entry(group_title).or_default() += live_in_group;
        }
        if vod_in_group > 0 {
            stats.vod_count += vod_in_group;
            stats.vod_groups.insert(group_title);
        }
        if has_series {
            stats.series_groups.insert(group_title);
        }
    }

    let (series_count, episodes, episodes_complete) = stats.series_and_episodes_counts();
    let (top_live_groups, _) = top_n_groups(stats.live_group_counts, 5);
    let (missing_by_group, missing_other) = top_n_groups(stats.missing_group_counts, 4);

    let has_any_epg_configured = analytics_input.source_info.inputs.iter().any(|i| i.epg.is_some());
    let epg_analytics = if has_any_epg_configured {
        let sources = build_epg_source_rows(analytics_input, now, &stats.source_mapped_counts, &stats.source_max_stops);
        Some(EpgAnalytics {
            live_channels: stats.live_count,
            mapped: stats.epg_mapped,
            guide_until: stats.overall_guide_until,
            sources,
            missing_by_group,
            missing_other,
        })
    } else {
        None
    };

    let dto = TargetAnalyticsDto {
        version: TARGET_ANALYTICS_VERSION,
        success: true,
        updated_at: now,
        took_secs,
        inputs: analytics_input.input_names(),
        live: ClusterAnalytics {
            count: stats.live_count,
            groups: stats.live_groups.len(),
            input_count: analytics_input.source_info.raw_counts.live,
        },
        vod: ClusterAnalytics {
            count: stats.vod_count,
            groups: stats.vod_groups.len(),
            input_count: analytics_input.source_info.raw_counts.vod,
        },
        series: ClusterAnalytics {
            count: series_count,
            groups: stats.series_groups.len(),
            input_count: analytics_input.source_info.raw_counts.series,
        },
        episodes,
        episodes_complete,
        catchup_channels: stats.catchup_channels,
        catchup_max_days: stats.catchup_max_days,
        top_live_groups,
        epg: epg_analytics,
    };

    (dto, stats.unmapped_csv)
}

/// How a target run ended, as far as its analytics are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAnalyticsOutcome {
    /// The playlist was published: store the new metrics and the unmapped CSV.
    Published,
    /// Publishing failed: keep previous metrics if any, otherwise store this run's metrics, flagged as failed.
    Failed,
    /// The run succeeded but published nothing (empty playlist): keep previous metrics and CSV, record the run.
    Unchanged,
}

pub async fn persist_target_analytics(
    config: &Config,
    target_name: &str,
    analytics_dto: TargetAnalyticsDto,
    unmapped_csv: &str,
    outcome: TargetAnalyticsOutcome,
) {
    let Some(target_storage_path) = get_target_storage_path(config, target_name) else {
        warn!("Cannot resolve storage path for target '{target_name}' analytics");
        return;
    };

    let now = shared::utils::current_time_secs();
    let success = outcome != TargetAnalyticsOutcome::Failed;

    let (final_dto, write_csv) = if outcome == TargetAnalyticsOutcome::Published {
        (analytics_dto, true)
    } else {
        match load_target_analytics(&target_storage_path).await {
            Ok(Some(mut previous)) => {
                previous.success = success;
                previous.updated_at = now;
                previous.took_secs = analytics_dto.took_secs;
                (previous, false)
            }
            // No earlier snapshot: this run's metrics are the only ones there are.
            Ok(None) => (TargetAnalyticsDto { success, updated_at: now, ..analytics_dto }, success),
            Err(err) => {
                warn!("Failed to read previous target '{target_name}' analytics: {err}");
                return;
            }
        }
    };

    if let Err(err) = save_target_analytics(&target_storage_path, &final_dto).await {
        warn!("Failed to persist target '{target_name}' analytics: {err}");
    }

    if write_csv {
        if let Err(err) = save_target_analytics_unmapped_csv(&target_storage_path, unmapped_csv).await {
            warn!("Failed to persist target '{target_name}' unmapped channels CSV: {err}");
        }
    }
}

/// Records a target run that failed before analytics could be computed from a playlist
/// (e.g. while preparing the target): previous metrics stay, the run is marked as failed.
/// `started` is when the preparation attempt began, so `took_secs` covers the whole attempt.
pub async fn record_failed_target_analytics(
    config: &Config,
    target_name: &str,
    source_info: &Arc<SourceAnalyticsInfo>,
    started: Instant,
) {
    let input = TargetAnalyticsInput { started, source_info: Arc::clone(source_info) };
    let (dto, csv) = compute_target_analytics(&config.storage_dir, &input, &[], &[]);
    persist_target_analytics(config, target_name, dto, &csv, TargetAnalyticsOutcome::Failed).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{
        model::{
            ConfigTargetDto, EpgChannel, EpgConfigDto, EpgProgramme, EpgSourceDto, LiveStreamProperties, PlaylistItem,
            PlaylistItemHeader, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties,
            SeriesStreamProperties,
        },
        utils::Internable,
    };
    use tuliprox_core::model::ConfigTarget;

    fn make_test_target(name: &str) -> ConfigTarget {
        let dto = ConfigTargetDto { id: 1, name: name.to_string(), ..Default::default() };
        ConfigTarget::from(&dto)
    }

    fn make_live_item(name: &str, group: &str, epg_id: Option<&str>, archive: Option<i32>) -> PlaylistItem {
        let mut live_props = LiveStreamProperties::default();
        if let Some(days) = archive {
            live_props.tv_archive = Some(1);
            live_props.tv_archive_duration = Some(days);
        }

        PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                title: name.intern(),
                group: group.intern(),
                epg_channel_id: epg_id.map(Internable::intern),
                xtream_cluster: XtreamCluster::Live,
                item_type: PlaylistItemType::Live,
                additional_properties: Some(StreamProperties::Live(Box::new(live_props))),
                input_name: "test_input".intern(),
                ..Default::default()
            },
        }
    }

    fn make_vod_item(name: &str, group: &str) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                title: name.intern(),
                group: group.intern(),
                xtream_cluster: XtreamCluster::Video,
                item_type: PlaylistItemType::Video,
                input_name: "test_input".intern(),
                ..Default::default()
            },
        }
    }

    fn make_series_info_item(name: &str, group: &str, episode_count: usize) -> PlaylistItem {
        let episodes = (0..episode_count)
            .map(|i| SeriesStreamDetailEpisodeProperties {
                episode_num: u32::try_from(i + 1).unwrap_or(0),
                ..Default::default()
            })
            .collect();
        let details = SeriesStreamDetailProperties { episodes: Some(episodes), year: None, seasons: None };
        let series_props = SeriesStreamProperties { name: name.intern(), details: Some(details), ..Default::default() };

        PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                title: name.intern(),
                group: group.intern(),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::SeriesInfo,
                additional_properties: Some(StreamProperties::Series(Box::new(series_props))),
                input_name: "test_input".intern(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn test_csv_rfc4180_quoting() {
        let quote = |s: &str| {
            let mut out = String::new();
            push_csv_field(&mut out, s);
            out
        };
        assert_eq!(quote("simple"), "simple");
        assert_eq!(quote("with,comma"), "\"with,comma\"");
        assert_eq!(quote("with\"quote"), "\"with\"\"quote\"");
        assert_eq!(quote("with\nnewline"), "\"with\nnewline\"");
        assert_eq!(quote("with\r\ncrlf"), "\"with\r\ncrlf\"");
        assert_eq!(quote("foo,\"bar\",baz"), "\"foo,\"\"bar\"\",baz\"");

        let mut csv = String::from("group,name,epg_channel_id,input\n");
        push_unmapped_csv_row(&mut csv, "Sports, HD", "Channel \"1\"", Some("ch1.guide"), "provider1");
        push_unmapped_csv_row(&mut csv, "News", "Channel 2", None, "provider2");
        let expected = "group,name,epg_channel_id,input\n\"Sports, HD\",\"Channel \"\"1\"\"\",ch1.guide,provider1\nNews,Channel 2,,provider2\n";
        assert_eq!(csv, expected);

        let mut guarded = String::new();
        push_csv_field(&mut guarded, "=SUM(A1)");
        assert_eq!(guarded, "'=SUM(A1)");
        let mut guarded_quoted = String::new();
        push_csv_field(&mut guarded_quoted, "-a,b");
        assert_eq!(guarded_quoted, "\"'-a,b\"");
        let mut plain = String::new();
        push_csv_field(&mut plain, "a-b");
        assert_eq!(plain, "a-b");
    }

    #[test]
    fn test_analytics_computation_clusters_epg_and_top_groups() {
        let config = Config::default();

        let items = vec![
            // Group A: 3 channels (2 mapped, 1 unmapped)
            make_live_item("Channel A1", "Group A", Some("epg.a1"), Some(7)),
            make_live_item("Channel A2", "Group A", Some("epg.a2"), None),
            make_live_item("Channel A3", "Group A", None, None),
            // Group B: 2 channels (1 mapped, 1 unmapped)
            make_live_item("Channel B1", "Group B", Some("EPG.A1"), Some(14)), // case-folded match to epg.a1
            make_live_item("Channel B2", "Group B", Some("unmapped.id"), None),
            // Group C: 1 unmapped
            make_live_item("Channel C1", "Group C", None, None),
            // Group D: 1 unmapped
            make_live_item("Channel D1", "Group D", None, None),
            // Group E: 1 unmapped
            make_live_item("Channel E1", "Group E", None, None),
            // Group F: 1 unmapped
            make_live_item("Channel F1", "Group F", None, None),
            // VOD
            make_vod_item("Movie 1", "Action"),
            make_vod_item("Movie 2", "Comedy"),
            // Series
            make_series_info_item("Show 1", "Drama", 10),
            make_series_info_item("Show 2", "Drama", 5),
        ];

        let mut groups_map: HashMap<Arc<str>, Vec<PlaylistItem>> = HashMap::new();
        for item in items {
            groups_map.entry(Arc::clone(&item.header.group)).or_default().push(item);
        }

        let watch_playlist: Vec<PlaylistGroup> = groups_map
            .into_iter()
            .map(|(title, channels)| {
                let xtream_cluster = channels[0].header.xtream_cluster;
                PlaylistGroup { id: 0, title, channels, xtream_cluster }
            })
            .collect();

        // Two EPG sources: source 0 has epg.a1, source 1 has epg.a2
        let now_sec = i64::try_from(shared::utils::current_time_secs()).unwrap_or(0);
        let mut ch_a1 = EpgChannel::new("epg.a1".intern());
        ch_a1.programmes.push(EpgProgramme::new(now_sec, now_sec + 86400 * 3, "epg.a1".intern()));

        let mut ch_a2 = EpgChannel::new("epg.a2".intern());
        ch_a2.programmes.push(EpgProgramme::new(now_sec, now_sec + 86400 * 5, "epg.a2".intern()));

        let epg = Epg {
            priority: 0,
            logo_override: false,
            attributes: None,
            children: vec![Arc::new(ch_a1), Arc::new(ch_a2)],
            channel_sources: Some(vec![0, 1]), // channel 0 -> source 0, channel 1 -> source 1
            input_name: Some("test_input".intern()),
        };

        let epg_dto = EpgConfigDto {
            t_sources: vec![
                EpgSourceDto { url: "http://example.com/source0.xml".to_string(), ..Default::default() },
                EpgSourceDto { url: "http://example.com/source1.xml".to_string(), ..Default::default() },
            ],
            ..Default::default()
        };
        let epg_config = EpgConfig::from(&epg_dto);

        let analytics_input = TargetAnalyticsInput::new(Arc::new(SourceAnalyticsInfo {
            inputs: vec![TargetInputInfo { name: "test_input".to_string(), epg: Some(epg_config) }],
            raw_counts: TargetAnalyticsRawCounts { live: 15, vod: 5, series: 3 },
            ..Default::default()
        }));

        let (dto, unmapped_csv) =
            compute_target_analytics(&config.storage_dir, &analytics_input, &watch_playlist, &[epg]);

        assert!(dto.success);
        assert_eq!(dto.live.count, 9);
        assert_eq!(dto.live.groups, 6); // Groups A, B, C, D, E, F
        assert_eq!(dto.live.input_count, 15);

        assert_eq!(dto.vod.count, 2);
        assert_eq!(dto.vod.groups, 2); // Action, Comedy
        assert_eq!(dto.vod.input_count, 5);

        assert_eq!(dto.series.count, 2); // Show 1, Show 2
        assert_eq!(dto.series.groups, 1); // Drama
        assert_eq!(dto.series.input_count, 3);
        assert_eq!(dto.episodes, Some(15)); // 10 + 5

        assert_eq!(dto.catchup_channels, 2);
        assert_eq!(dto.catchup_max_days, Some(14)); // max(7, 14)

        // Top live groups (max 5)
        assert!(dto.top_live_groups.len() <= 5);
        assert_eq!(dto.top_live_groups[0].name, "Group A");
        assert_eq!(dto.top_live_groups[0].count, 3);
        assert_eq!(dto.top_live_groups[1].name, "Group B");
        assert_eq!(dto.top_live_groups[1].count, 2);

        // EPG coverage: 3 mapped channels (A1, A2, B1), 6 unmapped (A3, B2, C1, D1, E1, F1)
        let epg_res = dto.epg.expect("epg analytics present");
        assert_eq!(epg_res.live_channels, 9);
        assert_eq!(epg_res.mapped, 3);
        assert_eq!(epg_res.sources.len(), 2);
        // Source 0 won Channel A1 and Channel B1 (EPG.A1 case-folded) -> mapped_channels = 2
        assert_eq!(epg_res.sources[0].mapped_channels, 2);
        assert_eq!(epg_res.sources[0].guide_days, Some(3));
        // Source 1 won Channel A2 -> mapped_channels = 1
        assert_eq!(epg_res.sources[1].mapped_channels, 1);
        assert_eq!(epg_res.sources[1].guide_days, Some(5));

        // Missing EPG by group: top 4 groups + other
        assert_eq!(epg_res.missing_by_group.len(), 4);
        assert_eq!(epg_res.missing_other, 2); // remaining 2 groups with 1 unmapped each

        assert_eq!(unmapped_csv.lines().skip(1).count(), 6);
    }

    #[test]
    fn test_series_with_details_and_resolved_episodes_does_not_double_count() {
        let show = make_series_info_item("Show 1", "Drama", 10);
        let ep1 = PlaylistItem {
            header: PlaylistItemHeader {
                name: "Show 1 S01E01".intern(),
                title: "Show 1 S01E01".intern(),
                group: "Drama".intern(),
                parent_code: "Show 1".intern(),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::Series,
                ..Default::default()
            },
        };
        let ep2 = PlaylistItem {
            header: PlaylistItemHeader {
                name: "Show 1 S01E02".intern(),
                title: "Show 1 S01E02".intern(),
                group: "Drama".intern(),
                parent_code: "Show 1".intern(),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::Series,
                ..Default::default()
            },
        };

        let group = PlaylistGroup {
            id: 1,
            title: "Drama".intern(),
            xtream_cluster: XtreamCluster::Series,
            channels: vec![show, ep1, ep2],
        };

        let analytics_input = TargetAnalyticsInput::new(Arc::new(SourceAnalyticsInfo {
            inputs: Vec::new(),
            raw_counts: TargetAnalyticsRawCounts { live: 0, vod: 0, series: 3 },
            ..Default::default()
        }));

        let (dto, _) = compute_target_analytics("/tmp", &analytics_input, &[group], &[]);
        assert_eq!(dto.series.count, 1, "show item determines series count");
        assert_eq!(dto.episodes, Some(10), "all shows resolved: details.episodes win, items are not added on top");
        assert!(dto.episodes_complete);
    }

    #[test]
    fn test_partially_resolved_series_report_a_lower_bound() {
        let resolved = make_series_info_item("Show 1", "Drama", 4);
        let mut unresolved = make_series_info_item("Show 2", "Drama", 0);
        if let Some(StreamProperties::Series(ref mut props)) = unresolved.header.additional_properties {
            props.details = None;
        }
        let episode = PlaylistItem {
            header: PlaylistItemHeader {
                name: "Show 1 S01E01".intern(),
                group: "Drama".intern(),
                parent_code: "Show 1".intern(),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::Series,
                ..Default::default()
            },
        };
        let group = PlaylistGroup {
            id: 1,
            title: "Drama".intern(),
            xtream_cluster: XtreamCluster::Series,
            channels: vec![resolved, unresolved, episode],
        };
        let analytics_input = TargetAnalyticsInput::new(Arc::new(SourceAnalyticsInfo {
            inputs: Vec::new(),
            raw_counts: TargetAnalyticsRawCounts { live: 0, vod: 0, series: 3 },
            ..Default::default()
        }));

        let (dto, _) = compute_target_analytics("/tmp", &analytics_input, &[group], &[]);
        assert_eq!(dto.series.count, 2);
        assert_eq!(dto.episodes, Some(4), "known episodes are a lower bound");
        assert!(!dto.episodes_complete);
    }

    #[test]
    fn test_m3u_episodes_without_shows_are_complete() {
        let episode = PlaylistItem {
            header: PlaylistItemHeader {
                name: "M3U Show S01E01".intern(),
                group: "Series".intern(),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::Series,
                ..Default::default()
            },
        };
        let group = PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            xtream_cluster: XtreamCluster::Series,
            channels: vec![episode],
        };
        let analytics_input = TargetAnalyticsInput::new(Arc::new(SourceAnalyticsInfo {
            inputs: Vec::new(),
            raw_counts: TargetAnalyticsRawCounts { live: 0, vod: 0, series: 1 },
            ..Default::default()
        }));

        let (dto, _) = compute_target_analytics("/tmp", &analytics_input, &[group], &[]);
        assert_eq!(dto.episodes, Some(1));
        assert!(dto.episodes_complete);
    }

    #[test]
    fn test_local_series_and_local_series_info() {
        let mut show = make_series_info_item("Local Show", "Local", 5);
        show.header.item_type = PlaylistItemType::LocalSeriesInfo;

        let ep = PlaylistItem {
            header: PlaylistItemHeader {
                name: "Local Show S01E01".intern(),
                title: "Local Show S01E01".intern(),
                group: "Local".intern(),
                parent_code: "Local Show".intern(),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::LocalSeries,
                ..Default::default()
            },
        };

        let group = PlaylistGroup {
            id: 1,
            title: "Local".intern(),
            xtream_cluster: XtreamCluster::Series,
            channels: vec![show, ep],
        };

        let analytics_input = TargetAnalyticsInput::new(Arc::new(SourceAnalyticsInfo {
            inputs: Vec::new(),
            raw_counts: TargetAnalyticsRawCounts { live: 0, vod: 0, series: 2 },
            ..Default::default()
        }));

        let (dto, _) = compute_target_analytics("/tmp", &analytics_input, &[group], &[]);
        assert_eq!(dto.series.count, 1, "LocalSeriesInfo determines series count");
        assert_eq!(dto.episodes, Some(5), "LocalSeriesInfo details count when all shows are resolved");
        assert!(dto.episodes_complete);
    }

    #[tokio::test]
    async fn test_target_failure_keeps_previous_counts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().unwrap().to_string();
        let config = Config { storage_dir: storage_dir.clone(), ..Default::default() };
        let target = make_test_target("failed_target");

        let target_storage = get_target_storage_path(&config, &target.name).unwrap();
        tokio::fs::create_dir_all(&target_storage).await.unwrap();

        // 1. Initial success write
        let initial_dto = TargetAnalyticsDto {
            version: TARGET_ANALYTICS_VERSION,
            success: true,
            updated_at: 1_000,
            took_secs: 3,
            inputs: vec!["in1".to_string()],
            live: ClusterAnalytics { count: 42, groups: 5, input_count: 50 },
            vod: ClusterAnalytics { count: 10, groups: 2, input_count: 10 },
            series: ClusterAnalytics { count: 5, groups: 1, input_count: 5 },
            episodes: Some(100),
            episodes_complete: true,
            catchup_channels: 4,
            catchup_max_days: Some(3),
            top_live_groups: vec![GroupCount { name: "G1".to_string(), count: 42 }],
            epg: None,
        };

        persist_target_analytics(&config, &target.name, initial_dto, "", TargetAnalyticsOutcome::Published).await;

        let loaded = load_target_analytics(&target_storage).await.unwrap().unwrap();
        assert!(loaded.success);
        assert_eq!(loaded.live.count, 42);

        // 2. Failed run: input analytics has 0 counts
        let zero_dto = TargetAnalyticsDto {
            version: TARGET_ANALYTICS_VERSION,
            success: true,
            updated_at: 2_000,
            took_secs: 1,
            inputs: vec!["in1".to_string()],
            live: ClusterAnalytics { count: 0, groups: 0, input_count: 50 },
            vod: ClusterAnalytics { count: 0, groups: 0, input_count: 10 },
            series: ClusterAnalytics { count: 0, groups: 0, input_count: 5 },
            episodes: None,
            episodes_complete: false,
            catchup_channels: 0,
            catchup_max_days: None,
            top_live_groups: Vec::new(),
            epg: None,
        };

        persist_target_analytics(&config, &target.name, zero_dto.clone(), "", TargetAnalyticsOutcome::Failed).await;

        let reloaded = load_target_analytics(&target_storage).await.unwrap().unwrap();
        assert!(!reloaded.success, "success must be false after failure");
        assert_eq!(reloaded.live.count, 42, "previous live count must be preserved");
        assert_eq!(reloaded.episodes, Some(100), "previous episodes must be preserved");
        assert_eq!(reloaded.took_secs, 1);
        assert!(reloaded.updated_at >= 1_000);

        // 3. Successful run that published nothing: previous metrics stay, the run is recorded as successful.
        persist_target_analytics(&config, &target.name, zero_dto.clone(), "", TargetAnalyticsOutcome::Unchanged).await;
        let unchanged = load_target_analytics(&target_storage).await.unwrap().unwrap();
        assert!(unchanged.success);
        assert_eq!(unchanged.live.count, 42, "previous metrics are kept for an unpublished run");

        // 4. First run of another target fails: a failed snapshot with this run's metrics is written.
        let fresh = make_test_target("fresh_target");
        let fresh_storage = get_target_storage_path(&config, &fresh.name).unwrap();
        tokio::fs::create_dir_all(&fresh_storage).await.unwrap();
        persist_target_analytics(&config, &fresh.name, zero_dto, "", TargetAnalyticsOutcome::Failed).await;
        let first_failure = load_target_analytics(&fresh_storage).await.unwrap().expect("failed snapshot written");
        assert!(!first_failure.success);
        assert_eq!(first_failure.live.input_count, 50);
        assert!(!tuliprox_repository::get_target_analytics_unmapped_file_path(&fresh_storage).exists());
    }

    #[tokio::test]
    async fn record_failed_target_analytics_marks_the_previous_snapshot_failed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = Config { storage_dir: temp.path().to_str().unwrap().to_string(), ..Default::default() };
        let target = make_test_target("prepare_failed");
        let target_storage = get_target_storage_path(&config, &target.name).unwrap();
        tokio::fs::create_dir_all(&target_storage).await.unwrap();
        let source_info = Arc::new(SourceAnalyticsInfo {
            raw_counts: TargetAnalyticsRawCounts { live: 7, vod: 0, series: 0 },
            ..Default::default()
        });

        let started = Instant::now().checked_sub(std::time::Duration::from_secs(5)).expect("instant in the past");
        record_failed_target_analytics(&config, &target.name, &source_info, started).await;
        let first = load_target_analytics(&target_storage).await.unwrap().expect("failed snapshot written");
        assert!(!first.success);
        assert_eq!(first.live.input_count, 7);
        assert!(first.took_secs >= 5, "took_secs covers the whole preparation attempt");
    }
}
