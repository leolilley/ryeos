use std::ffi::OsStr;

use lillux::PinnedDirectory;

#[test]
fn streamed_atomic_create_is_bounded_and_never_publishes_a_partial_file() {
    let root = tempfile::tempdir().unwrap();
    let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
    let mut oversized = std::io::Cursor::new(b"12345".to_vec());
    assert!(
        directory
            .atomic_create_regular_from_reader(OsStr::new("payload"), &mut oversized, 4, 0o600,)
            .is_err()
    );
    assert!(!root.path().join("payload").exists());

    let mut admitted = std::io::Cursor::new(b"1234".to_vec());
    let (_, written) = directory
        .atomic_create_regular_from_reader(OsStr::new("payload"), &mut admitted, 4, 0o600)
        .unwrap()
        .unwrap();
    assert_eq!(written, 4);
    assert_eq!(std::fs::read(root.path().join("payload")).unwrap(), b"1234");
}
