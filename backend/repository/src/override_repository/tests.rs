use super::*;
use shared::model::{ItemOverridePatch, OverrideFieldOp, OverrideItemKind};
use std::sync::atomic::Ordering;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn file_name_encoding_is_collision_free_and_reversible() -> TestResult {
    let test_names = [
        "my/provider",
        "my_provider",
        "..",
        "prov:1",
        "prov_1",
        "iptv_français_123",
        "österreich",
        "mixed/symbols:and..dots?query=1&b=2#frag",
    ];

    let mut encoded_set = std::collections::HashSet::new();

    for &name in &test_names {
        let enc = encode_override_file_name(name)?;
        assert!(encoded_set.insert(enc.clone()), "Encoding collision detected for name '{name}': encoded as '{enc}'");
        let dec = decode_override_file_name(&enc);
        assert_eq!(dec.as_deref(), Some(name), "Reversibility failed for '{name}'");
    }

    assert_ne!(encode_override_file_name("my/provider")?, encode_override_file_name("my_provider")?);
    assert_ne!(encode_override_file_name("prov:1")?, encode_override_file_name("prov_1")?);
    assert_ne!(encode_override_file_name("..")?, encode_override_file_name("__")?);

    Ok(())
}

#[tokio::test]
async fn stale_revision_is_conflict() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("test-provider".into());

    let patch = ItemOverridePatch { name: Some(OverrideFieldOp::Set("New Channel Name".into())), ..Default::default() };
    let patches = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "item-1".into(),
        patch,
        hint: "Live / Channel".into(),
    }];

    let rev1 = repo.apply_patches(&scope, &OverrideRevision::empty(), patches.clone()).await?;
    assert_ne!(rev1, OverrideRevision::empty());

    // Applying with stale empty revision must result in Conflict
    let res = repo.apply_patches(&scope, &OverrideRevision::empty(), patches).await;
    match res {
        Err(OverrideWriteError::Conflict { current }) => {
            assert_eq!(current, rev1);
        }
        other => panic!("expected Conflict error, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn external_edit_between_load_and_save_is_conflict() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("external-edit".into());

    let loaded = repo.load(&scope).await?;
    assert_eq!(loaded.revision, OverrideRevision::empty());

    // External edit writes directly to the target file
    let file_path = repo.file_path(&scope)?;
    tokio::fs::create_dir_all(file_path.parent().unwrap()).await?;
    let external_content = "schema_version: 1\nlive:\n  ext-1:\n    name: \"External\"\n";
    tokio::fs::write(&file_path, external_content).await?;
    let ext_rev = OverrideRevision::from_bytes(external_content.as_bytes());

    let patch = ItemOverridePatch { name: Some(OverrideFieldOp::Set("My Name".into())), ..Default::default() };
    let patches = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "item-1".into(),
        patch,
        hint: "Hint".into(),
    }];

    let res = repo.apply_patches(&scope, &loaded.revision, patches).await;
    match res {
        Err(OverrideWriteError::Conflict { current }) => {
            assert_eq!(current, ext_rev);
        }
        other => panic!("expected Conflict error, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn two_concurrent_saves_one_conflicts() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = Arc::new(OverrideRepository::new(dir.path()));
    let scope = OverrideScope::Input("concurrent".into());

    let base = OverrideRevision::empty();

    let patch1 = ItemOverridePatch { name: Some(OverrideFieldOp::Set("First".into())), ..Default::default() };
    let patch2 = ItemOverridePatch { name: Some(OverrideFieldOp::Set("Second".into())), ..Default::default() };

    let patches1 = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "item-1".into(),
        patch: patch1,
        hint: "Hint 1".into(),
    }];
    let patches2 = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "item-2".into(),
        patch: patch2,
        hint: "Hint 2".into(),
    }];

    let repo1 = repo.clone();
    let scope1 = scope.clone();
    let base1 = base.clone();
    let task1 = tokio::spawn(async move { repo1.apply_patches(&scope1, &base1, patches1).await });

    let repo2 = repo.clone();
    let scope2 = scope.clone();
    let base2 = base.clone();
    let task2 = tokio::spawn(async move { repo2.apply_patches(&scope2, &base2, patches2).await });

    let (res1, res2) = tokio::join!(task1, task2);
    let r1 = res1?;
    let r2 = res2?;

    let one_ok = r1.is_ok() ^ r2.is_ok();
    assert!(one_ok, "Exactly one concurrent save must succeed: r1={r1:?}, r2={r2:?}");

    let conflict_err = if r1.is_err() { r1.unwrap_err() } else { r2.unwrap_err() };
    assert!(matches!(conflict_err, OverrideWriteError::Conflict { .. }));

    Ok(())
}

