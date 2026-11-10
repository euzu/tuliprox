use super::*;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

fn zip_of(build: impl FnOnce(&mut ZipWriter<Cursor<Vec<u8>>>)) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    build(&mut writer);
    writer.finish().unwrap().into_inner()
}

fn stored() -> SimpleFileOptions { SimpleFileOptions::default().compression_method(CompressionMethod::Stored) }

fn deflated() -> SimpleFileOptions { SimpleFileOptions::default().compression_method(CompressionMethod::Deflated) }

fn file(writer: &mut ZipWriter<Cursor<Vec<u8>>>, name: &str, content: &[u8]) {
    writer.start_file(name, stored()).unwrap();
    writer.write_all(content).unwrap();
}

/// Extract into a fresh directory inside a temp dir that also holds a `canary`
/// path just outside it, which no extraction may create.
fn extract_package(bytes: &[u8]) -> (tempfile::TempDir, Result<(), String>) {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("work");
    fs::create_dir(&target).unwrap();
    let result = extract(bytes, &target);
    (root, result)
}

fn refused(bytes: &[u8], needle: &str) {
    let (root, result) = extract_package(bytes);
    let message = result.expect_err("the package must be refused");
    assert!(message.contains(needle), "`{message}` does not mention `{needle}`");
    assert!(!root.path().join("evil.txt").exists());
}

/// A deflated entry of `mib` MiB of zeros whose headers claim one byte.
fn lying_bomb(name: &str, mib: usize) -> Vec<u8> {
    let mut bytes = zip_of(|writer| {
        writer.start_file(name, deflated()).unwrap();
        let chunk = vec![0_u8; 1 << 20];
        for _ in 0..mib {
            writer.write_all(&chunk).unwrap();
        }
    });
    // local header (sizes at 18 and 22) and central directory entry (20 and 24)
    assert_eq!(&bytes[0..4], b"PK\x03\x04");
    bytes[22..26].copy_from_slice(&1_u32.to_le_bytes());
    let central = bytes.windows(4).rposition(|window| window == b"PK\x01\x02").unwrap();
    bytes[central + 24..central + 28].copy_from_slice(&1_u32.to_le_bytes());
    bytes
}

#[test]
fn a_well_formed_package_extracts() {
    let bytes = zip_of(|writer| {
        file(writer, "manifest.yml", b"id: demo");
        writer.add_directory("ui/", stored()).unwrap();
        file(writer, "ui/index.html", b"<html></html>");
        file(writer, "./plugin.wasm", b"\0asm");
    });
    let (root, result) = extract_package(&bytes);
    result.unwrap();
    let work = root.path().join("work");
    assert_eq!(fs::read(work.join("manifest.yml")).unwrap(), b"id: demo");
    assert_eq!(fs::read(work.join("ui/index.html")).unwrap(), b"<html></html>");
    assert!(work.join("plugin.wasm").is_file());
}

#[test]
fn parent_directory_paths_are_refused() {
    for name in ["../evil.txt", "a/../../evil.txt", "a/../b.txt", "ui/.."] {
        refused(&zip_of(|writer| file(writer, name, b"x")), "..");
    }
}

#[test]
fn absolute_paths_and_backslashes_are_refused() {
    refused(&zip_of(|writer| file(writer, "/tmp/evil.txt", b"x")), "absolute");
    refused(&zip_of(|writer| file(writer, "C:\\evil.txt", b"x")), "backslash");
    refused(&zip_of(|writer| file(writer, "ui\\evil.txt", b"x")), "backslash");
}

#[test]
fn symlinks_are_refused() {
    let bytes = zip_of(|writer| {
        file(writer, "manifest.yml", b"id: demo");
        writer.add_symlink("evil.txt", "/etc/passwd", stored()).unwrap();
    });
    refused(&bytes, "symlink");
}

#[test]
fn too_many_entries_are_refused_before_anything_is_written() {
    let bytes = zip_of(|writer| {
        for index in 0..=MAX_ENTRIES {
            file(writer, &format!("f{index}.txt"), b"");
        }
    });
    let (root, result) = extract_package(&bytes);
    assert!(result.unwrap_err().contains("entries"));
    assert_eq!(fs::read_dir(root.path().join("work")).unwrap().count(), 0);
}

#[test]
fn an_inflated_bomb_with_a_lying_header_is_stopped_by_the_running_count() {
    // 40 MiB of zeros deflate to a few dozen KiB; both headers claim 1 byte.
    let bytes = lying_bomb("bomb.bin", 40);
    assert!(bytes.len() < 1024 * 1024, "the archive itself is small: {} bytes", bytes.len());
    let (root, result) = extract_package(&bytes);
    let err = result.unwrap_err();
    assert!(
        err.contains("inflates past") || err.contains("larger than its declared uncompressed size"),
        "unexpected error: {err}"
    );
    // what was written stops at the cap
    let written = fs::metadata(root.path().join("work/bomb.bin")).unwrap().len();
    assert!(written <= MAX_ENTRY_BYTES + 64 * 1024, "{written} bytes were written");
}

