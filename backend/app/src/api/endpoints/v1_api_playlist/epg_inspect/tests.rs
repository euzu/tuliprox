use super::{
    collect_referenced_ids, format_rfc3339, insert_canonical_id, load_epg_keys, parse_output, parse_window,
    playlist_reference_note, programmes_in_window, resolve_epg_db, scan_epg_db, summarize, truncate_chars,
    with_epg_query, ChannelSummary, ChannelsPage, EpgChannelsResponse, EpgDb, EpgIssue, EpgOutput, ReferencedIds,
    MAX_PROGRAMME_DESC_CHARS,
};
use crate::{
    api::{
        endpoints::v1_api_playlist::{
            inspect_common::{CaseInsensitiveNeedle, PageWindow},
            items::tests::{m3u_item, stream_with_skips},
        },
        model::{EpgScanCoordinator, EPG_KEY_CACHE_ENTRIES, EPG_SCAN_PERMITS},
    },
    model::{
        Config, ConfigTarget, M3uTargetOutput, StrmTargetFlagsSet, StrmTargetOutput, TargetOutput,
        XtreamTargetFlagsSet, XtreamTargetOutput,
    },
    repository::{
        get_target_storage_path, m3u_get_epg_file_path_for_target, xtream_get_epg_file_path_for_target,
        xtream_get_storage_path, BPlusTree, BPlusTreeQuery,
    },
    utils::{canonicalize_output_epg_id, canonicalize_untrusted_epg_id, EpgIdOutputCase, FileLockManager},
};
use arc_swap::ArcSwapOption;
use shared::{
    error::TuliproxError,
    foundation::Filter,
    model::{
        ConfigTargetOptions, EpgCategory, EpgChannel, EpgOutputOptions, EpgProgramme, ProcessingOrder, TargetType,
    },
    utils::Internable,
};
use std::{
    collections::HashSet,
    fmt::Write as _,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    time::Duration,
};
use strum::IntoEnumIterator;
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};

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

#[test]
fn output_names_are_pinned() {
    let expected = [
        (TargetType::Xtream, "xtream"),
        (TargetType::M3u, "m3u"),
        (TargetType::Strm, "strm"),
        (TargetType::HdHomeRun, "hdhomerun"),
    ];
    assert_eq!(expected.len(), TargetType::iter().count(), "every output variant is pinned");
    for (output, name) in expected {
        assert_eq!(output.as_str(), name);
        assert_eq!(serde_json::to_value(output).unwrap(), serde_json::json!(name), "JSON `output` field");
    }
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
    assert_eq!(target("t", vec![xtream_output()], false).epg_id_case(), EpgIdOutputCase::Preserve);
    assert_eq!(target("t", vec![xtream_output()], true).epg_id_case(), EpgIdOutputCase::LowercaseAscii);
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

#[tokio::test]
async fn referenced_ids_use_the_target_case_and_report_skipped_records() {
    let entries = || {
        vec![
            Ok(m3u_item(1, "p1", "A", "g", Some("BBC.One"))),
            Ok(m3u_item(2, "p2", "B", "g", Some(" bbc.one "))),
            Ok(m3u_item(3, "p3", "C", "g", None)),
            Ok(m3u_item(4, "p4", "D", "g", Some("  "))),
        ]
    };

    let folded =
        collect_referenced_ids(stream_with_skips(entries(), 2), EpgIdOutputCase::LowercaseAscii).await.unwrap();
    assert_eq!(folded.ids, HashSet::from(["bbc.one".intern()]));
    assert_eq!(folded.skipped_records, 2);

    let preserved = collect_referenced_ids(stream_with_skips(entries(), 0), EpgIdOutputCase::Preserve).await.unwrap();
    assert_eq!(preserved.ids, HashSet::from(["BBC.One".intern(), "bbc.one".intern()]));
    assert_eq!(preserved.skipped_records, 0, "a healthy stream skips nothing");
}

#[tokio::test]
async fn referenced_ids_abort_on_a_stream_error() {
    let entries =
        vec![Ok(m3u_item(1, "p1", "A", "g", Some("a"))), Err(TuliproxError::RepositoryM3u("boom".to_string()))];
    assert!(collect_referenced_ids(stream_with_skips(entries, 0), EpgIdOutputCase::Preserve).await.is_err());
}

#[test]
fn playlist_reference_note_distinguishes_unstored_and_unreadable_playlists() {
    let healthy = ReferencedIds::default();
    assert_eq!(playlist_reference_note("news", Some(&healthy)), None);

    let unstored = playlist_reference_note("news", None).unwrap();
    assert!(unstored.contains("'news'") && unstored.contains("not stored"), "{unstored}");

    let damaged = ReferencedIds { skipped_records: 3, ..ReferencedIds::default() };
    assert_eq!(
        playlist_reference_note("news", Some(&damaged)).unwrap(),
        "3 stored playlist record(s) could not be read, so referenced_by_playlist and the unreferenced filter may be \
         wrong for channels used only by those records."
    );
}

#[test]
fn channels_response_always_serializes_skipped_playlist_records() {
    let window = PageWindow::resolve(None, None, 50, 500);
    let page = ChannelsPage::default();

    let healthy =
        serde_json::to_value(EpgChannelsResponse::new("news", Some(TargetType::Xtream), window, &page, 0, None))
            .unwrap();
    assert_eq!(healthy["skipped_playlist_records"], 0);
    assert!(healthy.get("note").is_none());

    let note = playlist_reference_note("news", Some(&ReferencedIds { skipped_records: 4, ..ReferencedIds::default() }));
    let damaged =
        serde_json::to_value(EpgChannelsResponse::new("news", Some(TargetType::Xtream), window, &page, 4, note))
            .unwrap();
    assert_eq!(damaged["skipped_playlist_records"], 4);
    assert!(damaged["note"].as_str().unwrap().starts_with("4 stored playlist record(s) could not be read"));
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
    assert_eq!(missing.output(), Some(TargetType::Xtream));
    let note = missing.missing_note("news").unwrap();
    assert!(note.contains("xtream") && note.contains("news") && note.contains("does not mean"), "{note}");
    assert!(EpgDb::Available(TargetType::M3u, "/x".into()).missing_note("t").is_none());
}

#[tokio::test]
async fn load_epg_keys_returns_every_channel_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("epg.db");
    write_epg_db(&path, &[epg_channel("a"), epg_channel("b"), epg_channel("c")]);
    let keys = load_epg_keys(&EpgScanCoordinator::default(), &FileLockManager::default(), &path).await.unwrap();
    let mut keys: Vec<_> = keys.iter().cloned().collect();
    keys.sort();
    assert_eq!(keys, vec!["a".intern(), "b".intern(), "c".intern()]);
}

