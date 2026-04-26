//! Tests for `lillux::cas::materialize_executable`.

use std::fs;
use std::path::PathBuf;

/// Return a fresh temp dir path that the caller is responsible for cleaning up.
fn tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lillux-test-{}-{}",
        label,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

#[test]
fn roundtrip_preserves_exec_bits() {
    let tmp = tmp_dir("exec");
    let target = tmp.join("test-binary");
    let data = b"#!/bin/sh\necho hello\n";

    lillux::cas::materialize_executable(&target, data, 0o755).expect("materialize");

    assert!(target.exists(), "target file should exist");
    let metadata = fs::metadata(&target).expect("metadata");
    assert!(metadata.is_file());

    let content = fs::read(&target).expect("read");
    assert_eq!(content.as_slice(), data, "file content should match");

    // Verify exec bits on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "expected mode 0o755, got {mode:#o}"
        );
    }

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn mode_0o644_is_not_executable() {
    let tmp = tmp_dir("mode644");
    let target = tmp.join("non-exec");
    let data = b"not executable\n";

    lillux::cas::materialize_executable(&target, data, 0o644).expect("materialize");

    assert!(target.exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0, "should have no exec bits");
    }

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn content_matches_input() {
    let tmp = tmp_dir("content");
    let target = tmp.join("blob");
    let data = (0..=255u8).collect::<Vec<_>>();

    lillux::cas::materialize_executable(&target, &data, 0o700).expect("materialize");

    let read_back = fs::read(&target).expect("read");
    assert_eq!(read_back, data, "all 256 byte values should round-trip");

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn creates_parent_dirs() {
    let tmp = tmp_dir("parents");
    let target = tmp.join("a").join("b").join("c").join("binary");

    lillux::cas::materialize_executable(&target, b"data", 0o755).expect("materialize");

    assert!(target.exists(), "should create nested directories");

    let _ = fs::remove_dir_all(&tmp);
}