#[tokio::test]
async fn save_refuses_unsupported_schema_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("unsupported-schema".into());

    let file_path = repo.file_path(&scope)?;
    tokio::fs::create_dir_all(file_path.parent().unwrap()).await?;
    let content = "schema_version: 2\nlive:\n  i1:\n    name: \"A\"\n";
    tokio::fs::write(&file_path, content).await?;
    let rev = OverrideRevision::from_bytes(content.as_bytes());

    let patch = ItemOverridePatch { name: Some(OverrideFieldOp::Set("B".into())), ..Default::default() };
    let patches = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "i1".into(),
        patch,
        hint: "Hint".into(),
    }];

    let res = repo.apply_patches(&scope, &rev, patches).await;
    match res {
        Err(OverrideWriteError::UnsupportedSchema { version }) => {
            assert_eq!(version, 2);
        }
        other => panic!("expected UnsupportedSchema error, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn save_refuses_malformed_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("malformed-save".into());

    let file_path = repo.file_path(&scope)?;
    tokio::fs::create_dir_all(file_path.parent().unwrap()).await?;
    let content = "schema_version: 1\nlive: [invalid yaml mapping structure";
    tokio::fs::write(&file_path, content).await?;
    let rev = OverrideRevision::from_bytes(content.as_bytes());

    let patch = ItemOverridePatch { name: Some(OverrideFieldOp::Set("B".into())), ..Default::default() };
    let patches = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "i1".into(),
        patch,
        hint: "Hint".into(),
    }];

    let res = repo.apply_patches(&scope, &rev, patches).await;
    assert!(matches!(res, Err(OverrideWriteError::Malformed { .. })));

    Ok(())
}

#[tokio::test]
async fn load_malformed_returns_revision() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("malformed-load".into());

    let file_path = repo.file_path(&scope)?;
    tokio::fs::create_dir_all(file_path.parent().unwrap()).await?;
    let content = "invalid: yaml: : : structure";
    tokio::fs::write(&file_path, content).await?;
    let expected_rev = OverrideRevision::from_bytes(content.as_bytes());

    let loaded = repo.load(&scope).await?;
    assert_eq!(loaded.revision, expected_rev);
    assert!(matches!(loaded.parsed, OverrideParseResult::Malformed { .. }));

    Ok(())
}

