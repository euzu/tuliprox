use super::{OverrideRunDiagnostics, OverrideStatusStore, SkippedOverrideFile};
use crate::override_repository::{OverrideRevision, OverrideScope};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn diagnostics() -> OverrideRunDiagnostics { OverrideRunDiagnostics::new(Vec::new(), Vec::new(), 0) }

fn revision(bytes: &[u8]) -> OverrideRevision { OverrideRevision::from_bytes(bytes) }

#[tokio::test]
async fn failed_run_keeps_pending() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = OverrideStatusStore::new(dir.path());
    let scope = OverrideScope::Input("provider-a".into());
    let current = revision(b"version: 1\nsections:\n  live: {}\n");

    assert_eq!(store.applied_revision("t1", &scope).await, None);
    assert!(store.is_pending("t1", &scope, &current).await);

    // A missing override file has no revision and no record: nothing is pending.
    assert!(!store.is_pending("t1", &scope, &OverrideRevision::empty()).await);

    Ok(())
}

#[tokio::test]
async fn save_during_run_stays_pending() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = OverrideStatusStore::new(dir.path());
    let scope = OverrideScope::Input("provider-a".into());

    // Revision loaded at the start of the run.
    let loaded_at_start = revision(b"loaded-at-start\n");
    // Revision written by a UI save while the run was still in flight.
    let saved_during_run = revision(b"saved-during-run\n");

    store.record_success("t1", vec![(scope.clone(), loaded_at_start.clone())], diagnostics()).await?;

    assert_eq!(store.applied_revision("t1", &scope).await, Some(loaded_at_start.clone()));
    assert!(store.is_pending("t1", &scope, &saved_during_run).await);
    assert!(!store.is_pending("t1", &scope, &loaded_at_start).await);

    Ok(())
}

#[tokio::test]
async fn skipped_malformed_file_not_applied() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = OverrideStatusStore::new(dir.path());
    let good = OverrideScope::Input("good-provider".into());
    let malformed = OverrideScope::Input("malformed-provider".into());
    let applied = revision(b"good-provider\n");
    let on_disk = revision(b"malformed-provider-on-disk\n");

    let run = OverrideRunDiagnostics::new(
        vec!["good-provider.yml".into()],
        vec![SkippedOverrideFile::malformed("malformed-provider.yml")],
        1,
    );
    store.record_success("t1", vec![(good.clone(), applied.clone())], run).await?;

    assert_eq!(store.applied_revision("t1", &good).await, Some(applied));
    assert_eq!(store.applied_revision("t1", &malformed).await, None);
    assert!(store.is_pending("t1", &malformed, &on_disk).await);

    Ok(())
}

#[tokio::test]
async fn input_override_status_per_target() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = OverrideStatusStore::new(dir.path());
    let scope = OverrideScope::Input("shared-provider".into());
    let applied = revision(b"shared-provider\n");

    store.record_success("target-a", vec![(scope.clone(), applied.clone())], diagnostics()).await?;

    assert_eq!(store.applied_revision("target-a", &scope).await, Some(applied.clone()));
    assert!(!store.is_pending("target-a", &scope, &applied).await);

    assert_eq!(store.applied_revision("target-b", &scope).await, None);
    assert!(store.is_pending("target-b", &scope, &applied).await);

    Ok(())
}

#[tokio::test]
async fn status_write_failure_returns_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    // A regular file where the storage directory is expected: every write fails.
    let storage = dir.path().join("storage");
    std::fs::write(&storage, b"not a directory")?;

    let store = OverrideStatusStore::new(&storage);
    let scope = OverrideScope::Input("provider-a".into());
    let applied = revision(b"provider-a\n");

    let result = store.record_success("t1", vec![(scope.clone(), applied.clone())], diagnostics()).await;
    assert!(result.is_err(), "unwritable storage must report an error");

    // Nothing was recorded, so the revision stays pending.
    assert_eq!(store.applied_revision("t1", &scope).await, None);
    assert!(store.is_pending("t1", &scope, &applied).await);

    Ok(())
}

