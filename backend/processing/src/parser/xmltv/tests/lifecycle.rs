/// Smallest testable piece of the disk-spilling path. The Drop guard is
/// what keeps a panic or early return from leaking temp files into /tmp.
/// If this fails, every other disk-spilling test is built on sand.
#[test]
fn disk_epg_source_removes_its_temp_file_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg-source.db");
    std::fs::write(&path, b"placeholder").unwrap();
    assert!(path.exists(), "precondition: temp file must exist");

    {
        let _src = super::super::DiskEpgSource::new(path.clone(), None, 0, 0);
    } // _src dropped here

    assert!(!path.exists(), "DiskEpgSource::Drop must remove its temp file");
}
