use super::{
    format_rfc3339, insert_canonical_id, load_epg_keys, parse_output, parse_window, programmes_in_window,
    resolve_epg_db, scan_epg_db, summarize, target_epg_case, truncate_chars, ChannelSummary, EpgDb, EpgIssue,
    EpgOutput, MAX_PROGRAMME_DESC_CHARS,
};
use crate::{
    api::endpoints::v1_api_playlist::inspect_common::CaseInsensitiveNeedle,
    model::{
        Config, ConfigTarget, M3uTargetOutput, StrmTargetFlagsSet, StrmTargetOutput, TargetOutput,
        XtreamTargetFlagsSet, XtreamTargetOutput,
    },
    repository::{
        get_target_storage_path, m3u_get_epg_file_path_for_target, xtream_get_epg_file_path_for_target,
        xtream_get_storage_path, BPlusTree,
    },
    utils::{canonicalize_output_epg_id, canonicalize_untrusted_epg_id, EpgIdOutputCase, FileLockManager},
};
use arc_swap::ArcSwapOption;
use shared::{
    foundation::Filter,
    model::{
        ConfigTargetOptions, EpgCategory, EpgChannel, EpgOutputOptions, EpgProgramme, ProcessingOrder, TargetType,
    },
    utils::Internable,
};
use std::{collections::HashSet, path::Path, sync::Arc};

const NOW: i64 = 1_000_000;

fn programme(start: i64, stop: i64) -> EpgProgramme { EpgProgramme::new(start, stop, "c".intern()) }

fn channel(title: Option<&str>, programmes: Vec<EpgProgramme>) -> EpgChannel {
    EpgChannel { id: "c".intern(), title: title.map(Internable::intern), icon: None, programmes }
}

fn summary(channel: &EpgChannel, referenced: Option<bool>) -> ChannelSummary {
    summarize("c".intern(), channel, NOW, referenced)
}

fn issues_of(summary: &ChannelSummary) -> Vec<EpgIssue> {
    EpgIssue::ALL.iter().copied().filter(|issue| summary.has_issue(*issue, NOW)).collect()
}

#[test]
fn healthy_channel_has_no_issue() {
    let healthy = channel(Some("Title"), vec![programme(NOW - 100, NOW + 100), programme(NOW + 100, NOW + 200)]);
    let summary = summary(&healthy, Some(true));
    assert_eq!(issues_of(&summary), Vec::<EpgIssue>::new());
    assert!(summary.covers_now);
    assert_eq!(summary.programme_count, 2);
    assert_eq!(summary.first_start, Some(NOW - 100));
    assert_eq!(summary.last_stop, Some(NOW + 200));
}

#[test]
fn empty_channel_has_no_programmes_and_is_not_current_but_not_expired() {
    let summary = summary(&channel(Some("Title"), Vec::new()), Some(true));
    assert_eq!(issues_of(&summary), vec![EpgIssue::NoProgrammes, EpgIssue::NotCurrent]);
    assert_eq!((summary.first_start, summary.last_stop), (None, None));
}

#[test]
fn expired_uses_the_last_stop_strictly_before_now() {
    let past = summary(&channel(Some("T"), vec![programme(NOW - 300, NOW - 200), programme(NOW - 200, NOW - 1)]), None);
    assert_eq!(issues_of(&past), vec![EpgIssue::Expired, EpgIssue::NotCurrent]);

    let ends_now = summary(&channel(Some("T"), vec![programme(NOW - 100, NOW)]), None);
    assert!(!ends_now.has_issue(EpgIssue::Expired, NOW), "stop == now is not expired");
    assert!(ends_now.has_issue(EpgIssue::NotCurrent, NOW), "but the programme no longer covers now");
}

#[test]
fn expired_is_decided_by_the_latest_stop_even_when_programmes_are_unordered() {
    let unordered = channel(Some("T"), vec![programme(0, NOW + 500), programme(10, 20)]);
    assert!(!summary(&unordered, None).has_issue(EpgIssue::Expired, NOW));
}