/// A channel whose encoded value is far beyond the inline limit even after compression: every programme carries
/// pseudo-random, distinct title and description text.
fn overflow_channel(id: &str) -> EpgChannel {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut random_text = || {
        let mut text = String::new();
        for _ in 0..24 {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            write!(text, "{state:016x}").unwrap();
        }
        text
    };
    let programmes = (0..200_i64)
        .map(|index| {
            let mut programme = EpgProgramme::new(index * 60, index * 60 + 60, id.intern());
            programme.title = Some(random_text().intern());
            programme.desc = Some(random_text().intern());
            programme
        })
        .collect();
    EpgChannel { id: id.intern(), title: Some(id.intern()), icon: None, programmes }
}

#[tokio::test]
async fn load_epg_keys_returns_the_ids_of_channels_stored_in_overflow_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    write_epg_db(&path, &[epg_channel("a"), overflow_channel("big"), epg_channel("z")]);
    let db_len = std::fs::metadata(&path).unwrap().len();
    assert!(db_len > 64 * 1024, "the big channel must span overflow pages, database is {db_len} bytes");

    let keys = load_epg_keys(&EpgScanCoordinator::default(), &FileLockManager::default(), &path).await.unwrap();
    let mut keys: Vec<_> = keys.iter().cloned().collect();
    keys.sort();
    assert_eq!(keys, vec!["a".intern(), "big".intern(), "z".intern()]);
}

#[tokio::test]
async fn load_epg_keys_reports_an_unreadable_database_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.db");
    let err = load_epg_keys(&EpgScanCoordinator::default(), &FileLockManager::default(), &missing).await.unwrap_err();
    assert!(err.to_string().contains("Cannot open EPG database"), "{err}");

    let garbage = dir.path().join("garbage.db");
    std::fs::write(&garbage, b"not a b+tree database at all, just bytes").unwrap();
    let result = load_epg_keys(&EpgScanCoordinator::default(), &FileLockManager::default(), &garbage).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn scan_epg_db_reports_an_unreadable_database_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.db");
    let result =
        scan_epg_db(&EpgScanCoordinator::default(), &FileLockManager::default(), &missing, 0_usize, |count, _, _| {
            *count += 1;
        })
        .await;
    assert!(result.is_err());

    let garbage = dir.path().join("garbage.db");
    std::fs::write(&garbage, b"not a b+tree database at all, just bytes").unwrap();
    let result =
        scan_epg_db(&EpgScanCoordinator::default(), &FileLockManager::default(), &garbage, 0_usize, |count, _, _| {
            *count += 1;
        })
        .await;
    assert!(result.is_err());
}

