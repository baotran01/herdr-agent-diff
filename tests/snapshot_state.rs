use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use herdr_agent_diff::diff::{DiffLineKind, render_change};
use herdr_agent_diff::model::{
    ChangeKind, CurrentFile, FileRecord, MANIFEST_VERSION, Manifest, TextEligibility,
};
use herdr_agent_diff::snapshot::{CaptureRequest, capture, classify, safe_read, scan};
use herdr_agent_diff::state::StateStore;
use tempfile::TempDir;

fn store() -> (TempDir, StateStore) {
    let temp = TempDir::new().expect("temp state");
    let store = StateStore::new(temp.path()).expect("state store");
    (temp, store)
}

#[cfg(unix)]
#[test]
fn state_storage_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let (_state_temp, store) = store();
    let blob = store.write_blob(b"private").expect("blob");
    let marker = store
        .begin_capture("w1:p1")
        .expect("marker")
        .expect("capture marker");

    assert_eq!(
        store
            .root()
            .metadata()
            .expect("root metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for directory in ["manifests", "blobs", "markers", "viewers"] {
        assert_eq!(
            store
                .root()
                .join(directory)
                .metadata()
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert_eq!(
        store
            .root()
            .join("blobs")
            .join(blob)
            .metadata()
            .expect("blob metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        store
            .root()
            .join("markers/w1_p1.capture")
            .metadata()
            .expect("marker metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(marker);
}

fn manifest(root: &Path, pane: &str, files: BTreeMap<PathBuf, FileRecord>) -> Manifest {
    Manifest {
        version: MANIFEST_VERSION,
        pane_id: pane.into(),
        agent: "codex".into(),
        session_ref: Some("session-1".into()),
        root: root.to_path_buf(),
        captured_unix_ms: 1,
        files,
        notices: Vec::new(),
    }
}

#[test]
fn capture_classifies_added_modified_deleted_and_exact_rename() {
    let project = TempDir::new().expect("temp project");
    fs::write(project.path().join("modified.txt"), "before\n").expect("write modified");
    fs::write(project.path().join("deleted.txt"), "gone\n").expect("write deleted");
    fs::write(project.path().join("old-name.txt"), "same\n").expect("write rename");
    let (_state_temp, store) = store();
    capture(
        &store,
        CaptureRequest {
            pane_id: "w1:p1",
            agent: "codex",
            session_ref: Some("native".into()),
            root: project.path(),
        },
    )
    .expect("capture");

    fs::write(project.path().join("modified.txt"), "after\n").expect("modify");
    fs::remove_file(project.path().join("deleted.txt")).expect("delete");
    fs::rename(
        project.path().join("old-name.txt"),
        project.path().join("new-name.txt"),
    )
    .expect("rename");
    fs::write(project.path().join("added.txt"), "new\n").expect("add");

    let baseline = store
        .load_manifest("w1:p1")
        .expect("load")
        .expect("manifest");
    let (current, _) = scan(project.path()).expect("scan");
    let changes = classify(&baseline, &current);

    assert!(changes.iter().any(|change| {
        change.kind == ChangeKind::Added && change.path == Path::new("added.txt")
    }));
    assert!(changes.iter().any(|change| {
        change.kind == ChangeKind::Modified && change.path == Path::new("modified.txt")
    }));
    assert!(changes.iter().any(|change| {
        change.kind == ChangeKind::Deleted && change.path == Path::new("deleted.txt")
    }));
    assert!(changes.iter().any(|change| {
        change.kind == ChangeKind::Renamed
            && change.path == Path::new("new-name.txt")
            && change.old_path.as_deref() == Some(Path::new("old-name.txt"))
    }));
}

#[test]
fn scan_honors_ignore_files_caches_binary_invalid_utf8_and_size_limit() {
    let project = TempDir::new().expect("temp project");
    fs::write(project.path().join(".gitignore"), "ignored.txt\n").expect("ignore file");
    fs::write(project.path().join("ignored.txt"), "ignored").expect("ignored");
    fs::create_dir(project.path().join("node_modules")).expect("node modules");
    fs::write(project.path().join("node_modules/pkg.js"), "ignored").expect("cache file");
    fs::write(project.path().join("binary.bin"), b"a\0b").expect("binary");
    fs::write(project.path().join("invalid.txt"), [0xff, 0xfe]).expect("invalid");
    let oversized = fs::File::create(project.path().join("large.txt")).expect("large");
    oversized
        .set_len(herdr_agent_diff::model::INLINE_TEXT_LIMIT + 1)
        .expect("set len");

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
fn blobs_deduplicate_and_cleanup_when_unreferenced() {
    let project = TempDir::new().expect("temp project");
    let (_state_temp, store) = store();
    let blob = store.write_blob(b"same").expect("blob");
    assert_eq!(blob, store.write_blob(b"same").expect("same blob"));
    let record = FileRecord {
        size: 4,
        modified_unix_ns: None,
        hash: Some(blob.clone()),
        blob: Some(blob.clone()),
        text: TextEligibility::Text,
    };
    store
        .commit_manifest(&manifest(
            project.path(),
            "w1:p1",
            BTreeMap::from([(PathBuf::from("a.txt"), record)]),
        ))
        .expect("commit");
    assert!(store.root().join("blobs").join(&blob).exists());
    store.remove_pane("w1:p1").expect("cleanup");
    assert!(!store.root().join("blobs").join(blob).exists());
}

#[test]
fn blob_reads_reject_path_traversal_and_oversized_files() {
    let (_state_temp, store) = store();
    assert!(store.read_blob("../manifest.json").is_err());
    assert!(store.read_blob(&"a".repeat(64)).is_err());

    let hash = "b".repeat(64);
    let path = store.root().join("blobs").join(&hash);
    let file = fs::File::create(path).expect("oversized blob");
    file.set_len(herdr_agent_diff::model::INLINE_TEXT_LIMIT + 1)
        .expect("set blob length");
    assert!(store.read_blob(&hash).is_err());
}

#[test]
fn capture_is_atomic_serialized_and_supersedes_previous_manifest() {
    let project = TempDir::new().expect("temp project");
    fs::write(project.path().join("a.txt"), "one").expect("write");
    let (_state_temp, store) = store();
    let guard = store
        .begin_capture("w1:p1")
        .expect("marker")
        .expect("first marker");
    assert!(store.begin_capture("w1:p1").expect("second").is_none());
    drop(guard);

    for session in ["one", "two"] {
        capture(
            &store,
            CaptureRequest {
                pane_id: "w1:p1",
                agent: "codex",
                session_ref: Some(session.into()),
                root: project.path(),
            },
        )
        .expect("capture");
    }
    assert_eq!(
        store
            .load_manifest("w1:p1")
            .expect("load")
            .expect("manifest")
            .session_ref
            .as_deref(),
        Some("two")
    );
}

#[test]
fn unified_diff_colors_semantic_line_kinds() {
    let project = TempDir::new().expect("temp project");
    fs::write(project.path().join("a.rs"), "fn old() {}\n").expect("write");
    let (_state_temp, store) = store();
    capture(
        &store,
        CaptureRequest {
            pane_id: "w1:p1",
            agent: "codex",
            session_ref: None,
            root: project.path(),
        },
    )
    .expect("capture");
    fs::write(project.path().join("a.rs"), "fn new() {}\n").expect("modify");
    let baseline = store
        .load_manifest("w1:p1")
        .expect("load")
        .expect("manifest");
    let (current, _) = scan(project.path()).expect("scan");
    let change = classify(&baseline, &current).remove(0);
    let lines = render_change(&store, project.path(), &change);
    assert!(lines.iter().any(|line| line.kind == DiffLineKind::Deletion));
    assert!(lines.iter().any(|line| line.kind == DiffLineKind::Addition));
    assert!(lines.iter().any(|line| line.kind == DiffLineKind::Hunk));
    let deletion = lines
        .iter()
        .find(|line| line.kind == DiffLineKind::Deletion)
        .expect("deletion line");
    assert_eq!(deletion.old_line, Some(1));
    assert_eq!(deletion.new_line, None);
    let addition = lines
        .iter()
        .find(|line| line.kind == DiffLineKind::Addition)
        .expect("addition line");
    assert_eq!(addition.old_line, None);
    assert_eq!(addition.new_line, Some(1));
}

#[test]
fn manifest_version_mismatch_is_rejected() {
    let project = TempDir::new().expect("temp project");
    let (_state_temp, store) = store();
    let invalid = manifest(project.path(), "w1:p1", BTreeMap::new());
    let mut value = serde_json::to_value(invalid).expect("serialize");
    value["version"] = serde_json::json!(999);
    let path = store.root().join("manifests/w1_p1.json");
    fs::write(path, serde_json::to_vec(&value).expect("bytes")).expect("write manifest");
    assert!(store.load_manifest("w1:p1").is_err());
}

#[test]
fn unreadable_files_with_changed_metadata_are_reported() {
    let project = TempDir::new().expect("temp project");
    let baseline = manifest(
        project.path(),
        "w1:p1",
        BTreeMap::from([(
            PathBuf::from("unreadable.bin"),
            FileRecord {
                size: 4,
                modified_unix_ns: Some(1),
                hash: None,
                blob: None,
                text: TextEligibility::Unreadable,
            },
        )]),
    );
    let current = BTreeMap::from([(
        PathBuf::from("unreadable.bin"),
        CurrentFile {
            relative: PathBuf::from("unreadable.bin"),
            absolute: project.path().join("unreadable.bin"),
            size: 8,
            modified_unix_ns: Some(2),
            hash: None,
            text: TextEligibility::Unreadable,
        },
    )]);

    let changes = classify(&baseline, &current);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, ChangeKind::Modified);
}

#[test]
fn blob_gc_keeps_blobs_when_a_manifest_is_unreadable() {
    let (_state_temp, store) = store();
    let blob = store.write_blob(b"keep me").expect("blob");
    fs::write(store.root().join("manifests/broken.json"), b"{").expect("broken manifest");

    store.gc_blobs().expect("gc");
    assert!(store.root().join("blobs").join(blob).exists());
}

#[test]
fn blob_gc_defers_while_a_capture_is_in_progress() {
    let (_state_temp, store) = store();
    let blob = store.write_blob(b"capture in progress").expect("blob");
    let guard = store
        .begin_capture("w1:p1")
        .expect("marker")
        .expect("capture marker");

    store.gc_blobs().expect("deferred gc");
    assert!(store.root().join("blobs").join(&blob).exists());
    drop(guard);

    store.gc_blobs().expect("gc");
    assert!(!store.root().join("blobs").join(blob).exists());
}