#[test]
fn the_total_inflated_size_is_capped_too() {
    let bytes = zip_of(|writer| {
        let chunk = vec![0_u8; 1 << 20];
        for name in ["a.bin", "b.bin", "c.bin"] {
            writer.start_file(name, deflated()).unwrap();
            for _ in 0..30 {
                writer.write_all(&chunk).unwrap();
            }
        }
    });
    let (_root, result) = extract_package(&bytes);
    assert!(result.unwrap_err().contains("package inflates past"));
}

#[test]
fn a_file_that_is_not_a_zip_is_refused() {
    let (_root, result) = extract_package(b"not a zip at all");
    assert!(result.unwrap_err().contains("not a valid .tlpx"));
}

#[test]
fn installable_ids_follow_the_id_grammar_and_skip_reserved_names() {
    let longest = format!("a{}", "b".repeat(40));
    for good in ["abc", "demo", "sports-enricher", "a1-2", longest.as_str()] {
        assert!(is_installable_id(good), "{good}");
    }
    let too_long = format!("a{}", "b".repeat(41));
    for bad in [
        "",
        "ab",
        "Abc",
        "1abc",
        "-abc",
        "a_b_c",
        "a b c",
        "../evil",
        "a/b",
        "a\\b",
        ".hidden",
        "abc/",
        "abc.",
        "ábc",
        "staging",
        "repositories",
        "marketplace",
        too_long.as_str(),
    ] {
        assert!(!is_installable_id(bad), "{bad:?}");
    }
}

#[test]
fn the_manifest_id_is_checked_before_it_can_name_a_path() {
    for reserved in ["staging", "metrics", "repositories", "marketplace"] {
        assert!(manifest_id(&format!("id: {reserved}\nname: x")).unwrap_err().contains("reserved"), "{reserved}");
    }
    assert_eq!(manifest_id("id: demo\nname: x").unwrap(), "demo");
    for text in [
        "id: ../evil\nname: x",
        "id: /etc/passwd",
        "id: \"a/b\"",
        "id: Demo",
        "id: ab",
        "id: staging",
        "name: no id here",
        "id: [not, a, string]",
        "::: not yaml :::",
    ] {
        assert!(manifest_id(text).is_err(), "{text}");
    }
}

#[test]
fn text_is_stripped_of_controls_and_cut() {
    assert_eq!(sanitize_text("a\u{7}b\u{0}c\u{1b}[31md", 100), "abc[31md");
    assert_eq!(sanitize_text("line one\nline two\t!", 100), "line one line two !");
    assert_eq!(sanitize_text("  padded  ", 100), "padded");
    assert_eq!(sanitize_text("evil\u{202e}txt", 100), "eviltxt");
    assert_eq!(sanitize_text(&"é".repeat(600), 512).chars().count(), 512);
}

#[test]
fn a_staged_package_replaces_the_installed_one_and_leaves_nothing_behind() {
    let root = tempfile::tempdir().unwrap();
    let plugins = root.path().join("plugins");
    fs::create_dir_all(plugins.join("demo")).unwrap();
    fs::write(plugins.join("demo/manifest.yml"), "old").unwrap();
    fs::write(plugins.join("demo/old-only.txt"), "x").unwrap();
    let staged = root.path().join("staging/demo");
    fs::create_dir_all(staged.join("ui")).unwrap();
    fs::write(staged.join("manifest.yml"), "new").unwrap();
    fs::write(staged.join("ui/a.html"), "a").unwrap();

    install_staged(&staged, &plugins, "demo").unwrap();

    assert_eq!(fs::read_to_string(plugins.join("demo/manifest.yml")).unwrap(), "new");
    assert!(plugins.join("demo/ui/a.html").is_file());
    assert!(!plugins.join("demo/old-only.txt").exists());
    assert!(!staged.exists());
    let names: Vec<_> = fs::read_dir(&plugins).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(names, ["demo"], "no .incoming or .old leftovers");
}

#[test]
fn the_cross_filesystem_fallback_copies_a_whole_tree() {
    let root = tempfile::tempdir().unwrap();
    let from = root.path().join("from");
    fs::create_dir_all(from.join("ui/assets")).unwrap();
    fs::write(from.join("manifest.yml"), "m").unwrap();
    fs::write(from.join("ui/assets/x.css"), "css").unwrap();
    let to = root.path().join("to");
    copy_tree(&from, &to).unwrap();
    assert_eq!(fs::read_to_string(to.join("manifest.yml")).unwrap(), "m");
    assert_eq!(fs::read_to_string(to.join("ui/assets/x.css")).unwrap(), "css");
    // the copy refuses to follow anything that is not a regular file
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", from.join("link")).unwrap();
        assert!(copy_tree(&from, &root.path().join("to2")).is_err());
    }
}

#[test]
fn removing_a_symlink_leaves_its_target() {
    #[cfg(unix)]
    {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.txt"), "x").unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        remove_path(&link).unwrap();
        assert!(!link.exists());
        assert!(target.join("keep.txt").is_file());
        assert!(remove_path(&root.path().join("missing")).is_ok());
    }
}