#[test]
fn file_name_edge_cases() -> TestResult {
    // Empty name rejected
    assert_eq!(encode_override_file_name(""), Err(OverrideNameError::Empty));

    // Leading and trailing spaces
    let space_name = "  leading trailing  ";
    let enc_space = encode_override_file_name(space_name)?;
    assert_eq!(enc_space, "%20%20leading%20trailing%20%20");
    assert_eq!(decode_override_file_name(&enc_space).as_deref(), Some(space_name));

    // Percent sign
    let pct_name = "100%_guaranteed";
    let enc_pct = encode_override_file_name(pct_name)?;
    assert_eq!(enc_pct, "100%25_guaranteed");
    assert_eq!(decode_override_file_name(&enc_pct).as_deref(), Some(pct_name));

    // Case variants distinct
    let enc_lower = encode_override_file_name("provider")?;
    let enc_upper = encode_override_file_name("Provider")?;
    let enc_allcaps = encode_override_file_name("PROVIDER")?;
    assert_ne!(enc_lower, enc_upper);
    assert_ne!(enc_lower, enc_allcaps);
    assert_ne!(enc_upper, enc_allcaps);
    assert_eq!(decode_override_file_name(&enc_lower).as_deref(), Some("provider"));
    assert_eq!(decode_override_file_name(&enc_upper).as_deref(), Some("Provider"));
    assert_eq!(decode_override_file_name(&enc_allcaps).as_deref(), Some("PROVIDER"));

    // Unicode NFC vs NFD distinct
    let nfc = "\u{00E9}"; // 'é' as single code point
    let nfd = "e\u{0301}"; // 'e' + combining acute accent
    let encoded_composed = encode_override_file_name(nfc)?;
    let encoded_decomposed = encode_override_file_name(nfd)?;
    assert_ne!(encoded_composed, encoded_decomposed);
    assert_eq!(decode_override_file_name(&encoded_composed).as_deref(), Some(nfc));
    assert_eq!(decode_override_file_name(&encoded_decomposed).as_deref(), Some(nfd));

    // Invalid percent sequence decodes to None
    assert_eq!(decode_override_file_name("test%"), None);
    assert_eq!(decode_override_file_name("test%1"), None);
    assert_eq!(decode_override_file_name("test%ZZ"), None);
    assert_eq!(decode_override_file_name("test%G1"), None);
    assert_eq!(decode_override_file_name("test%2G"), None);
    assert_eq!(decode_override_file_name("test%FF"), None); // Invalid UTF-8 sequence

    // Encoded name over 240 bytes rejected with clear error
    let exact_240 = "a".repeat(240);
    assert!(encode_override_file_name(&exact_240).is_ok());

    let long_241 = "a".repeat(241);
    assert_eq!(encode_override_file_name(&long_241), Err(OverrideNameError::TooLong(241)));

    let pct_81 = "%".repeat(81); // 81 * 3 = 243 bytes
    assert_eq!(encode_override_file_name(&pct_81), Err(OverrideNameError::TooLong(243)));

    Ok(())
}

#[tokio::test]
async fn empty_after_reset_deletes_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("empty-delete".into());

    let patch_set = ItemOverridePatch { name: Some(OverrideFieldOp::Set("Channel".into())), ..Default::default() };
    let patches = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "item-1".into(),
        patch: patch_set,
        hint: "Live / Channel".into(),
    }];

    let rev = repo.apply_patches(&scope, &OverrideRevision::empty(), patches).await?;
    let path = repo.file_path(&scope)?;
    assert!(path.exists(), "File should exist after initial save");

    // Reset the only entry
    let patch_reset = ItemOverridePatch { name: Some(OverrideFieldOp::Reset), ..Default::default() };
    let patches_reset = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "item-1".into(),
        patch: patch_reset,
        hint: "Live / Channel".into(),
    }];

    let rev_after_reset = repo.apply_patches(&scope, &rev, patches_reset).await?;
    assert_eq!(rev_after_reset, OverrideRevision::empty());
    assert!(!path.exists(), "File should be deleted when empty");

    // Loading deleted file returns Valid empty file and empty revision
    let loaded = repo.load(&scope).await?;
    assert_eq!(loaded.revision, OverrideRevision::empty());
    assert!(matches!(loaded.parsed, OverrideParseResult::Valid { .. }));

    Ok(())
}

