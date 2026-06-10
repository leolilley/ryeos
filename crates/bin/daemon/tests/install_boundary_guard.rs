use std::fs;
use std::path::{Path, PathBuf};

const WRITE_PATTERNS: &[&str] = &[
    "fs::write",
    "std::fs::write",
    "create_dir_all",
    "File::create",
    "OpenOptions::new",
];

const ALLOWLIST: &[&str] = &[
    "crates/daemon/ryeos-node/src/init.rs",
    "crates/daemon/ryeos-bundle/",
    "crates/daemon/ryeos-api/src/handlers/bundle_install.rs",
    "crates/daemon/ryeos-api/src/handlers/remote_bundle_install.rs",
    "crates/daemon/ryeos-api/src/handlers/remote_bundle_remove.rs",
];

#[test]
fn install_root_does_not_leak_writable_paths() {
    let root = workspace_root();
    let roots_rs = root.join("crates/engine/ryeos-engine/src/roots.rs");
    let roots_content = fs::read_to_string(&roots_rs).expect("read roots.rs");
    assert!(
        !roots_content.contains("impl AsRef<Path> for InstallRoot"),
        "InstallRoot must not implement AsRef<Path>"
    );
    let install_impl = roots_content
        .split("impl InstallRoot {")
        .nth(1)
        .and_then(|tail| {
            tail.split("/// Writable handle to the runtime/config/state zone.")
                .next()
        })
        .expect("InstallRoot impl block present");
    assert!(
        !install_impl.contains("fn as_path(&self)"),
        "InstallRoot must not expose as_path()"
    );

    let mut failures = Vec::new();
    scan_dir(&root.join("crates"), &mut |path| {
        let rel = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWLIST.iter().any(|prefix| rel.starts_with(prefix)) {
            return;
        }
        let Ok(content) = fs::read_to_string(path) else {
            return;
        };
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if !WRITE_PATTERNS.iter().any(|pattern| line.contains(pattern)) {
                continue;
            }
            let start = idx.saturating_sub(3);
            let window = lines[start..=idx].join("\n");
            if window.contains("install_root")
                || window.contains(".bundles()")
                || window.contains("InstallRoot")
            {
                failures.push(format!("{}:{}\n{}", rel, idx + 1, window));
            }
        }
    });

    assert!(
        failures.is_empty(),
        "install-zone write candidates found:\n{}",
        failures.join("\n\n")
    );
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("crates").is_dir())
        .expect("workspace root")
        .to_path_buf()
}

fn scan_dir(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("read dir entry");
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name == "target" || file_name == ".git" {
            continue;
        }
        if path.is_dir() {
            scan_dir(&path, visit);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            visit(&path);
        }
    }
}