#[test]
fn covers_now_boundaries_are_start_inclusive_and_stop_exclusive() {
    for (start, stop, expected) in [
        (NOW, NOW + 10, true),
        (NOW - 10, NOW + 1, true),
        (NOW - 10, NOW, false),
        (NOW + 1, NOW + 10, false),
        (NOW - 10, NOW - 1, false),
        (NOW, NOW, false),
    ] {
        let summary = summary(&channel(Some("T"), vec![programme(start, stop)]), None);
        assert_eq!(summary.covers_now, expected, "[{start}, {stop}) at {NOW}");
    }
}

#[test]
fn future_only_channel_is_not_current_but_not_expired() {
    let future = summary(&channel(Some("T"), vec![programme(NOW + 10, NOW + 20)]), None);
    assert_eq!(issues_of(&future), vec![EpgIssue::NotCurrent]);
}

#[test]
fn no_title_covers_missing_and_blank_titles() {
    let programmes = || vec![programme(NOW - 1, NOW + 1)];
    assert_eq!(issues_of(&summary(&channel(None, programmes()), None)), vec![EpgIssue::NoTitle]);
    assert_eq!(issues_of(&summary(&channel(Some("   "), programmes()), None)), vec![EpgIssue::NoTitle]);
    assert_eq!(issues_of(&summary(&channel(Some("x"), programmes()), None)), Vec::<EpgIssue>::new());
}

#[test]
fn unreferenced_needs_a_known_playlist() {
    let programmes = || vec![programme(NOW - 1, NOW + 1)];
    let unreferenced = summary(&channel(Some("x"), programmes()), Some(false));
    assert_eq!(issues_of(&unreferenced), vec![EpgIssue::Unreferenced]);
    let unknown = summary(&channel(Some("x"), programmes()), None);
    assert!(issues_of(&unknown).is_empty(), "no stored playlist: the reference is unknown, not missing");
    assert_eq!(issues_of(&summary(&channel(Some("x"), programmes()), Some(true))), Vec::<EpgIssue>::new());
}

#[test]
fn issue_filter_combines_with_or_and_empty_means_all() {
    let summary = summary(&channel(None, vec![programme(NOW - 1, NOW + 1)]), Some(true));
    assert!(summary.has_any_issue(&[], NOW));
    assert!(summary.has_any_issue(&[EpgIssue::Expired, EpgIssue::NoTitle], NOW));
    assert!(!summary.has_any_issue(&[EpgIssue::Expired, EpgIssue::NoProgrammes], NOW));
}

#[test]
fn search_matches_id_or_title_case_insensitively() {
    let summary = summarize("bbc.one".intern(), &channel(Some("BBC One HD"), Vec::new()), NOW, None);
    for (text, expected) in [("BBC", true), ("one hd", true), ("bbc.o", true), ("cnn", false)] {
        let needle = CaseInsensitiveNeedle::new(Some(text));
        assert_eq!(summary.matches_search(needle.as_ref()), expected, "{text}");
    }
    assert!(summary.matches_search(None));
}

#[test]
fn has_icon_ignores_blank_icons() {
    let mut with_icon = channel(Some("x"), Vec::new());
    with_icon.icon = Some("http://img/i.png".intern());
    assert!(summary(&with_icon, None).has_icon);
    with_icon.icon = Some("  ".intern());
    assert!(!summary(&with_icon, None).has_icon);
    with_icon.icon = None;
    assert!(!summary(&with_icon, None).has_icon);
}

