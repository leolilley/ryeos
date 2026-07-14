//! Generic atomic filesystem primitive coverage.

use std::fs;
use std::path::Path;

use lillux::atomic_fs::atomic_write;

/// True when `dir` holds no atomic-write temp file (`*.tmp.<pid>`).
fn no_temp_files_left(dir: &Path) -> bool {
    fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp."))
}

#[test]
fn atomic_write_creates_parents_and_leaves_no_temp() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("a").join("b").join("c").join("blob");

    atomic_write(&target, b"payload").expect("write");

    assert_eq!(fs::read(&target).expect("read"), b"payload");
    assert!(
        no_temp_files_left(target.parent().unwrap()),
        "no temp file may survive a successful write"
    );
}

#[test]
fn atomic_write_replaces_existing_content() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("blob");

    atomic_write(&target, b"first").expect("write 1");
    atomic_write(&target, b"second").expect("write 2");

    assert_eq!(
        fs::read(&target).expect("read"),
        b"second",
        "rename replaces the target wholesale"
    );
    assert!(no_temp_files_left(tmp.path()));
}
