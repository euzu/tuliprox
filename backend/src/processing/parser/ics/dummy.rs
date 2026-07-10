use crate::model::IcsDummyConfig;
use chrono::{Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use shared::{error::TuliproxError, model::EpgProgramme, utils::Internable};
use std::sync::Arc;

pub fn fill_dummy_gaps(
    programmes: &mut Vec<EpgProgramme>,
    channel_id: &Arc<str>,
    timezone: &str,
    config: &IcsDummyConfig,
    now: chrono::DateTime<Utc>,
) -> Result<(), TuliproxError> {
    if !config.enabled {
        return Ok(());
    }
    if config.block_hours == 0 || 24 % config.block_hours != 0 {
        return Err(TuliproxError::ConfigEpg(format!(
            "ICS dummy block_hours must divide 24 evenly, got {}",
            config.block_hours
        )));
    }

    let tz: Tz =
        timezone.parse().map_err(|_| TuliproxError::ConfigEpg(format!("Unknown ICS timezone '{timezone}'")))?;

    let local_today = now.with_timezone(&tz).date_naive();
    let start_day = local_today - Duration::days(i64::from(config.days_past));
    let end_day = local_today + Duration::days(i64::from(config.days_future));
    let blocks = build_block_ranges(tz, start_day, end_day, config.block_hours)?;
    let Some(&(window_start, _)) = blocks.first() else {
        return Ok(());
    };
    let window_end = blocks.last().map_or(window_start, |(_, stop)| *stop);

    programmes.sort_by_key(|programme| (programme.start, programme.stop));
    let merged_real_ranges = merge_ranges_in_window(programmes, window_start, window_end);
    let mut dummy_programmes = Vec::new();
    let mut range_index = 0;
    for (block_start, block_end) in blocks {
        fill_block(
            &mut dummy_programmes,
            &merged_real_ranges,
            &mut range_index,
            channel_id,
            block_start,
            block_end,
            config,
        );
    }

    programmes.extend(dummy_programmes);
    programmes.sort_by_key(|programme| (programme.start, programme.stop));
    Ok(())
}

fn build_block_ranges(
    timezone: Tz,
    start_day: NaiveDate,
    end_day: NaiveDate,
    block_hours: u8,
) -> Result<Vec<(i64, i64)>, TuliproxError> {
    let mut blocks = Vec::new();
    let mut day = start_day;
    while day <= end_day {
        let mut boundaries = Vec::with_capacity(usize::from(24 / block_hours) + 1);
        for hour in (0_u32..=24).step_by(usize::from(block_hours)) {
            let local = if hour == 24 {
                (day + Duration::days(1)).and_hms_opt(0, 0, 0)
            } else {
                day.and_hms_opt(hour, 0, 0)
            }
            .ok_or_else(|| TuliproxError::ConfigEpg(format!("Invalid dummy block boundary for {day} {hour}:00")))?;
            boundaries.push(resolve_local_boundary(timezone, local)?);
        }

        for boundary in boundaries.windows(2) {
            let (start, stop) = (boundary[0], boundary[1]);
            // A spring-forward gap can map two nominal boundaries to the same
            // real instant. It therefore represents no programme interval.
            if stop > start {
                blocks.push((start, stop));
            }
        }
        day += Duration::days(1);
    }
    Ok(blocks)
}

fn resolve_local_boundary(timezone: Tz, local: NaiveDateTime) -> Result<i64, TuliproxError> {
    match timezone.from_local_datetime(&local) {
        LocalResult::Single(value) => Ok(value.with_timezone(&Utc).timestamp()),
        LocalResult::Ambiguous(first, second) => Ok(first.min(second).with_timezone(&Utc).timestamp()),
        LocalResult::None => {
            // Map a non-existent wall-clock boundary to the first real instant
            // after the DST gap. Adjacent UTC intervals stay monotone, while the
            // skipped local interval becomes a zero-length block and is omitted.
            let mut candidate = local;
            for _ in 0..=(24 * 60) {
                candidate = candidate
                    .checked_add_signed(Duration::minutes(1))
                    .ok_or_else(|| TuliproxError::ConfigEpg(format!("Invalid local dummy time {local}")))?;
                match timezone.from_local_datetime(&candidate) {
                    LocalResult::Single(value) => return Ok(value.with_timezone(&Utc).timestamp()),
                    LocalResult::Ambiguous(first, second) => {
                        return Ok(first.min(second).with_timezone(&Utc).timestamp());
                    }
                    LocalResult::None => {}
                }
            }
            Err(TuliproxError::ConfigEpg(format!("Could not resolve local dummy time {local}")))
        }
    }
}

fn merge_ranges_in_window(programmes: &[EpgProgramme], window_start: i64, window_end: i64) -> Vec<(i64, i64)> {
    let mut merged = Vec::<(i64, i64)>::new();
    for programme in programmes {
        if programme.stop <= window_start || programme.stop <= programme.start {
            continue;
        }
        if programme.start >= window_end {
            break;
        }

        let start = programme.start.max(window_start);
        let stop = programme.stop.min(window_end);
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(stop);
                continue;
            }
        }
        merged.push((start, stop));
    }
    merged
}

