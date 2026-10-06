use super::layout::TIME_BLOCK_MINS;
use chrono::{Local, Offset, TimeZone, Utc};
use shared::model::{EpgGridRow, EpgTv, SearchRequest, REGEX_CACHE};
use std::{collections::HashMap, ops::Range, rc::Rc, sync::Arc};

pub(super) struct EpgGridProgramme {
    pub start: i64,
    pub stop: i64,
    /// Minutes relative to `EpgGridModel::start_window_min`.
    pub left_min: i64,
    pub right_min: i64,
    /// `HH:MM-HH:MM` in local time.
    pub time_label: Rc<str>,
    pub title: Rc<str>,
}

pub(super) struct EpgGridChannel {
    /// Stable index in the loaded data, used as render key.
    pub key: usize,
    pub epg_id: Arc<str>,
    /// Playlist channel; `None` for sources without a playlist mapping.
    pub virtual_id: Option<u32>,
    pub title: Rc<str>,
    pub title_lc: String,
    pub icon: Option<Arc<str>>,
    /// Sorted by start.
    pub programmes: Vec<EpgGridProgramme>,
    /// Running maximum of `right_min`, same length as `programmes`.
    pub max_stop_prefix: Vec<i64>,
}

pub(super) struct EpgGridModel {
    /// Unix secs of the earliest start and latest stop over all programmes.
    pub start: i64,
    pub stop: i64,
    pub start_window_min: i64,
    pub num_blocks: i64,
    pub channels: Vec<Rc<EpgGridChannel>>,
}

/// Identity handle for hook dependencies: equal only when it is the same model instance.
#[derive(Clone)]
pub(super) struct ModelRef(pub Rc<EpgGridModel>);

impl PartialEq for ModelRef {
    fn eq(&self, other: &Self) -> bool { Rc::ptr_eq(&self.0, &other.0) }
}

fn hours_minutes(secs_of_day: i64) -> (i64, i64) {
    let secs = secs_of_day.rem_euclid(86_400);
    (secs / 3600, (secs % 3600) / 60)
}

pub(super) fn format_time_label(start: i64, stop: i64, offset_secs: &mut impl FnMut(i64) -> i32) -> String {
    let (start_h, start_m) = hours_minutes(start + i64::from(offset_secs(start)));
    let (stop_h, stop_m) = hours_minutes(stop + i64::from(offset_secs(stop)));
    format!("{start_h:02}:{start_m:02}-{stop_h:02}:{stop_m:02}")
}

pub(super) fn local_offset_secs(ts: i64) -> i32 {
    Utc.timestamp_opt(ts, 0).single().map_or(0, |utc| utc.with_timezone(&Local).offset().fix().local_minus_utc())
}

/// Caches the UTC offset per exact timestamp. Bucketing (e.g. per hour) is wrong for zones
/// whose offset changes on a half hour. Adjacent programmes share boundaries, so about half
/// of the lookups hit the cache. The cache lives for one model build only.
pub(super) fn cached_offsets(mut lookup: impl FnMut(i64) -> i32) -> impl FnMut(i64) -> i32 {
    let mut cache = HashMap::<i64, i32>::new();
    move |ts| *cache.entry(ts).or_insert_with(|| lookup(ts))
}

#[derive(Default)]
pub(super) struct RequestSeq(u32);

impl RequestSeq {
    pub fn begin(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(1);
        self.0
    }

    pub fn is_current(&self, token: u32) -> bool { self.0 == token }
}

/// Completes one load: applies `result` only if `token` is still current, and always hides
/// the busy indicator (it is a counter, so every Show needs exactly one Hide).
pub(super) fn finish_load<T>(seq: &RequestSeq, token: u32, result: T, apply: impl FnOnce(T), hide_busy: impl FnOnce()) {
    if seq.is_current(token) {
        apply(result);
    }
    hide_busy();
}

pub(super) struct ChannelSource<'a, I: Iterator<Item = (i64, i64, Option<&'a str>)>> {
    pub key: usize,
    pub epg_id: Arc<str>,
    pub virtual_id: Option<u32>,
    pub title: &'a str,
    pub icon: Option<Arc<str>>,
    pub programmes: I,
}