#[tokio::test]
async fn corrupt_status_file_treated_as_empty() -> TestResult {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("override_status.json"), b"{not json at all")?;

    let store = OverrideStatusStore::new(dir.path());
    let scope = OverrideScope::Target("target-a".into());
    let current = revision(b"target-a\n");

    assert_eq!(store.applied_revision("target-a", &scope).await, None);
    assert!(!store.is_pending("target-a", &scope, &OverrideRevision::empty()).await);
    assert!(store.is_pending("target-a", &scope, &current).await);

    // A successful run replaces the corrupt file.
    store.record_success("target-a", vec![(scope.clone(), current.clone())], diagnostics()).await?;
    let reloaded = OverrideStatusStore::new(dir.path());
    assert_eq!(reloaded.applied_revision("target-a", &scope).await, Some(current));

    Ok(())
}

#[tokio::test]
async fn roundtrip_persists_across_instances() -> TestResult {
    let dir = tempfile::tempdir()?;
    let scope = OverrideScope::Input("provider-a".into());
    let applied = revision(b"provider-a-v1\n");
    let run = OverrideRunDiagnostics::new(
        vec!["provider-a.yml".into()],
        vec![SkippedOverrideFile::unsupported("future-provider.yml")],
        3,
    );

    {
        let store = OverrideStatusStore::new(dir.path());
        store.record_success("t1", vec![(scope.clone(), applied.clone())], run).await?;
    }

    let reloaded = OverrideStatusStore::new(dir.path());
    assert_eq!(reloaded.applied_revision("t1", &scope).await, Some(applied.clone()));
    assert!(!reloaded.is_pending("t1", &scope, &applied).await);
    assert!(reloaded.is_pending("t1", &scope, &revision(b"provider-a-v2\n")).await);

    let raw = std::fs::read_to_string(dir.path().join("override_status.json"))?;
    let json: serde_json::Value = serde_json::from_str(&raw)?;
    assert_eq!(json["targets"]["t1"]["applied"][0]["scope"], serde_json::json!({ "input": "provider-a" }));
    assert_eq!(json["targets"]["t1"]["applied"][0]["revision"], serde_json::json!(applied.as_str()));
    assert_eq!(json["targets"]["t1"]["diagnostics"]["ambiguous_keys"], 3);
    assert_eq!(json["targets"]["t1"]["diagnostics"]["applied_files"][0], "provider-a.yml");
    assert_eq!(json["targets"]["t1"]["diagnostics"]["skipped_files"][0]["name"], "future-provider.yml");
    assert_eq!(json["targets"]["t1"]["diagnostics"]["skipped_files"][0]["reason"], "unsupported");
    assert!(json["targets"]["t1"]["diagnostics"]["finished_at"].as_u64().is_some());

    Ok(())
}

#[tokio::test]
async fn target_scope_roundtrips() -> TestResult {
    let dir = tempfile::tempdir()?;
    let scope = OverrideScope::Target("target-a".into());
    let applied = revision(b"target-a\n");

    {
        let store = OverrideStatusStore::new(dir.path());
        store.record_success("t1", vec![(scope.clone(), applied.clone())], diagnostics()).await?;
    }

    let reloaded = OverrideStatusStore::new(dir.path());
    assert_eq!(reloaded.applied_revision("t1", &scope).await, Some(applied.clone()));
    assert_eq!(reloaded.applied_revision("t1", &OverrideScope::Input("target-a".into())).await, None);

    let raw = std::fs::read_to_string(dir.path().join("override_status.json"))?;
    let json: serde_json::Value = serde_json::from_str(&raw)?;
    assert_eq!(json["targets"]["t1"]["applied"][0]["scope"], serde_json::json!({ "target": "target-a" }));

    Ok(())
}