/// Upper bound for every wait on a background thread; a healthy run never gets near it.
const WAIT: Duration = Duration::from_secs(10);

/// Takes every scan slot, which succeeds only once each blocking scan has ended and released its permit.
async fn assert_scans_idle(scans: &EpgScanCoordinator) {
    let mut held = Vec::new();
    for _ in 0..EPG_SCAN_PERMITS {
        held.push(timeout(WAIT, scans.acquire()).await.expect("scan slot released").unwrap());
    }
    assert_eq!(scans.available_permits(), 0);
}

/// Starts a scan that reports its `id` on entering and then waits for `gate`.
fn gated_scan(
    scans: &Arc<EpgScanCoordinator>,
    locks: &Arc<FileLockManager>,
    path: &Path,
    entered: tokio::sync::mpsc::UnboundedSender<usize>,
    id: usize,
    gate: Receiver<()>,
) -> JoinHandle<()> {
    let (scans, locks, path) = (Arc::clone(scans), Arc::clone(locks), path.to_path_buf());
    tokio::spawn(async move {
        with_epg_query(&scans, &locks, &path, move |_, _, _| {
            entered.send(id).ok();
            gate.recv_timeout(WAIT).ok();
            Ok(())
        })
        .await
        .expect("gated scan succeeds");
    })
}

#[tokio::test]
async fn cancelled_scan_stops_and_releases_everything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    write_epg_db(&path, &[epg_channel("a")]);
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();
    let (started_tx, started_rx) = oneshot::channel();
    let (observed_tx, observed_rx) = oneshot::channel();

    let mut scan = Box::pin(with_epg_query(&scans, &locks, &path, move |_, _, cancel| {
        started_tx.send(()).ok();
        while !cancel.is_cancelled() {
            std::thread::yield_now();
        }
        observed_tx.send(()).ok();
        Ok(())
    }));
    tokio::select! {
        _ = &mut scan => panic!("the scan must keep running until it is cancelled"),
        started = timeout(WAIT, started_rx) => started.expect("scan started").unwrap(),
    }
    assert_eq!(scans.available_permits(), EPG_SCAN_PERMITS - 1);

    drop(scan);
    timeout(WAIT, observed_rx).await.expect("blocking scan saw the cancellation").unwrap();
    assert_scans_idle(&scans).await;
    drop(timeout(WAIT, locks.write_lock(&path)).await.expect("file lock released"));
}

#[tokio::test]
async fn scans_are_limited_to_the_permit_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    write_epg_db(&path, &[epg_channel("a")]);
    let scans = Arc::new(EpgScanCoordinator::default());
    let locks = Arc::new(FileLockManager::default());
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut gates = Vec::new();
    let mut tasks = Vec::new();
    for id in 0..=EPG_SCAN_PERMITS {
        let (gate_tx, gate_rx) = mpsc::channel();
        gates.push(gate_tx);
        tasks.push(gated_scan(&scans, &locks, &path, entered_tx.clone(), id, gate_rx));
    }

    for _ in 0..EPG_SCAN_PERMITS {
        timeout(WAIT, entered_rx.recv()).await.expect("scan entered").unwrap();
    }
    // A scan reports itself only after taking its slot, so with all slots taken the last one cannot be inside.
    assert_eq!(scans.available_permits(), 0);
    assert!(entered_rx.try_recv().is_err());

    gates[0].send(()).unwrap();
    timeout(WAIT, entered_rx.recv()).await.expect("waiting scan entered after a slot was freed").unwrap();

    for gate in &gates {
        gate.send(()).ok();
    }
    for task in tasks {
        timeout(WAIT, task).await.expect("scan finished").unwrap();
    }
    assert_scans_idle(&scans).await;
}