#[tokio::test]
async fn write_is_atomic() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("atomic-test".into());

    // First save valid content
    let initial_patch = ItemOverridePatch { name: Some(OverrideFieldOp::Set("Initial".into())), ..Default::default() };
    let rev = repo
        .apply_patches(
            &scope,
            &OverrideRevision::empty(),
            vec![OverridePatchItem {
                kind: OverrideItemKind::Live,
                input: None,
                id: "item-1".into(),
                patch: initial_patch,
                hint: "Hint".into(),
            }],
        )
        .await?;

    let target_file_path = repo.file_path(&scope)?;
    let original_bytes = tokio::fs::read(&target_file_path).await?;

    // Now enable simulated failure via pre-rename hook
    repo.set_pre_rename_hook(|_path| async {
        Err(OverrideWriteError::Io(std::io::Error::other("simulated write failure")))
    });

    let patch_fail =
        ItemOverridePatch { name: Some(OverrideFieldOp::Set("Should Not Persist".into())), ..Default::default() };
    let res = repo
        .apply_patches(
            &scope,
            &rev,
            vec![OverridePatchItem {
                kind: OverrideItemKind::Live,
                input: None,
                id: "item-1".into(),
                patch: patch_fail,
                hint: "Hint".into(),
            }],
        )
        .await;

    assert!(res.is_err(), "Must fail when simulated failure is enabled");

    // Verify original file is untouched and no temp files remain in directory
    let current_bytes = tokio::fs::read(&target_file_path).await?;
    assert_eq!(current_bytes, original_bytes, "Original file must not be modified");

    let parent_dir = target_file_path.parent().unwrap();
    let mut entries = tokio::fs::read_dir(parent_dir).await?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        files.push(entry.file_name().to_string_lossy().to_string());
    }
    // Only the target file should exist, no .tmp or temporary files
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], target_file_path.file_name().unwrap().to_string_lossy());

    Ok(())
}

#[tokio::test]
async fn output_is_sorted_and_has_schema_version() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("sorted-test".into());

    let patch1 = ItemOverridePatch {
        title: Some(OverrideFieldOp::Set("Zebra".into())),
        chno: Some(OverrideFieldOp::Set(10)),
        name: Some(OverrideFieldOp::Set("Alpha".into())),
        ..Default::default()
    };
    let patch2 = ItemOverridePatch { name: Some(OverrideFieldOp::Set("Beta".into())), ..Default::default() };
    let patch3 = ItemOverridePatch { name: Some(OverrideFieldOp::Set("Gamma".into())), ..Default::default() };

    // Intentionally out of order: z_item, a_item, m_item
    let patches = vec![
        OverridePatchItem {
            kind: OverrideItemKind::Live,
            input: None,
            id: "z_item".into(),
            patch: patch1,
            hint: "News / Zebra".into(),
        },
        OverridePatchItem {
            kind: OverrideItemKind::Live,
            input: None,
            id: "a_item".into(),
            patch: patch2,
            hint: "News / Beta".into(),
        },
        OverridePatchItem {
            kind: OverrideItemKind::Live,
            input: None,
            id: "m_item".into(),
            patch: patch3,
            hint: "News / Gamma".into(),
        },
    ];

    repo.apply_patches(&scope, &OverrideRevision::empty(), patches).await?;

    let path = repo.file_path(&scope)?;
    let content = tokio::fs::read_to_string(&path).await?;

    // 1. schema_version: 1 is present
    assert!(content.contains("schema_version: 1"));

    // 2. IDs are in sorted order in the output: a_item < m_item < z_item
    let pos_a = content.find("a_item").expect("find a_item");
    let pos_m = content.find("m_item").expect("find m_item");
    let pos_z = content.find("z_item").expect("find z_item");
    assert!(pos_a < pos_m && pos_m < pos_z, "Entries must be sorted by key");

    // 3. For z_item: _hint comes first, then fields in alphabetical order:
    // chno before name before title
    let z_substr = &content[pos_z..];
    let pos_hint = z_substr.find("_hint").expect("find _hint");
    let pos_chno = z_substr.find("chno").expect("find chno");
    let pos_name = z_substr.find("name").expect("find name");
    let pos_title = z_substr.find("title").expect("find title");

    assert!(pos_hint < pos_chno, "_hint must appear before chno");
    assert!(pos_chno < pos_name, "chno must appear before name");
    assert!(pos_name < pos_title, "name must appear before title");

    // Also test Target scope with multiple inputs and list_files
    let target_scope = OverrideScope::Target("target-1".into());
    let t_patch = ItemOverridePatch { name: Some(OverrideFieldOp::Set("Custom Name".into())), ..Default::default() };
    repo.apply_patches(
        &target_scope,
        &OverrideRevision::empty(),
        vec![OverridePatchItem {
            kind: OverrideItemKind::Vod,
            input: Some("my-input".into()),
            id: "vod-1".into(),
            patch: t_patch,
            hint: "Movies / Custom".into(),
        }],
    )
    .await?;

    let listed = repo.list_files().await;
    assert_eq!(listed.len(), 2);
    let scopes: Vec<_> = listed.into_iter().map(|(s, _)| s).collect();
    assert!(scopes.contains(&scope));
    assert!(scopes.contains(&target_scope));

    Ok(())
}

