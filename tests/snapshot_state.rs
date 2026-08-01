use std::fs;
use std::path::Path;

use herdr_agent_diff::model::{INLINE_TEXT_LIMIT, TextEligibility};
use herdr_agent_diff::snapshot::{safe_read, scan, workspace_fingerprint};
use tempfile::TempDir;

#[test]
fn scan_honors_ignore_files_and_classifies_file_content() {
    let project = TempDir::new().expect("temp project");
    fs::write(project.path().join(".gitignore"), "ignored.txt\n").expect("ignore file");
    fs::write(project.path().join("ignored.txt"), "ignored").expect("ignored");
    fs::create_dir(project.path().join("node_modules")).expect("node modules");
    fs::write(project.path().join("node_modules/pkg.js"), "ignored").expect("cache file");
    fs::write(project.path().join("binary.bin"), b"a\0b").expect("binary");
    fs::write(project.path().join("invalid.txt"), [0xff, 0xfe]).expect("invalid");
    let oversized = fs::File::create(project.path().join("large.txt")).expect("large");
    oversized.set_len(INLINE_TEXT_LIMIT + 1).expect("set len");

    let (files, _) = scan(project.path()).expect("scan");
    assert!(!files.contains_key(Path::new("ignored.txt")));
    assert!(!files.contains_key(Path::new("node_modules/pkg.js")));
    assert_eq!(files[Path::new("binary.bin")].text, TextEligibility::Binary);
    assert_eq!(
        files[Path::new("invalid.txt")].text,
        TextEligibility::InvalidUtf8
    );
    assert_eq!(
        files[Path::new("large.txt")].text,
        TextEligibility::Oversized
    );
}

#[test]
fn workspace_fingerprint_skips_ignored_files_and_detects_metadata_changes() {
    let project = TempDir::new().expect("temp project");
    fs::write(project.path().join(".gitignore"), "ignored.txt\n").expect("ignore file");
    fs::write(project.path().join("visible.txt"), "before").expect("visible");

    let initial = workspace_fingerprint(project.path()).expect("initial fingerprint");
    fs::write(project.path().join("ignored.txt"), "ignored").expect("ignored");
    assert_eq!(
        workspace_fingerprint(project.path()).expect("ignored fingerprint"),
        initial
    );

    fs::write(project.path().join("visible.txt"), "after").expect("changed");
    assert_ne!(
        workspace_fingerprint(project.path()).expect("changed fingerprint"),
        initial
    );
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed_and_reads_cannot_escape_root() {
    use std::os::unix::fs::symlink;

    let project = TempDir::new().expect("temp project");
    let outside = TempDir::new().expect("outside");
    fs::write(outside.path().join("secret.txt"), "secret").expect("secret");
    symlink(
        outside.path().join("secret.txt"),
        project.path().join("link.txt"),
    )
    .expect("symlink");
    let (files, _) = scan(project.path()).expect("scan");
    assert!(!files.contains_key(Path::new("link.txt")));
    assert!(safe_read(project.path(), Path::new("../secret.txt"), 100).is_err());
    assert!(safe_read(project.path(), Path::new("link.txt"), 100).is_err());
    fs::create_dir(project.path().join("nested")).expect("nested");
    symlink(outside.path(), project.path().join("nested/outside")).expect("nested symlink");
    assert!(safe_read(project.path(), Path::new("nested/outside/secret.txt"), 100).is_err());
}

#[test]
fn safe_read_enforces_the_inline_limit() {
    let project = TempDir::new().expect("temp project");
    fs::write(project.path().join("small.txt"), "small").expect("small");
    assert_eq!(
        safe_read(project.path(), Path::new("small.txt"), 100).expect("read small"),
        b"small"
    );
    assert!(safe_read(project.path(), Path::new("small.txt"), 2).is_err());
}