fn fill_block(
    out: &mut Vec<EpgProgramme>,
    real_ranges: &[(i64, i64)],
    range_index: &mut usize,
    channel_id: &Arc<str>,
    block_start: i64,
    block_end: i64,
    config: &IcsDummyConfig,
) {
    if block_end <= block_start {
        return;
    }

    let mut cursor = block_start;
    let min_gap_seconds = i64::from(config.min_gap_minutes) * 60;
    while *range_index < real_ranges.len() && real_ranges[*range_index].1 <= block_start {
        *range_index += 1;
    }

    let mut current_index = *range_index;
    while let Some(&(event_start, event_stop)) = real_ranges.get(current_index) {
        if event_start >= block_end {
            break;
        }

        let clipped_start = event_start.max(block_start);
        let clipped_stop = event_stop.min(block_end);

        if clipped_start > cursor {
            push_dummy_if_large_enough(out, channel_id, cursor, clipped_start, config, min_gap_seconds);
        }

        cursor = cursor.max(clipped_stop);
        if event_stop <= block_end {
            current_index += 1;
        }
        if cursor >= block_end {
            break;
        }
    }
    *range_index = current_index;

    if cursor < block_end {
        push_dummy_if_large_enough(out, channel_id, cursor, block_end, config, min_gap_seconds);
    }
}

