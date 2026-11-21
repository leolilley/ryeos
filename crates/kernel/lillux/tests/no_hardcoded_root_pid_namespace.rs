//! Guard: no fixture may hard-code the root pid namespace inode.
//!
//! A recorded identity is only judged by pid and birth evidence when its
//! `pid_namespace` equals the observer's. A fixture that names the host's
//! root namespace literally passes on a bare-metal runner and silently turns
//! every fake-pid "dead" judgment into "unavailable" on a runner whose test
//! process sits in another pid namespace (a container). Fixtures take the
//! observer's own namespace from `lillux::current_pid_namespace()`; a fixture
//! that means a foreign namespace names it with a small distinct inode
//! (`pid:[1]`, `pid:[2]`, `pid:[0]`) or an arbitrary non-root one.

use std::path::{Path, PathBuf};

/// The root pid namespace inode on a stock Linux host, assembled so this file
/// does not contain the literal it forbids.
fn forbidden() -> String {
    ["4026531", "836"].concat()
}

fn visit(directory: &Path, needle: &str, offenders: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "target" || name == ".git" || name == "node_modules" {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            visit(&path, needle, offenders);
        } else if kind.is_file()
            && std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle))
        {
            offenders.push(path);
        }
    }
}

#[test]
fn no_source_or_fixture_hard_codes_the_root_pid_namespace_inode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace root")
        .to_path_buf();
    let needle = forbidden();
    let mut offenders = Vec::new();
    for tree in ["crates", "tests"] {
        visit(&root.join(tree), &needle, &mut offenders);
    }
    assert!(
        offenders.is_empty(),
        "fixtures must read the observer's pid namespace (lillux::current_pid_namespace), \
         not hard-code the root namespace inode; found in {offenders:#?}"
    );
}