#[test]
fn description_is_truncated_at_a_char_boundary() {
    let long = "ä".repeat(MAX_PROGRAMME_DESC_CHARS + 25);
    let truncated = truncate_chars(&long, MAX_PROGRAMME_DESC_CHARS);
    assert_eq!(truncated.chars().count(), MAX_PROGRAMME_DESC_CHARS + 1);
    assert!(truncated.ends_with('…'));
    assert!(truncated.starts_with("äää"));

    let emoji = "😀日本語".repeat(300);
    let truncated = truncate_chars(&emoji, MAX_PROGRAMME_DESC_CHARS);
    assert_eq!(truncated.chars().count(), MAX_PROGRAMME_DESC_CHARS + 1);

    let exact = "é".repeat(MAX_PROGRAMME_DESC_CHARS);
    assert_eq!(truncate_chars(&exact, MAX_PROGRAMME_DESC_CHARS), exact);
    assert_eq!(truncate_chars("short", MAX_PROGRAMME_DESC_CHARS), "short");
    assert_eq!(truncate_chars("", MAX_PROGRAMME_DESC_CHARS), "");
}

#[test]
fn timestamps_are_rendered_as_rfc3339_utc() {
    assert_eq!(format_rfc3339(0).as_deref(), Some("1970-01-01T00:00:00Z"));
    assert_eq!(format_rfc3339(1_700_000_000).as_deref(), Some("2023-11-14T22:13:20Z"));
}

#[test]
fn window_defaults_to_now_plus_24_hours() {
    assert_eq!(parse_window(None, None, NOW).unwrap(), (NOW, NOW + 24 * 3600));
    assert_eq!(parse_window(Some(""), Some(" "), NOW).unwrap(), (NOW, NOW + 24 * 3600));
    let from_only = parse_window(Some("1970-01-02T00:00:00Z"), None, NOW).unwrap();
    assert_eq!(from_only, (86_400, 2 * 86_400));
    let explicit = parse_window(Some("1970-01-01T00:00:00Z"), Some("1970-01-01T02:00:00+01:00"), NOW).unwrap();
    assert_eq!(explicit, (0, 3600), "offsets are converted to UTC");
}

#[test]
fn window_rejects_bad_and_inverted_ranges() {
    assert!(parse_window(Some("yesterday"), None, NOW).unwrap_err().message().contains("Invalid from"));
    assert!(parse_window(None, Some("2026-13-01"), NOW).unwrap_err().message().contains("Invalid to"));
    let inverted = parse_window(Some("2026-01-02T00:00:00Z"), Some("2026-01-01T00:00:00Z"), NOW);
    assert!(inverted.unwrap_err().message().contains("after"));
    let empty = parse_window(Some("2026-01-01T00:00:00Z"), Some("2026-01-01T00:00:00Z"), NOW);
    assert!(empty.is_err());
}

#[test]
fn programmes_in_window_selects_overlapping_ones_and_reports_the_total() {
    let mut with_meta = programme(100, 200);
    with_meta.title = Some("Show".intern());
    with_meta.desc = Some("ä".repeat(600).intern());
    with_meta.is_live = true;
    with_meta.categories = vec![EpgCategory { value: "News".intern(), lang: None }];
    let channel = channel(
        Some("T"),
        vec![programme(0, 100), with_meta, programme(200, 300), programme(300, 400), programme(400, 500)],
    );

    let (total, rows) = programmes_in_window(&channel, 100, 400, 10);
    assert_eq!(total, 3, "[0,100) ends at the window start, [400,500) starts at the window end");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].title.as_deref(), Some("Show"));
    assert_eq!(rows[0].categories, vec!["News".to_string()]);
    assert!(rows[0].is_live);
    assert_eq!(rows[0].desc.as_deref().map(|d| d.chars().count()), Some(MAX_PROGRAMME_DESC_CHARS + 1));
    assert_eq!(rows[0].start.as_deref(), Some("1970-01-01T00:01:40Z"));

    let (total, rows) = programmes_in_window(&channel, 100, 400, 2);
    assert_eq!((total, rows.len()), (3, 2), "total counts beyond the limit");
    let (total, rows) = programmes_in_window(&channel, 1000, 2000, 2);
    assert_eq!((total, rows.len()), (0, 0));
}

