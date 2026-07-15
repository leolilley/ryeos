//! Shard-path, blob-store, and batch-write hardening for `lillux::cas`.
//!
//! Companion to `cas_materialize_executable.rs`: exercises the content-
//! addressed store's addressing math (`shard_path`, `valid_hash`), the
//! `CasStore` blob round-trip and idempotency, and the crash-window guarantee
//! of `atomic_write_batch`.

use std::fs;
use std::path::Path;

#[cfg(unix)]
use lillux::cas::atomic_write_batch_in_pinned_root;
use lillux::cas::{atomic_write_batch, shard_path, valid_hash, CasStore};
use lillux::sha256_hex;
#[cfg(unix)]
use lillux::PinnedDirectory;

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
    assert!(store.has_blob(&hash).expect("has blob"));
    assert!(store.has(&hash).expect("has typed entry"));
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
fn store_blob_rehashes_and_rejects_a_corrupt_existing_entry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(tmp.path().to_path_buf());
    let data = b"expected blob bytes";
    let hash = sha256_hex(data);
    let path = shard_path(store.root(), "blobs", &hash, "");
    fs::create_dir_all(path.parent().unwrap()).expect("create shard");
    fs::write(&path, b"substituted bytes").expect("seed corrupt entry");

    let error = store
        .store_blob(data)
        .expect_err("path existence must not satisfy a CAS write");

    assert!(error.to_string().contains("integrity failure"), "{error:#}");
    assert_eq!(
        fs::read(&path).expect("read corrupt entry"),
        b"substituted bytes",
        "failed verification must not silently repair or conceal corruption"
    );
}

#[test]
fn store_object_rehashes_and_rejects_a_corrupt_existing_entry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(tmp.path().to_path_buf());
    let value = serde_json::json!({"kind": "test", "value": 7});
    let canonical = lillux::cas::canonical_json(&value);
    let hash = sha256_hex(canonical.as_bytes());
    let path = shard_path(store.root(), "objects", &hash, ".json");
    fs::create_dir_all(path.parent().unwrap()).expect("create shard");
    fs::write(&path, br#"{"kind":"substituted"}"#).expect("seed corrupt entry");

    let error = store
        .store_object(&value)
        .expect_err("path existence must not satisfy a CAS object write");

    assert!(error.to_string().contains("integrity failure"), "{error:#}");
}

#[test]
fn get_and_has_reject_malformed_hash() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(tmp.path().to_path_buf());

    assert!(!store.has_blob("nothex").expect("has malformed blob"));
    assert!(!store.has("too-short").expect("has malformed entry"));
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

    assert!(!store.has_blob(&absent).expect("has absent blob"));
    assert!(store.get_blob(&absent).expect("get").is_none());
}

#[test]
fn corrupt_existing_entry_is_an_error_and_is_never_replaced() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(tmp.path().to_path_buf());
    let expected = b"addressed bytes";
    let hash = store.store_blob(expected).expect("initial store");
    let path = shard_path(store.root(), "blobs", &hash, "");
    fs::write(&path, b"corrupt bytes").expect("inject corruption");

    assert!(store.get_blob(&hash).is_err());
    assert!(store.has_blob(&hash).is_err());
    assert!(store.store_blob(expected).is_err());
    assert_eq!(
        fs::read(&path).expect("read corrupt entry"),
        b"corrupt bytes",
        "a CAS store must not repair corruption by replacing authority"
    );
}

#[test]
fn object_reader_rejects_hash_valid_noncanonical_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(tmp.path().to_path_buf());
    let bytes = br#"{"b":2,"a":1}"#;
    let hash = sha256_hex(bytes);
    let path = shard_path(store.root(), "objects", &hash, ".json");
    fs::create_dir_all(path.parent().expect("object parent")).expect("create shard");
    fs::write(&path, bytes).expect("write noncanonical object");

    assert!(store.get_object(&hash).is_err());
}

#[cfg(unix)]
#[test]
fn cas_store_rejects_a_symlinked_root() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).expect("outside");
    let root = tmp.path().join("cas-link");
    symlink(&outside, &root).expect("root symlink");
    let store = CasStore::new(root);
    let hash = "a".repeat(64);

    assert!(store.get_blob(&hash).is_err());
    assert!(store.store_blob(b"must not escape").is_err());
    assert!(fs::read_dir(&outside)
        .expect("outside listing")
        .next()
        .is_none());
}

// ── atomic write crash-window behavior ─────────────────────────────────

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

#[test]
fn atomic_write_batch_accepts_exact_existing_bytes_but_never_replaces_them() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("immutable");
    fs::write(&target, b"existing").expect("seed immutable entry");

    atomic_write_batch(&[(target.clone(), b"existing".to_vec())])
        .expect("exact existing bytes are idempotent");
    assert!(atomic_write_batch(&[(target.clone(), b"different".to_vec())]).is_err());
    assert_eq!(
        fs::read(&target).expect("read immutable entry"),
        b"existing"
    );
}

#[cfg(unix)]
#[test]
fn atomic_write_batch_rejects_a_symlinked_parent() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).expect("outside");
    let linked = tmp.path().join("linked");
    symlink(&outside, &linked).expect("parent symlink");

    assert!(atomic_write_batch(&[(linked.join("escaped"), b"bytes".to_vec())]).is_err());
    assert!(!outside.join("escaped").exists());
}

#[cfg(unix)]
#[test]
fn pinned_atomic_batch_rejects_targets_outside_its_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root_path = tmp.path().join("cas");
    fs::create_dir(&root_path).expect("cas root");
    let root = PinnedDirectory::open(&root_path)
        .expect("open root")
        .expect("root exists");
    let outside = tmp.path().join("outside");

    assert!(atomic_write_batch_in_pinned_root(
        &root,
        &[(outside.clone(), b"must not escape".to_vec())]
    )
    .is_err());
    assert!(!outside.exists());
}

#[cfg(unix)]
#[test]
fn pinned_atomic_batch_does_not_rebind_after_root_replacement() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root_path = tmp.path().join("cas");
    fs::create_dir(&root_path).expect("cas root");
    let root = PinnedDirectory::open(&root_path)
        .expect("open root")
        .expect("root exists");
    let retained_path = tmp.path().join("retained-cas");
    fs::rename(&root_path, &retained_path).expect("move retained root");
    fs::create_dir(&root_path).expect("replacement root");
    let target = root_path.join("objects/aa/bb/value.json");

    atomic_write_batch_in_pinned_root(&root, &[(target, b"authority".to_vec())])
        .expect("write through retained root");

    assert_eq!(
        fs::read(retained_path.join("objects/aa/bb/value.json")).expect("retained object"),
        b"authority"
    );
    assert!(!root_path.join("objects/aa/bb/value.json").exists());
}