fn build_channel<'a, I>(
    src: ChannelSource<'a, I>,
    start_window_min: i64,
    offset_secs: &mut impl FnMut(i64) -> i32,
) -> EpgGridChannel
where
    I: Iterator<Item = (i64, i64, Option<&'a str>)>,
{
    let mut sorted: Vec<_> = src.programmes.collect();
    sorted.sort_by_key(|(start, _, _)| *start);
    let mut running = i64::MIN;
    let mut max_stop_prefix = Vec::with_capacity(sorted.len());
    let programmes = sorted
        .into_iter()
        .map(|(start, stop, title)| {
            let right_min = stop / 60 - start_window_min;
            running = running.max(right_min);
            max_stop_prefix.push(running);
            EpgGridProgramme {
                start,
                stop,
                left_min: start / 60 - start_window_min,
                right_min,
                time_label: format_time_label(start, stop, offset_secs).into(),
                title: title.unwrap_or_default().into(),
            }
        })
        .collect();
    let title: Rc<str> = src.title.into();
    EpgGridChannel {
        key: src.key,
        epg_id: src.epg_id,
        virtual_id: src.virtual_id,
        title_lc: title.to_lowercase(),
        title,
        icon: src.icon,
        programmes,
        max_stop_prefix,
    }
}

fn window_layout(start: i64, stop: i64) -> (i64, i64) {
    let block_secs = TIME_BLOCK_MINS * 60;
    let start_window_min = ((start / block_secs) * block_secs / 60).max(0);
    let end_min = (stop / 60).max(0);
    let num_blocks = ((end_min - start_window_min).max(0) + TIME_BLOCK_MINS - 1) / TIME_BLOCK_MINS;
    (start_window_min, num_blocks)
}

impl EpgGridModel {
    pub fn from_epg_tv(tv: &EpgTv, mut offset_secs: impl FnMut(i64) -> i32) -> Self {
        let (start_window_min, num_blocks) = window_layout(tv.start, tv.stop);
        let channels = tv
            .channels
            .iter()
            .enumerate()
            .map(|(key, ch)| {
                Rc::new(build_channel(
                    ChannelSource {
                        key,
                        epg_id: Arc::clone(&ch.id),
                        virtual_id: None,
                        title: ch.title.as_deref().unwrap_or_default(),
                        icon: ch.icon.clone(),
                        programmes: ch.programmes.iter().map(|p| (p.start, p.stop, p.title.as_deref())),
                    },
                    start_window_min,
                    &mut offset_secs,
                ))
            })
            .collect();
        Self { start: tv.start, stop: tv.stop, start_window_min, num_blocks, channels }
    }
}

impl EpgGridModel {
    /// Model of one playlist group: rows carry the playlist channel (`virtual_id`, name, logo).
    pub fn from_grid_rows(rows: &[EpgGridRow], mut offset_secs: impl FnMut(i64) -> i32) -> Self {
        let (start, stop) = rows
            .iter()
            .flat_map(|row| row.programmes.iter())
            .fold(None, |bounds: Option<(i64, i64)>, programme| {
                Some(bounds.map_or((programme.start, programme.stop), |(start, stop)| {
                    (start.min(programme.start), stop.max(programme.stop))
                }))
            })
            .unwrap_or((0, 0));
        let (start_window_min, num_blocks) = window_layout(start, stop);
        let channels = rows
            .iter()
            .enumerate()
            .map(|(key, row)| {
                Rc::new(build_channel(
                    ChannelSource {
                        key,
                        epg_id: Arc::clone(&row.epg_channel_id),
                        virtual_id: Some(row.virtual_id),
                        title: &row.name,
                        icon: (!row.logo.is_empty()).then(|| Arc::clone(&row.logo)),
                        programmes: row.programmes.iter().map(|p| (p.start, p.stop, p.title.as_deref())),
                    },
                    start_window_min,
                    &mut offset_secs,
                ))
            })
            .collect();
        Self { start, stop, start_window_min, num_blocks, channels }
    }
}

/// Index range of programmes that may overlap `[from_min, to_min)`. `max_stop_prefix` is
/// monotonic, so everything before `first` ended before the window even when individual
/// `stop` values are not sorted. Callers still skip entries with `right_min <= from_min`.
pub(super) fn visible_programme_range(channel: &EpgGridChannel, from_min: i64, to_min: i64) -> Range<usize> {
    let first = channel.max_stop_prefix.partition_point(|&stop| stop <= from_min);
    let last = channel.programmes.partition_point(|p| p.left_min < to_min);
    first..last.max(first)
}

pub(super) fn filter_channel_indices(model: &EpgGridModel, filter: &SearchRequest) -> Vec<usize> {
    let matching = |pred: &dyn Fn(&EpgGridChannel) -> bool| {
        model.channels.iter().enumerate().filter(|(_, c)| pred(c)).map(|(i, _)| i).collect()
    };
    match filter {
        SearchRequest::Clear => (0..model.channels.len()).collect(),
        SearchRequest::Text(pattern, _) => {
            let lc = pattern.to_lowercase();
            matching(&|c| c.title_lc.contains(&lc))
        }
        SearchRequest::Regexp(pattern, _) => match REGEX_CACHE.get_or_compile(pattern) {
            Ok(re) => matching(&|c| re.is_match(&c.title)),
            Err(_) => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{
        model::{EpgChannel, EpgGridProgrammeDto, EpgProgramme, EpgTv},
        utils::Internable,
    };
    use std::cell::{Cell, RefCell};

    fn tv(programmes: &[(i64, i64)]) -> EpgTv {
        let mut ch = EpgChannel::new("c1".intern());
        ch.title = Some("Chan".intern());
        ch.programmes = programmes
            .iter()
            .map(|&(s, e)| {
                let mut p = EpgProgramme::new(s, e, "c1".intern());
                p.title = Some("P".intern());
                p
            })
            .collect();
        EpgTv::new(vec![ch])
    }

    #[test]
    fn model_positions_are_minutes_from_aligned_window_start() {
        // window start 10:10 aligns down to 10:00 (30 min blocks)
        let base = 36_000; // 10:00 UTC
        let model = EpgGridModel::from_epg_tv(&tv(&[(base + 600, base + 2400)]), |_| 0);
        assert_eq!(model.start_window_min, base / 60);
        let p = &model.channels[0].programmes[0];
        assert_eq!((p.left_min, p.right_min), (10, 40));
        assert_eq!(&*p.time_label, "10:10-10:40");
    }

    #[test]
    fn time_label_uses_offset() {
        let mut off = |_| 3600;
        assert_eq!(format_time_label(0, 1800, &mut off), "01:00-01:30");
    }

    #[test]
    fn time_label_handles_offset_change_between_start_and_end() {
        // Lord Howe style: +10:30 before 15:30 UTC, +11:00 from then on.
        let switch = 15 * 3600 + 1800;
        let mut off = |ts: i64| if ts < switch { 37_800 } else { 39_600 };
        // 15:00 UTC -> 01:30 local; 16:00 UTC -> 03:00 local
        assert_eq!(format_time_label(15 * 3600, 16 * 3600, &mut off), "01:30-03:00");
    }

    #[test]
    fn cached_offsets_caches_exact_timestamps_not_hour_buckets() {
        let calls = Cell::new(0);
        let mut cached = cached_offsets(|ts| {
            calls.set(calls.get() + 1);
            if ts < 1800 {
                0
            } else {
                1800
            }
        });
        assert_eq!(cached(0), 0);
        assert_eq!(cached(0), 0);
        assert_eq!(calls.get(), 1);
        // same UTC hour, different timestamp: must ask again and see the new offset
        assert_eq!(cached(1800), 1800);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn visible_range_keeps_running_programme_that_started_before_window() {
        // long programme 0..300 min, short ones after; non-monotonic stop
        let model = EpgGridModel::from_epg_tv(&tv(&[(0, 18_000), (600, 1200), (1200, 1800), (18_000, 19_800)]), |_| 0);
        let ch = &model.channels[0];
        let idx: Vec<_> = visible_programme_range(ch, 100, 120).filter(|&i| ch.programmes[i].right_min > 100).collect();
        assert_eq!(idx, vec![0]);
    }

    #[test]
    fn visible_range_excludes_programmes_outside_window() {
        let model = EpgGridModel::from_epg_tv(&tv(&[(0, 600), (600, 1200), (1200, 1800), (1800, 2400)]), |_| 0);
        // minutes 12..25 overlap programmes 1 (10..20) and 2 (20..30)
        assert_eq!(visible_programme_range(&model.channels[0], 12, 25), 1..3);
    }

    #[test]
    fn visible_range_empty_channel() {
        let model = EpgGridModel::from_epg_tv(&tv(&[]), |_| 0);
        assert!(visible_programme_range(&model.channels[0], 0, 100).is_empty());
    }

    #[test]
    fn programmes_are_sorted_by_start() {
        let model = EpgGridModel::from_epg_tv(&tv(&[(1200, 1800), (0, 600)]), |_| 0);
        let starts: Vec<_> = model.channels[0].programmes.iter().map(|p| p.start).collect();
        assert_eq!(starts, vec![0, 1200]);
    }

    #[test]
    fn model_ref_equality_is_identity() {
        let a = ModelRef(Rc::new(EpgGridModel::from_epg_tv(&tv(&[]), |_| 0)));
        let b = ModelRef(Rc::new(EpgGridModel::from_epg_tv(&tv(&[]), |_| 0)));
        assert!(a == a.clone());
        assert!(a != b);
    }

    #[test]
    fn request_seq_only_latest_token_is_current() {
        let mut seq = RequestSeq::default();
        let first = seq.begin();
        let second = seq.begin();
        assert!(!seq.is_current(first));
        assert!(seq.is_current(second));
    }

    // Simulates the real load order: A starts, B starts, A's delayed callback runs, then B's.
    #[test]
    fn finish_load_drops_superseded_result_and_always_hides_busy() {
        let mut seq = RequestSeq::default();
        let busy = Cell::new(0i32);
        let shown = RefCell::new(None::<&str>);
        let show = || busy.set(busy.get() + 1);
        let hide = || busy.set(busy.get() - 1);

        show();
        let a = seq.begin();
        show();
        let b = seq.begin();

        finish_load(&seq, a, Some("A"), |r| *shown.borrow_mut() = r, hide);
        assert_eq!(*shown.borrow(), None, "superseded result must not be applied");
        assert_eq!(busy.get(), 1, "superseded request must still hide its busy");

        finish_load(&seq, b, Some("B"), |r| *shown.borrow_mut() = r, hide);
        assert_eq!(*shown.borrow(), Some("B"));
        assert_eq!(busy.get(), 0);
    }

    #[test]
    fn finish_load_error_path_clears_data_and_hides_busy() {
        let mut seq = RequestSeq::default();
        let busy = Cell::new(1i32);
        let shown = RefCell::new(Some("old"));
        let token = seq.begin();
        finish_load(&seq, token, None::<&str>, |r| *shown.borrow_mut() = r, || busy.set(busy.get() - 1));
        assert_eq!(*shown.borrow(), None);
        assert_eq!(busy.get(), 0);
    }

    #[test]
    fn finish_load_late_error_of_superseded_request_keeps_current_data() {
        let mut seq = RequestSeq::default();
        let busy = Cell::new(2i32);
        let shown = RefCell::new(Some("B"));
        let a = seq.begin();
        let _b = seq.begin();
        finish_load(&seq, a, None::<&str>, |r| *shown.borrow_mut() = r, || busy.set(busy.get() - 1));
        assert_eq!(*shown.borrow(), Some("B"));
        assert_eq!(busy.get(), 1);
    }

    fn grid_row(virtual_id: u32, epg: &str, programmes: Vec<EpgGridProgrammeDto>) -> EpgGridRow {
        EpgGridRow { virtual_id, name: "ZDF".intern(), logo: "".intern(), epg_channel_id: epg.intern(), programmes }
    }

    #[test]
    fn grid_rows_model_carries_virtual_id_and_window() {
        let rows = vec![grid_row(
            42,
            "zdf.de",
            vec![EpgGridProgrammeDto { start: 36_600, stop: 38_400, title: Some("News".intern()) }],
        )];
        let model = EpgGridModel::from_grid_rows(&rows, |_| 0);
        assert_eq!(model.channels[0].virtual_id, Some(42));
        assert_eq!(&*model.channels[0].title, "ZDF");
        assert_eq!(model.channels[0].icon, None);
        assert_eq!(model.start_window_min, 36_000 / 60);
        assert_eq!((model.start, model.stop), (36_600, 38_400));
        assert_eq!(&*model.channels[0].programmes[0].time_label, "10:10-10:40");
    }

    #[test]
    fn grid_rows_without_programmes_have_empty_window() {
        let model = EpgGridModel::from_grid_rows(&[grid_row(1, "x", vec![])], |_| 0);
        assert_eq!((model.start, model.stop, model.num_blocks), (0, 0, 0));
        assert!(model.channels[0].programmes.is_empty());
    }

    #[test]
    fn filter_channel_indices_text_is_case_insensitive_and_keeps_order() {
        let tv = EpgTv::new(
            ["Das Erste", "ZDF", "arte"]
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut ch = EpgChannel::new(format!("c{i}").intern());
                    ch.title = Some((*t).intern());
                    ch
                })
                .collect(),
        );
        let model = EpgGridModel::from_epg_tv(&tv, |_| 0);
        assert_eq!(filter_channel_indices(&model, &SearchRequest::Clear), vec![0, 1, 2]);
        assert_eq!(filter_channel_indices(&model, &SearchRequest::Text("ER".into(), None)), vec![0]);
    }
}