#[test]
fn issue_and_output_name_lists_are_consistent() {
    assert_eq!(EpgIssue::ALL.len(), EpgIssue::NAMES.len());
    assert_eq!(EpgIssue::NAMES, &["no_programmes", "expired", "not_current", "no_title", "unreferenced"]);
    assert_eq!(EpgOutput::NAMES, &["xtream", "m3u"]);
    assert_eq!(parse_output(Some("m3u")).unwrap(), Some(EpgOutput::M3u));
    assert_eq!(parse_output(Some(" ")).unwrap(), None);
    assert_eq!(parse_output(None).unwrap(), None);
    assert!(parse_output(Some("strm")).unwrap_err().message().contains("xtream, m3u"));
}

fn target(name: &str, outputs: Vec<TargetOutput>, lowercase_ids: bool) -> ConfigTarget {
    ConfigTarget {
        curation: None,
        id: 1,
        enabled: true,
        name: name.to_string(),
        options: lowercase_ids.then(|| ConfigTargetOptions {
            epg_output: EpgOutputOptions { lowercase_ids: true, ..EpgOutputOptions::default() },
            ..ConfigTargetOptions::default()
        }),
        sort: None,
        filter: Filter::default().into(),
        output: outputs,
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::new(None)),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    }
}

fn xtream_output() -> TargetOutput {
    TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None })
}

fn m3u_output() -> TargetOutput {
    TargetOutput::M3u(M3uTargetOutput {
        filename: None,
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: None,
    })
}

fn strm_output() -> TargetOutput {
    TargetOutput::Strm(StrmTargetOutput {
        directory: "/tmp/strm".to_string(),
        username: None,
        style: shared::model::StrmExportStyle::default(),
        flags: StrmTargetFlagsSet::default(),
        strm_props: None,
        filter: None,
        probe_probe_size_bytes: None,
        probe_analyze_duration: None,
    })
}

fn write_epg_db(path: &Path, channels: &[EpgChannel]) {
    std::fs::create_dir_all(path.parent().expect("epg path has a parent")).expect("storage dir");
    let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
    for channel in channels {
        tree.insert(Arc::clone(&channel.id), channel.clone());
    }
    tree.store(path).expect("EPG DB written");
}

fn epg_channel(id: &str) -> EpgChannel {
    EpgChannel {
        id: id.intern(),
        title: Some(id.intern()),
        icon: None,
        programmes: vec![EpgProgramme::new(0, 10, id.intern())],
    }
}

fn config_with_storage(dir: &tempfile::TempDir) -> Config {
    Config { storage_dir: dir.path().to_string_lossy().into_owned(), ..Config::default() }
}

#[test]
fn epg_case_follows_the_target_lowercase_ids_option() {
    assert_eq!(target_epg_case(&target("t", vec![xtream_output()], false)), EpgIdOutputCase::Preserve);
    assert_eq!(target_epg_case(&target("t", vec![xtream_output()], true)), EpgIdOutputCase::LowercaseAscii);
}

#[test]
fn request_id_canonicalization_matches_the_xmltv_output_canonicalization() {
    for case in [EpgIdOutputCase::Preserve, EpgIdOutputCase::LowercaseAscii] {
        for id in ["BBC.One", "bbc.one", "ÄRD.de", "", "Mixed.Case.ID"] {
            let arc: Arc<str> = id.intern();
            assert_eq!(canonicalize_untrusted_epg_id(id, case), canonicalize_output_epg_id(&arc, case), "{id}");
        }
    }
}

#[test]
fn insert_canonical_id_folds_like_the_epg_keys_and_deduplicates() {
    let mut folded = HashSet::new();
    for id in ["BBC.One", "bbc.one", "Sky.Sports"] {
        insert_canonical_id(&mut folded, id, EpgIdOutputCase::LowercaseAscii);
    }
    let mut folded: Vec<_> = folded.into_iter().collect();
    folded.sort();
    assert_eq!(folded, vec!["bbc.one".intern(), "sky.sports".intern()]);

    let mut preserved = HashSet::new();
    for id in ["BBC.One", "bbc.one", "BBC.One"] {
        insert_canonical_id(&mut preserved, id, EpgIdOutputCase::Preserve);
    }
    assert_eq!(preserved.len(), 2);
}

