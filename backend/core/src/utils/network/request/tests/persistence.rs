use super::{
    atomic_download_temp_files, get_input_epg_content_as_file, make_epg_test_client, make_test_app_config,
    InputEpgFileRequest,
};
use crate::model::{Config, ConfigInput};
use url::Url;

#[tokio::test]
async fn file_url_epg_source_is_copied_to_persist_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("source.ics");
    let persist = dir.path().join("cache.ics");
    tokio::fs::write(&source, b"BEGIN:VCALENDAR\nEND:VCALENDAR\n").await.expect("write source");
    tokio::fs::write(&persist, b"old cache").await.expect("write old cache");
    let source_url = Url::from_file_path(&source).expect("file url");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();

    let result = get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: dir.path().to_string_lossy().as_ref(),
            url: source_url.as_str(),
            persist_path: &persist,
            max_bytes: Some(1024),
        },
    )
    .await
    .expect("download");

    assert_eq!(result, persist);
    assert_eq!(
        tokio::fs::read_to_string(&persist).await.expect("persisted content"),
        "BEGIN:VCALENDAR\nEND:VCALENDAR\n"
    );
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
}
