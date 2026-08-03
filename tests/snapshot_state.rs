use std::fs;
use std::path::Path;

use herdr_agent_diff::model::{INLINE_TEXT_LIMIT, TextEligibility};
use herdr_agent_diff::snapshot::{safe_read, scan, workspace_fingerprint};
use tempfile::TempDir;

#[test]
fn scan_honors_ignore_files_and_classifies_file_content() {
    let project = TempDir::new().expect("temp project");
    fs::write(
        project.path().join(".gitignore"),
        "ignored.txt\ngenerated/\n",
    )
    .expect("ignore file");
    fs::write(project.path().join("ignored.txt"), "ignored").expect("ignored");
    fs::create_dir(project.path().join("generated")).expect("generated");
    let generated =
        fs::File::create(project.path().join("generated/large.bin")).expect("large ignored file");
    generated
        .set_len(1024 * 1024 * 1024 + 1)
        .expect("set ignored length");
    fs::create_dir(project.path().join("node_modules")).expect("node modules");
    fs::write(project.path().join("node_modules/pkg.js"), "ignored").expect("cache file");
    fs::write(project.path().join("binary.bin"), b"a\0b").expect("binary");
    fs::write(project.path().join("invalid.txt"), [0xff, 0xfe]).expect("invalid");
    let oversized = fs::File::create(project.path().join("large.txt")).expect("large");
    oversized.set_len(INLINE_TEXT_LIMIT + 1).expect("set len");

    let (files, _) = scan(project.path()).expect("scan");
    assert!(!files.contains_key(Path::new("ignored.txt")));
    assert!(!files.contains_key(Path::new("generated/large.bin")));
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

#[test]
fn scan_uses_eclipse_bazelproject_directories_when_present() {
    let project = TempDir::new().expect("temp project");
    fs::create_dir(project.path().join(".eclipse")).expect("eclipse directory");
    fs::write(
        project.path().join(".eclipse/.bazelproject"),
        "directories:\n  java/selected\n  -java/selected/generated\n",
    )
    .expect("project view");
    fs::create_dir_all(project.path().join("java/selected/generated")).expect("generated");
    fs::create_dir_all(project.path().join("java/other")).expect("other");
    fs::write(project.path().join("java/selected/Main.java"), "selected").expect("selected");
    fs::write(
        project.path().join("java/selected/generated/Build.java"),
        "excluded",
    )
    .expect("excluded");
    fs::write(project.path().join("java/other/Other.java"), "other").expect("other");
    fs::write(project.path().join("README.md"), "outside").expect("outside");

    let (files, _) = scan(project.path()).expect("scan");
    assert!(files.contains_key(Path::new("java/selected/Main.java")));
    assert!(!files.contains_key(Path::new("java/selected/generated/Build.java")));
    assert!(!files.contains_key(Path::new("java/other/Other.java")));
    assert!(!files.contains_key(Path::new("README.md")));
}

#[test]
fn non_bazel_repositories_keep_full_filesystem_scope() {
    let project = TempDir::new().expect("temp project");
    fs::write(project.path().join("README.md"), "outside").expect("outside");
    fs::create_dir_all(project.path().join("java/other")).expect("other");
    fs::write(project.path().join("java/other/Other.java"), "other").expect("other");

    let (files, _) = scan(project.path()).expect("scan");
    assert!(files.contains_key(Path::new("README.md")));
    assert!(files.contains_key(Path::new("java/other/Other.java")));
}

#[test]
fn root_level_bazelproject_activates_project_scope() {
    let project = TempDir::new().expect("temp project");
    fs::write(
        project.path().join(".bazelproject"),
        "directories:\n  java/selected\n",
    )
    .expect("root project view");
    fs::create_dir_all(project.path().join("java/selected")).expect("selected directory");
    fs::write(project.path().join("java/selected/Main.java"), "selected").expect("selected");
    fs::write(project.path().join("README.md"), "outside").expect("outside");

    let (files, _) = scan(project.path()).expect("scan");
    assert!(files.contains_key(Path::new("java/selected/Main.java")));
    assert!(!files.contains_key(Path::new("README.md")));
}

#[test]
fn project_view_is_found_above_a_nested_workspace_with_upper_git_root() {
    let repository = TempDir::new().expect("repository");
    fs::create_dir(repository.path().join(".git")).expect("git marker");
    fs::create_dir(repository.path().join(".eclipse")).expect("eclipse directory");
    fs::write(
        repository.path().join(".eclipse/.bazelproject"),
        "directories:\n  project/java/selected\n",
    )
    .expect("project view");
    let project = repository.path().join("project");
    fs::create_dir_all(project.join("java/selected")).expect("selected directory");
    fs::create_dir_all(project.join("java/other")).expect("other directory");
    fs::write(project.join("java/selected/Main.java"), "selected").expect("selected");
    fs::write(project.join("java/other/Other.java"), "other").expect("other");

    let (files, _) = scan(&project).expect("scan");
    assert!(files.contains_key(Path::new("java/selected/Main.java")));
    assert!(!files.contains_key(Path::new("java/other/Other.java")));
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