// === NEW TESTS FOR REVIEW ROUND 2 ===

#[tokio::test]
async fn temp_file_embeds_no_target_name_and_240_byte_name_roundtrips() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());

    // Exactly 240 bytes encoded name
    let exact_240_name = "a".repeat(240);
    assert_eq!(exact_240_name.len(), 240);
    let scope = OverrideScope::Input(exact_240_name);

    let patch =
        ItemOverridePatch { name: Some(OverrideFieldOp::Set("Max Name Length Channel".into())), ..Default::default() };
    let patches = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "ch-1".into(),
        patch,
        hint: "Hint".into(),
    }];

    let rev = repo.apply_patches(&scope, &OverrideRevision::empty(), patches).await?;
    assert_ne!(rev, OverrideRevision::empty());

    let file_path = repo.file_path(&scope)?;
    assert!(file_path.exists(), "Target file must exist");

    // Check that temp file did not leave any target name in directory
    let parent = file_path.parent().unwrap();
    let mut entries = tokio::fs::read_dir(parent).await?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        files.push(entry.file_name().to_string_lossy().to_string());
    }
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], file_path.file_name().unwrap().to_string_lossy());

    // Round-trip through load
    let loaded = repo.load(&scope).await?;
    assert_eq!(loaded.revision, rev);
    let OverrideParseResult::Valid { file: ParsedOverrideFile::Input(file), .. } = loaded.parsed else {
        panic!("expected InputOverrideFile");
    };
    assert_eq!(
        file.live.get("ch-1").unwrap().name,
        shared::model::FieldDelta::Set("Max Name Length Channel".to_string())
    );

    Ok(())
}

#[tokio::test]
async fn external_edit_right_before_rename_is_conflict() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("pre-rename-conflict".into());

    let file_path = repo.file_path(&scope)?;
    tokio::fs::create_dir_all(file_path.parent().unwrap()).await?;
    let initial_content = "schema_version: 1\nlive:\n  one:\n    name: \"V1\"\n";
    tokio::fs::write(&file_path, initial_content).await?;
    let initial_rev = OverrideRevision::from_bytes(initial_content.as_bytes());

    let external_content = "schema_version: 1\nlive:\n  one:\n    name: \"Externally Injected\"\n";
    let external_rev = OverrideRevision::from_bytes(external_content.as_bytes());

    // Set hook that writes external content to the target file right after temp is written
    let ext_bytes = external_content.as_bytes().to_vec();
    repo.set_pre_rename_hook(move |path| {
        let path = path.to_path_buf();
        let ext_bytes = ext_bytes.clone();
        async move {
            tokio::fs::write(&path, ext_bytes).await.map_err(OverrideWriteError::Io)?;
            Ok(())
        }
    });

    let patch =
        ItemOverridePatch { name: Some(OverrideFieldOp::Set("Tuliprox Internal".into())), ..Default::default() };
    let patches = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "one".into(),
        patch,
        hint: "Hint".into(),
    }];

    let res = repo.apply_patches(&scope, &initial_rev, patches).await;
    match res {
        Err(OverrideWriteError::Conflict { current }) => {
            assert_eq!(current, external_rev);
        }
        other => panic!("expected Conflict error, got {other:?}"),
    }

    // Verify external content was preserved intact
    let disk_content = tokio::fs::read_to_string(&file_path).await?;
    assert_eq!(disk_content, external_content);

    Ok(())
}