#[tokio::test]
async fn dropped_waiter_does_not_consume_a_permit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    write_epg_db(&path, &[epg_channel("a")]);
    let scans = Arc::new(EpgScanCoordinator::default());
    let locks = Arc::new(FileLockManager::default());
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut gates = Vec::new();
    let mut tasks = Vec::new();
    for id in 0..EPG_SCAN_PERMITS {
        let (gate_tx, gate_rx) = mpsc::channel();
        gates.push(gate_tx);
        tasks.push(gated_scan(&scans, &locks, &path, entered_tx.clone(), id, gate_rx));
    }
    for _ in 0..EPG_SCAN_PERMITS {
        timeout(WAIT, entered_rx.recv()).await.expect("scan entered").unwrap();
    }

    let ran = Arc::new(AtomicUsize::new(0));
    let ran_in_scan = Arc::clone(&ran);
    let mut waiter = Box::pin(with_epg_query(&scans, &locks, &path, move |_, _, _| {
        ran_in_scan.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    tokio::select! {
        biased;
        _ = &mut waiter => panic!("the waiter must wait for a slot"),
        () = std::future::ready(()) => {}
    }
    drop(waiter);

    for gate in &gates {
        gate.send(()).unwrap();
    }
    for task in tasks {
        timeout(WAIT, task).await.expect("scan finished").unwrap();
    }
    assert_scans_idle(&scans).await;
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn load_epg_keys_is_cached_until_the_database_is_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();
    write_epg_db(&path, &[epg_channel("a"), epg_channel("b")]);

    let first = load_epg_keys(&scans, &locks, &path).await.unwrap();
    let second = load_epg_keys(&scans, &locks, &path).await.unwrap();
    assert!(Arc::ptr_eq(&first, &second));

    let identity_before = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&path).unwrap().snapshot_identity();
    write_epg_db(&path, &[epg_channel("a"), epg_channel("c")]);
    let identity_after = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&path).unwrap().snapshot_identity();
    assert_ne!(identity_before, identity_after, "a rewritten database must get a new identity");

    let third = load_epg_keys(&scans, &locks, &path).await.unwrap();
    assert!(!Arc::ptr_eq(&first, &third));
    let mut keys: Vec<_> = third.iter().cloned().collect();
    keys.sort();
    assert_eq!(keys, vec!["a".intern(), "c".intern()]);
    let fourth = load_epg_keys(&scans, &locks, &path).await.unwrap();
    assert!(Arc::ptr_eq(&third, &fourth));
}

#[tokio::test]
async fn load_epg_keys_failure_is_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();

    for _ in 0..2 {
        let err = load_epg_keys(&scans, &locks, &path).await.unwrap_err();
        assert!(err.to_string().contains("Cannot open EPG database"), "{err}");
    }
    assert_eq!(scans.key_cache().len(), 0);

    write_epg_db(&path, &[epg_channel("a")]);
    let keys = load_epg_keys(&scans, &locks, &path).await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(scans.key_cache().len(), 1);
}

#[tokio::test]
async fn cached_key_set_eviction_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();

    let mut last = None;
    for index in 0..EPG_KEY_CACHE_ENTRIES + 3 {
        let path = dir.path().join(format!("epg-{index}.db"));
        write_epg_db(&path, &[epg_channel("a")]);
        load_epg_keys(&scans, &locks, &path).await.unwrap();
        assert!(scans.key_cache().len() <= EPG_KEY_CACHE_ENTRIES);
        last = Some(path);
    }
    assert_eq!(scans.key_cache().len(), EPG_KEY_CACHE_ENTRIES);

    let last = last.unwrap();
    let cached = load_epg_keys(&scans, &locks, &last).await.unwrap();
    let again = load_epg_keys(&scans, &locks, &last).await.unwrap();
    assert!(Arc::ptr_eq(&cached, &again), "the most recently cached entry is never the one evicted");
}

#[tokio::test]
async fn eviction_drops_the_least_recently_used_entry() {
    let dir = tempfile::tempdir().unwrap();
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();
    let paths: Vec<_> = (0..=EPG_KEY_CACHE_ENTRIES).map(|index| dir.path().join(format!("epg-{index}.db"))).collect();
    for path in &paths[..EPG_KEY_CACHE_ENTRIES] {
        write_epg_db(path, &[epg_channel("a")]);
        load_epg_keys(&scans, &locks, path).await.unwrap();
    }
    // The oldest entry is read again, so the second oldest becomes the eviction candidate.
    load_epg_keys(&scans, &locks, &paths[0]).await.unwrap();

    write_epg_db(&paths[EPG_KEY_CACHE_ENTRIES], &[epg_channel("a")]);
    load_epg_keys(&scans, &locks, &paths[EPG_KEY_CACHE_ENTRIES]).await.unwrap();

    let cache = scans.key_cache();
    assert_eq!(cache.len(), EPG_KEY_CACHE_ENTRIES);
    assert!(cache.contains(&paths[0]), "a recently read entry survives");
    assert!(!cache.contains(&paths[1]), "the least recently used entry is evicted");
    assert!(cache.contains(&paths[EPG_KEY_CACHE_ENTRIES]));
}