#[test]
fn resolve_epg_db_picks_the_first_output_with_a_database_and_explains_absence() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_storage(&dir);
    let both = target("both", vec![xtream_output(), m3u_output()], false);

    // No database anywhere: the first EPG-capable output is reported as missing.
    assert_eq!(resolve_epg_db(&config, &both, None).unwrap(), EpgDb::Missing(TargetType::Xtream));
    assert_eq!(resolve_epg_db(&config, &both, Some(EpgOutput::M3u)).unwrap(), EpgDb::Missing(TargetType::M3u));

    // Only the m3u database exists: the default falls through to it, like `get_epg_path_for_target`.
    let m3u_path = m3u_get_epg_file_path_for_target(&get_target_storage_path(&config, "both").unwrap());
    write_epg_db(&m3u_path, &[epg_channel("a")]);
    assert_eq!(resolve_epg_db(&config, &both, None).unwrap(), EpgDb::Available(TargetType::M3u, m3u_path.clone()));
    assert_eq!(resolve_epg_db(&config, &both, Some(EpgOutput::Xtream)).unwrap(), EpgDb::Missing(TargetType::Xtream));

    // The xtream database wins by configuration order once it exists.
    let xtream_path = xtream_get_epg_file_path_for_target(&xtream_get_storage_path(&config, "both").unwrap());
    write_epg_db(&xtream_path, &[epg_channel("a")]);
    assert_eq!(resolve_epg_db(&config, &both, None).unwrap(), EpgDb::Available(TargetType::Xtream, xtream_path));
}

#[test]
fn resolve_epg_db_rejects_an_output_the_target_does_not_have_and_handles_epg_less_targets() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_storage(&dir);
    let xtream_only = target("x", vec![xtream_output()], false);
    let err = resolve_epg_db(&config, &xtream_only, Some(EpgOutput::M3u)).unwrap_err();
    assert!(err.message().contains("no m3u output"), "{}", err.message());

    let strm_only = target("s", vec![strm_output()], false);
    let db = resolve_epg_db(&config, &strm_only, None).unwrap();
    assert_eq!(db, EpgDb::NoEpgOutput);
    assert_eq!(db.output(), None);
    assert!(db.missing_note("s").unwrap().contains("no output with EPG support"));
}

#[test]
fn missing_database_note_names_the_output_and_never_reads_as_healthy() {
    let missing = EpgDb::Missing(TargetType::Xtream);
    assert_eq!(missing.output(), Some("xtream"));
    let note = missing.missing_note("news").unwrap();
    assert!(note.contains("xtream") && note.contains("news") && note.contains("does not mean"), "{note}");
    assert!(EpgDb::Available(TargetType::M3u, "/x".into()).missing_note("t").is_none());
}

#[tokio::test]
async fn load_epg_keys_returns_every_channel_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("epg.db");
    write_epg_db(&path, &[epg_channel("a"), epg_channel("b"), epg_channel("c")]);
    let keys = load_epg_keys(&FileLockManager::default(), &path).await.unwrap();
    let mut keys: Vec<_> = keys.into_iter().collect();
    keys.sort();
    assert_eq!(keys, vec!["a".intern(), "b".intern(), "c".intern()]);
}

#[tokio::test]
async fn scan_epg_db_reports_an_unreadable_database_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.db");
    let result = scan_epg_db(&FileLockManager::default(), &missing, 0_usize, |count, _, _| *count += 1).await;
    assert!(result.is_err());

    let garbage = dir.path().join("garbage.db");
    std::fs::write(&garbage, b"not a b+tree database at all, just bytes").unwrap();
    let result = scan_epg_db(&FileLockManager::default(), &garbage, 0_usize, |count, _, _| *count += 1).await;
    assert!(result.is_err());
}
