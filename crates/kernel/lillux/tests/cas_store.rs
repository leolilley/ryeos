//! Shard-path, blob-store, and atomic-write hardening for `lillux::cas`.
//!
//! Companion to `cas_materialize_executable.rs`: exercises the content-
//! addressed store's addressing math (`shard_path`, `valid_hash`), the
//! `CasStore` blob round-trip and idempotency, and the crash-window
//! guarantee of `atomic_write` / `atomic_write_batch` (a successful write
//! leaves the target fully populated and no `.tmp.<pid>` sibling behind —
//! a crash can only ever leave the temp, never a partial target).

use std::fs;
use std::path::Path;

use lillux::cas::{atomic_write, atomic_write_batch, shard_path, valid_hash, CasStore};
use lillux::sha256_hex;

/// True when `dir` holds no atomic-write temp file (`*.tmp.<pid>`).
fn no_temp_files_left(dir: &Path) -> bool {
    fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .all(|e| !e.file_name().to_string_lossy().contains(".tmp."))
}

// ── shard-path addressing ──────────────────────────────────────────────

#[test]
fn shard_path_uses_two_level_hex_prefix() {
    let root = Path::new("/cas");
    let hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    let blob = shard_path(root, "blobs", hash, "");
    assert_eq!(blob, Path::new("/cas/blobs/ab/cd").join(hash));

    let object = shard_path(root, "objects", hash, ".json");
    assert_eq!(
        object,
        Path::new("/cas/objects/ab/cd").join(format!("{hash}.json"))
    );
}

#[test]
fn valid_hash_edge_cases() {
    assert!(valid_hash(&"a".repeat(64)), "64 lowercase hex is valid");
    assert!(valid_hash(&"A".repeat(64)), "uppercase hex is valid");
    assert!(!valid_hash(&"a".repeat(63)), "63 chars is too short");
    assert!(!valid_hash(&"a".repeat(65)), "65 chars is too long");
    assert!(!valid_hash(""), "empty is invalid");
    assert!(
        !valid_hash(&format!("{}g", "a".repeat(63))),
        "non-hex character is invalid"
    );
}

// ── CasStore blob round-trip ───────────────────────────────────────────

#[test]
fn store_blob_round_trip_and_membership() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(tmp.path().to_path_buf());
    let data = b"hello cas";

    let hash = store.store_blob(data).expect("store");
    assert_eq!(hash, sha256_hex(data), "hash must be the content digest");
    assert!(store.has_blob(&hash));
    assert!(store.has(&hash));
    assert_eq!(
        store.get_blob(&hash).expect("get").expect("present"),
        data,
        "round-tripped bytes must match"
    );
}

#[test]
fn store_blob_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(tmp.path().to_path_buf());
    let data = b"same bytes";

    let first = store.store_blob(data).expect("store 1");
    let second = store.store_blob(data).expect("store 2");
    assert_eq!(first, second, "same content addresses the same hash");

    let path = shard_path(store.root(), "blobs", &first, "");
    assert!(path.exists(), "blob lands at its shard path");
}

#[test]
fn get_and_has_reject_malformed_hash() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(tmp.path().to_path_buf());

    assert!(!store.has_blob("nothex"));
    assert!(!store.has("too-short"));
    assert!(
        store.get_blob("not-a-valid-hash").expect("get").is_none(),
        "a malformed hash must never resolve to a blob"
    );
}

#[test]
fn get_blob_absent_returns_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(tmp.path().to_path_buf());
    let absent = "b".repeat(64);

    assert!(!store.has_blob(&absent));
    assert!(store.get_blob(&absent).expect("get").is_none());
}

// ── atomic write crash-window behavior ─────────────────────────────────

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

#[test]
fn atomic_write_batch_single_writes_and_cleans_up() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("solo");

    atomic_write_batch(&[(target.clone(), b"only".to_vec())]).expect("batch");

    assert_eq!(fs::read(&target).expect("read"), b"only");
    assert!(no_temp_files_left(tmp.path()));
}

#[test]
fn atomic_write_batch_multi_writes_all_and_cleans_up() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let writes: Vec<_> = (0..3)
        .map(|i| {
            (
                tmp.path().join(format!("f{i}")),
                format!("body-{i}").into_bytes(),
            )
        })
        .collect();

    atomic_write_batch(&writes).expect("batch");

    for (path, body) in &writes {
        assert_eq!(&fs::read(path).expect("read"), body);
    }
    assert!(
        no_temp_files_left(tmp.path()),
        "no batch temp file may survive a successful flush"
    );
}
