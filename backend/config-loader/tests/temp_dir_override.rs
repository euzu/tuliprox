use tuliprox_core::model::Config;

#[test]
fn explicit_test_setting_disables_temp_dir_override() {
    let storage_dir = tempfile::tempdir().expect("storage temp dir");
    std::env::set_var("TULIPROX_DISABLE_TEMP_DIR_OVERRIDE", "1");

    let config = Config { storage_dir: storage_dir.path().to_string_lossy().into_owned(), ..Config::default() };
    config.update_runtime();

    let temp_dir = tempfile::tempdir().expect("process temp dir");
    assert!(!temp_dir.path().starts_with(storage_dir.path()));
}