#[tokio::test]
async fn rename_succeeds_even_if_directory_sync_fails() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("sync-fail".into());

    repo.fail_dir_sync.store(true, Ordering::Relaxed);

    let patch =
        ItemOverridePatch { name: Some(OverrideFieldOp::Set("Sync Fail Channel".into())), ..Default::default() };
    let patches = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "item-sync".into(),
        patch,
        hint: "Hint".into(),
    }];

    let res = repo.apply_patches(&scope, &OverrideRevision::empty(), patches).await;
    assert!(res.is_ok(), "apply_patches must succeed even if directory sync fails");

    let file_path = repo.file_path(&scope)?;
    assert!(file_path.exists());

    Ok(())
}

#[tokio::test]
async fn external_edit_right_before_delete_is_conflict() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("pre-delete-conflict".into());

    let initial_item_patch =
        ItemOverridePatch { name: Some(OverrideFieldOp::Set("Will Reset".into())), ..Default::default() };
    let rev1 = repo
        .apply_patches(
            &scope,
            &OverrideRevision::empty(),
            vec![OverridePatchItem {
                kind: OverrideItemKind::Live,
                input: None,
                id: "item-1".into(),
                patch: initial_item_patch,
                hint: "Hint".into(),
            }],
        )
        .await?;

    let file_path = repo.file_path(&scope)?;
    assert!(file_path.exists());

    // Hook simulates external modification right before delete
    let external_content = "schema_version: 1\nlive:\n  survivor:\n    name: \"External Save\"\n";
    let ext_bytes = external_content.as_bytes().to_vec();
    let expected_ext_rev = OverrideRevision::from_bytes(external_content.as_bytes());

    repo.set_pre_rename_hook(move |p| {
        let target_path = p.to_path_buf();
        let ext_bytes = ext_bytes.clone();
        async move {
            tokio::fs::write(&target_path, ext_bytes).await.map_err(OverrideWriteError::Io)?;
            Ok(())
        }
    });

    // Reset entry so that file would be deleted if not modified
    let patch_reset = ItemOverridePatch { name: Some(OverrideFieldOp::Reset), ..Default::default() };
    let patches_reset = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "item-1".into(),
        patch: patch_reset,
        hint: "Hint".into(),
    }];

    let res = repo.apply_patches(&scope, &rev1, patches_reset).await;
    match res {
        Err(OverrideWriteError::Conflict { current }) => {
            assert_eq!(current, expected_ext_rev);
        }
        other => panic!("expected Conflict error, got {other:?}"),
    }

    // Verify file still exists on disk with external content
    assert!(file_path.exists());
    let current_disk = tokio::fs::read_to_string(&file_path).await?;
    assert_eq!(current_disk, external_content);

    Ok(())
}

#[tokio::test]
async fn rewrite_preserves_invalid_type_field_on_unrelated_patch() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("preserve-invalid".into());

    let file_path = repo.file_path(&scope)?;
    tokio::fs::create_dir_all(file_path.parent().unwrap()).await?;

    // File with chno: "abc" on entry-1 and another entry-2
    let initial_yaml = r#"schema_version: 1
live:
  entry-1:
    name: "Valid Name"
    chno: "abc"
  entry-2:
    name: "Old Name"
"#;
    tokio::fs::write(&file_path, initial_yaml).await?;
    let initial_rev = OverrideRevision::from_bytes(initial_yaml.as_bytes());

    // Patch unrelated entry-2
    let patch_unrelated =
        ItemOverridePatch { name: Some(OverrideFieldOp::Set("Updated Name".into())), ..Default::default() };
    let patches = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "entry-2".into(),
        patch: patch_unrelated,
        hint: "Hint".into(),
    }];

    repo.apply_patches(&scope, &initial_rev, patches).await?;

    // Read back content from disk
    let disk_content = tokio::fs::read_to_string(&file_path).await?;
    assert!(
        disk_content.contains("chno: abc") || disk_content.contains("chno: \"abc\""),
        "Raw invalid chno value must be preserved on disk: {disk_content}"
    );
    assert!(disk_content.contains("entry-1:"));
    assert!(disk_content.contains("Updated Name"));

    Ok(())
}