/// Takes every scan slot through the coordinator and keeps them until dropped.
async fn hold_all_scan_slots(scans: &EpgScanCoordinator) -> Vec<tokio::sync::OwnedSemaphorePermit> {
    let mut held = Vec::new();
    for _ in 0..EPG_SCAN_PERMITS {
        held.push(timeout(WAIT, scans.acquire()).await.expect("scan slot free").unwrap());
    }
    assert_eq!(scans.available_permits(), 0);
    held
}

#[tokio::test]
async fn cache_hit_skips_the_scan_slot_and_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    write_epg_db(&path, &[epg_channel("a"), epg_channel("b")]);
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();
    let first = load_epg_keys(&scans, &locks, &path).await.unwrap();

    let _slots = hold_all_scan_slots(&scans).await;
    let _write_lock = timeout(WAIT, locks.write_lock(&path)).await.expect("file lock free");
    let second = timeout(Duration::from_secs(2), load_epg_keys(&scans, &locks, &path))
        .await
        .expect("a cache hit needs neither a scan slot nor the file lock")
        .unwrap();
    assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn rewritten_database_misses_the_fast_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    write_epg_db(&path, &[epg_channel("a"), epg_channel("b")]);
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();
    let first = load_epg_keys(&scans, &locks, &path).await.unwrap();

    write_epg_db(&path, &[epg_channel("c"), epg_channel("d")]);
    let second = load_epg_keys(&scans, &locks, &path).await.unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    let mut keys: Vec<_> = second.iter().cloned().collect();
    keys.sort();
    assert_eq!(keys, vec!["c".intern(), "d".intern()]);

    // The rewrite is cached under its own stamp, so the next call is a hit again.
    let _slots = hold_all_scan_slots(&scans).await;
    let third =
        timeout(Duration::from_secs(2), load_epg_keys(&scans, &locks, &path)).await.expect("fast path").unwrap();
    assert!(Arc::ptr_eq(&second, &third));
}

#[tokio::test]
async fn same_snapshot_with_a_new_file_stamp_is_refreshed_without_a_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    write_epg_db(&path, &[epg_channel("a")]);
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();
    let first = load_epg_keys(&scans, &locks, &path).await.unwrap();

    // A byte-identical copy published by rename has the same header identity but another inode.
    let copy = dir.path().join("copy.db");
    std::fs::copy(&path, &copy).unwrap();
    std::fs::rename(&copy, &path).unwrap();

    let second = load_epg_keys(&scans, &locks, &path).await.unwrap();
    assert!(Arc::ptr_eq(&first, &second), "the header identity is unchanged, so the set is reused");

    let _slots = hold_all_scan_slots(&scans).await;
    let third =
        timeout(Duration::from_secs(2), load_epg_keys(&scans, &locks, &path)).await.expect("fast path").unwrap();
    assert!(Arc::ptr_eq(&first, &third), "the refreshed stamp makes the next lookup a fast hit");
}

#[tokio::test]
async fn deleted_database_is_not_served_from_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    write_epg_db(&path, &[epg_channel("a")]);
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();
    load_epg_keys(&scans, &locks, &path).await.unwrap();

    std::fs::remove_file(&path).unwrap();
    let err = load_epg_keys(&scans, &locks, &path).await.unwrap_err();
    assert!(err.to_string().contains("Cannot open EPG database"), "{err}");
}

#[tokio::test]
async fn scan_epg_db_stops_on_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg.db");
    let channels: Vec<_> = (0..5).map(|index| epg_channel(&format!("c{index}"))).collect();
    write_epg_db(&path, &channels);
    let scans = EpgScanCoordinator::default();
    let locks = FileLockManager::default();
    let visited = Arc::new(AtomicUsize::new(0));
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();

    let visited_in_scan = Arc::clone(&visited);
    let mut scan = Box::pin(scan_epg_db(&scans, &locks, &path, (), move |(), _, _| {
        visited_in_scan.fetch_add(1, Ordering::SeqCst);
        entered_tx.send(()).ok();
        gate_rx.recv_timeout(WAIT).ok();
    }));
    tokio::select! {
        _ = &mut scan => panic!("the scan is blocked on the first channel"),
        entered = timeout(WAIT, entered_rx.recv()) => entered.expect("scan visited a channel").unwrap(),
    }

    drop(scan);
    gate_tx.send(()).unwrap();
    assert_scans_idle(&scans).await;
    drop(timeout(WAIT, locks.write_lock(&path)).await.expect("file lock released"));
    assert_eq!(visited.load(Ordering::SeqCst), 1, "no channel is visited after the caller is gone");
}