fn push_dummy_if_large_enough(
    out: &mut Vec<EpgProgramme>,
    channel_id: &Arc<str>,
    start: i64,
    stop: i64,
    config: &IcsDummyConfig,
    min_gap_seconds: i64,
) {
    if stop - start < min_gap_seconds {
        return;
    }

    out.push(EpgProgramme::new_all(
        start,
        stop,
        Arc::clone(channel_id),
        Some(config.title.as_str().intern()),
        (!config.description.is_empty()).then(|| config.description.as_str().intern()),
        None,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use shared::model::EpgProgramme;

    fn config() -> IcsDummyConfig {
        IcsDummyConfig {
            enabled: true,
            title: "No programme".to_string(),
            description: String::new(),
            days_past: 0,
            days_future: 0,
            block_hours: 4,
            min_gap_minutes: 1,
        }
    }

    fn ts(hour: u32, minute: u32) -> i64 {
        Utc.with_ymd_and_hms(2026, 3, 6, hour, minute, 0).single().expect("valid time").timestamp()
    }

    fn real(start_hour: u32, start_minute: u32, stop_hour: u32, stop_minute: u32) -> EpgProgramme {
        EpgProgramme::new(ts(start_hour, start_minute), ts(stop_hour, stop_minute), "demo".intern())
    }

    fn dummy_ranges(programmes: &[EpgProgramme]) -> Vec<(i64, i64)> {
        programmes
            .iter()
            .filter(|programme| programme.title.as_deref() == Some("No programme"))
            .map(|programme| (programme.start, programme.stop))
            .collect()
    }

    fn assert_dummy_covers_local_day(
        programmes: &[EpgProgramme],
        timezone: &str,
        date: NaiveDate,
        expected_seconds: i64,
    ) {
        let timezone: Tz = timezone.parse().expect("timezone");
        let start = resolve_local_boundary(timezone, date.and_hms_opt(0, 0, 0).expect("midnight")).expect("day start");
        let stop =
            resolve_local_boundary(timezone, (date + Duration::days(1)).and_hms_opt(0, 0, 0).expect("next midnight"))
                .expect("day stop");
        let ranges = dummy_ranges(programmes);

        assert_eq!(ranges.first().map(|range| range.0), Some(start));
        assert_eq!(ranges.last().map(|range| range.1), Some(stop));
        assert!(ranges.iter().all(|(range_start, range_stop)| range_stop > range_start));
        assert!(ranges.windows(2).all(|window| window[0].1 == window[1].0));
        assert_eq!(
            ranges.iter().map(|(range_start, range_stop)| range_stop - range_start).sum::<i64>(),
            expected_seconds
        );
    }

    #[test]
    fn creates_six_dummy_programmes_for_empty_day() {
        let mut programmes = Vec::new();
        fill_dummy_gaps(
            &mut programmes,
            &"demo".intern(),
            "UTC",
            &config(),
            Utc.with_ymd_and_hms(2026, 3, 6, 12, 0, 0).single().unwrap(),
        )
        .expect("dummy");

        assert_eq!(dummy_ranges(&programmes).len(), 6);
        assert_eq!(dummy_ranges(&programmes)[0], (ts(0, 0), ts(4, 0)));
    }

    #[test]
    fn event_inside_block_splits_dummy_ranges() {
        let mut programmes = vec![real(3, 30, 5, 0)];
        fill_dummy_gaps(
            &mut programmes,
            &"demo".intern(),
            "UTC",
            &config(),
            Utc.with_ymd_and_hms(2026, 3, 6, 12, 0, 0).single().unwrap(),
        )
        .expect("dummy");

        let ranges = dummy_ranges(&programmes);
        assert!(ranges.contains(&(ts(0, 0), ts(3, 30))));
        assert!(ranges.contains(&(ts(5, 0), ts(8, 0))));
    }

    #[test]
    fn block_boundary_event_keeps_boundary_dummy() {
        let mut programmes = vec![real(4, 0, 6, 0)];
        fill_dummy_gaps(
            &mut programmes,
            &"demo".intern(),
            "UTC",
            &config(),
            Utc.with_ymd_and_hms(2026, 3, 6, 12, 0, 0).single().unwrap(),
        )
        .expect("dummy");

        let ranges = dummy_ranges(&programmes);
        assert!(ranges.contains(&(ts(0, 0), ts(4, 0))));
        assert!(ranges.contains(&(ts(6, 0), ts(8, 0))));
    }

    #[test]
    fn adjacent_events_do_not_get_dummy_between_them() {
        let mut programmes = vec![real(10, 0, 11, 0), real(11, 0, 12, 0)];
        fill_dummy_gaps(
            &mut programmes,
            &"demo".intern(),
            "UTC",
            &config(),
            Utc.with_ymd_and_hms(2026, 3, 6, 12, 0, 0).single().unwrap(),
        )
        .expect("dummy");

        let ranges = dummy_ranges(&programmes);
        assert!(ranges.contains(&(ts(8, 0), ts(10, 0))));
        assert!(!ranges.contains(&(ts(11, 0), ts(12, 0))));
        assert!(ranges.contains(&(ts(12, 0), ts(16, 0))));
    }

    #[test]
    fn min_gap_minutes_skips_tiny_gaps() {
        let mut cfg = config();
        cfg.min_gap_minutes = 10;
        let mut programmes = vec![EpgProgramme::new(ts(0, 5), ts(4, 0), "demo".intern())];
        fill_dummy_gaps(
            &mut programmes,
            &"demo".intern(),
            "UTC",
            &cfg,
            Utc.with_ymd_and_hms(2026, 3, 6, 12, 0, 0).single().unwrap(),
        )
        .expect("dummy");

        assert!(!dummy_ranges(&programmes).contains(&(ts(0, 0), ts(0, 5))));
    }

    #[test]
    fn spring_forward_with_one_hour_blocks_covers_real_day_without_zero_length_programmes() {
        let mut cfg = config();
        cfg.block_hours = 1;
        let mut programmes = Vec::new();
        fill_dummy_gaps(
            &mut programmes,
            &"demo".intern(),
            "Europe/Berlin",
            &cfg,
            Utc.with_ymd_and_hms(2026, 3, 29, 12, 0, 0).single().unwrap(),
        )
        .expect("dummy");

        assert_eq!(dummy_ranges(&programmes).len(), 23);
        assert_dummy_covers_local_day(
            &programmes,
            "Europe/Berlin",
            NaiveDate::from_ymd_opt(2026, 3, 29).unwrap(),
            23 * 60 * 60,
        );
    }

    #[test]
    fn spring_forward_with_two_hour_blocks_covers_real_day() {
        let mut cfg = config();
        cfg.block_hours = 2;
        let mut programmes = Vec::new();
        fill_dummy_gaps(
            &mut programmes,
            &"demo".intern(),
            "Europe/Berlin",
            &cfg,
            Utc.with_ymd_and_hms(2026, 3, 29, 12, 0, 0).single().unwrap(),
        )
        .expect("dummy");

        assert_eq!(dummy_ranges(&programmes).len(), 12);
        assert_dummy_covers_local_day(
            &programmes,
            "Europe/Berlin",
            NaiveDate::from_ymd_opt(2026, 3, 29).unwrap(),
            23 * 60 * 60,
        );
    }

    #[test]
    fn fall_back_with_one_hour_blocks_covers_repeated_hour_once_without_overlap() {
        let mut cfg = config();
        cfg.block_hours = 1;
        let mut programmes = Vec::new();
        fill_dummy_gaps(
            &mut programmes,
            &"demo".intern(),
            "Europe/Berlin",
            &cfg,
            Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).single().unwrap(),
        )
        .expect("dummy");

        assert_eq!(dummy_ranges(&programmes).len(), 24);
        assert_dummy_covers_local_day(
            &programmes,
            "Europe/Berlin",
            NaiveDate::from_ymd_opt(2026, 10, 25).unwrap(),
            25 * 60 * 60,
        );
    }

    #[test]
    fn default_four_hour_blocks_remain_stable_on_spring_forward_day() {
        let mut programmes = Vec::new();
        fill_dummy_gaps(
            &mut programmes,
            &"demo".intern(),
            "Europe/Berlin",
            &config(),
            Utc.with_ymd_and_hms(2026, 3, 29, 12, 0, 0).single().unwrap(),
        )
        .expect("dummy");

        assert_eq!(dummy_ranges(&programmes).len(), 6);
        assert_dummy_covers_local_day(
            &programmes,
            "Europe/Berlin",
            NaiveDate::from_ymd_opt(2026, 3, 29).unwrap(),
            23 * 60 * 60,
        );
    }

    #[test]
    fn merge_ranges_discards_many_events_outside_dummy_window() {
        let window_start = ts(0, 0);
        let window_end = Utc.with_ymd_and_hms(2026, 3, 7, 0, 0, 0).single().unwrap().timestamp();
        let mut programmes = Vec::with_capacity(20_001);
        for index in 1..=10_000_i64 {
            let stop = window_start - index * 120;
            programmes.push(EpgProgramme::new(stop - 60, stop, "demo".intern()));
        }
        programmes.push(EpgProgramme::new(ts(10, 0), ts(11, 0), "demo".intern()));
        for index in 1..=10_000_i64 {
            let start = window_end + index * 120;
            programmes.push(EpgProgramme::new(start, start + 60, "demo".intern()));
        }
        programmes.sort_by_key(|programme| (programme.start, programme.stop));

        assert_eq!(merge_ranges_in_window(&programmes, window_start, window_end), vec![(ts(10, 0), ts(11, 0))]);
    }
}