#[tokio::test]
async fn list_files_canonical_filtering_and_dedup() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());

    let input_dir = dir.path().join("input");
    tokio::fs::create_dir_all(&input_dir).await?;

    // 1. Canonical uppercase encoded file
    let canonical_path = input_dir.join("prov%2FA.yml");
    tokio::fs::write(&canonical_path, "schema_version: 1\nlive: {}\n").await?;

    // 2. Non-canonical lowercase encoded file: prov%2fa.yml
    let non_canonical_path = input_dir.join("prov%2fa.yml");
    tokio::fs::write(&non_canonical_path, "schema_version: 1\nlive: {}\n").await?;

    // 3. .yaml extension (must be ignored per specification)
    let yaml_path = input_dir.join("other.yaml");
    tokio::fs::write(&yaml_path, "schema_version: 1\nlive: {}\n").await?;

    // 4. Subdirectory (must be ignored)
    let sub_path = input_dir.join("sub.yml");
    tokio::fs::create_dir(&sub_path).await?;

    let listed = repo.list_files().await;
    // Only prov%2FA.yml should be listed
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, OverrideScope::Input("prov/A".into()));
    assert_eq!(listed[0].1, canonical_path);

    Ok(())
}

#[tokio::test]
async fn validate_scope_patches_rejects_empty_id_and_whitespace() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());

    // 1. Empty item ID
    let scope = OverrideScope::Input("my-input".into());
    let patch_empty_id = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: String::new(),
        patch: ItemOverridePatch::default(),
        hint: String::new(),
    }];
    let res = repo.apply_patches(&scope, &OverrideRevision::empty(), patch_empty_id).await;
    assert!(matches!(res, Err(OverrideWriteError::InvalidPatch(_))));

    // 2. Blank item ID (spaces)
    let patch_blank_id = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "   ".into(),
        patch: ItemOverridePatch::default(),
        hint: String::new(),
    }];
    let res = repo.apply_patches(&scope, &OverrideRevision::empty(), patch_blank_id).await;
    assert!(matches!(res, Err(OverrideWriteError::InvalidPatch(_))));

    // 3. Surrounding whitespace in Input scope name
    let scope_ws = OverrideScope::Input(" my-input ".into());
    let patch_valid = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: None,
        id: "1".into(),
        patch: ItemOverridePatch::default(),
        hint: String::new(),
    }];
    let res = repo.apply_patches(&scope_ws, &OverrideRevision::empty(), patch_valid).await;
    assert!(matches!(res, Err(OverrideWriteError::InvalidPatch(_))));

    // 4. Surrounding whitespace in patch input name for Target scope
    let target_scope = OverrideScope::Target("my-target".into());
    let patch_ws_input = vec![OverridePatchItem {
        kind: OverrideItemKind::Live,
        input: Some(" bad-input ".into()),
        id: "1".into(),
        patch: ItemOverridePatch::default(),
        hint: String::new(),
    }];
    let res = repo.apply_patches(&target_scope, &OverrideRevision::empty(), patch_ws_input).await;
    assert!(matches!(res, Err(OverrideWriteError::InvalidPatch(_))));

    Ok(())
}

#[tokio::test]
async fn delete_file_checks_revision_and_is_idempotent() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = OverrideRepository::new(dir.path());
    let scope = OverrideScope::Input("orphaned-input".into());
    let path = repo.file_path(&scope)?;

    std::fs::create_dir_all(path.parent().expect("parent"))?;
    std::fs::write(&path, "schema_version: 1\nlive:\n  item-1:\n    name: Channel\n")?;
    let revision = OverrideRevision::from_bytes(&std::fs::read(&path)?);

    // A stale base revision never removes the file.
    match repo.delete_file(&scope, &OverrideRevision("stale".into())).await {
        Err(OverrideWriteError::Conflict { current }) => assert_eq!(current, revision),
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert!(path.exists(), "a rejected delete keeps the file");

    // The current revision removes the whole file, whatever it holds.
    assert_eq!(repo.delete_file(&scope, &revision).await?, OverrideRevision::empty());
    assert!(!path.exists(), "the file is gone");

    // Repeating the delete against an absent file is not an error.
    assert_eq!(repo.delete_file(&scope, &OverrideRevision::empty()).await?, OverrideRevision::empty());

    Ok(())
}
