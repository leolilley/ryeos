//! Shared native code names no hosted-worker product. Two guards:
//!
//! - the production text of every Rust source in a shared crate (comments and
//!   strings included, `#[cfg(test)]` items excluded, read through the shared
//!   `rust_source_text` helper) contains none of the product words, except at
//!   pinned entries. A pin is a path, one word and the exact number of times
//!   it occurs, with the row that removes it; a pin that no longer matches
//!   fails, so a removed name cannot leave a stale allowance behind.
//! - no shared crate depends on a product crate through a normal or build
//!   dependency, except at pinned edges that name their row.
//!
//! "Shared" is structural. A crate is a product crate when its package name
//! holds a product word. Host-adapter crates (`crates/host-adapters/`) run a
//! named workload on purpose and are out of both guards, as are the workspace
//! members under `tests/` in the dependency guard. Test code is out:
//! files under any `tests` directory, `tests.rs`, `*_tests.rs`, and items
//! behind `#[cfg(test)]`. Non-Rust files are out.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../../../../tests/support/rust_source_text.rs"]
mod rust_source_text;

const PRODUCT_WORDS: [&str; 4] = ["codex", "claude", "anthropic", "openai"];

const HOST_ADAPTERS: &str = "crates/host-adapters/";

/// One allowed place a product word still appears.
struct Pin {
    path: &'static str,
    word: &'static str,
    count: usize,
    /// The roadmap row whose work removes this occurrence.
    row: &'static str,
}

const PINS: &[Pin] = &[
    Pin {
        path: "crates/bin/cli/src/codex_device_login.rs",
        word: "codex",
        count: 10,
        row: "W-8",
    },
    Pin {
        path: "crates/bin/cli/src/dispatcher.rs",
        word: "codex",
        count: 4,
        row: "W-8",
    },
    Pin {
        path: "crates/bin/cli/src/help.rs",
        word: "codex",
        count: 5,
        row: "W-8",
    },
    Pin {
        path: "crates/bin/cli/src/lib.rs",
        word: "codex",
        count: 1,
        row: "W-8",
    },
    Pin {
        path: "crates/bin/cli/src/lifecycle_commands.rs",
        word: "codex",
        count: 5,
        row: "W-8",
    },
    Pin {
        path: "crates/bin/cli/src/tty/help_flow.rs",
        word: "codex",
        count: 2,
        row: "W-8",
    },
    Pin {
        path: "crates/bin/daemon/src/test_support/fast_fixture.rs",
        word: "claude",
        count: 1,
        row: "W-49",
    },
    Pin {
        path: "crates/bin/daemon/src/test_support/fast_fixture.rs",
        word: "codex",
        count: 2,
        row: "W-49",
    },
    Pin {
        path: "crates/kinds/directive-definition/src/lib.rs",
        word: "anthropic",
        count: 5,
        row: "Q-20",
    },
    Pin {
        path: "crates/runtimes/directive/src/provider_adapter/messages.rs",
        word: "anthropic",
        count: 1,
        row: "Q-20",
    },
    Pin {
        path: "crates/runtimes/directive/src/provider_adapter/messages.rs",
        word: "openai",
        count: 7,
        row: "Q-20",
    },
    Pin {
        path: "crates/runtimes/directive/src/provider_adapter/streaming.rs",
        word: "anthropic",
        count: 3,
        row: "Q-20",
    },
    Pin {
        path: "crates/runtimes/directive/src/provider_adapter/streaming.rs",
        word: "claude",
        count: 1,
        row: "Q-20",
    },
    Pin {
        path: "crates/runtimes/directive/src/provider_adapter/streaming.rs",
        word: "openai",
        count: 3,
        row: "Q-20",
    },
    Pin {
        path: "crates/runtimes/directive/src/provider_adapter/tools.rs",
        word: "openai",
        count: 3,
        row: "Q-20",
    },
    Pin {
        path: "crates/daemon/ryeos-app/src/node_config/sections/external_execution.rs",
        word: "codex",
        count: 1,
        row: "W-50",
    },
    Pin {
        path: "crates/state/ryeos-state/src/external_execution/admission.rs",
        word: "codex",
        count: 5,
        row: "W-50",
    },
];

/// One allowed dependency of a shared crate on a product crate.
struct EdgePin {
    package: &'static str,
    dependency: &'static str,
    row: &'static str,
}

const EDGE_PINS: &[EdgePin] = &[];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace root above crates/state/ryeos-state")
        .to_path_buf()
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("path under the workspace")
        .to_string_lossy()
        .replace('\\', "/")
}

fn names_a_product(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["codex", "claude"].iter().any(|word| lower.contains(word))
}

/// The package name of a `Cargo.toml`: the first `name = "..."` line after
/// `[package]`.
fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = line.strip_prefix("name") {
                let value = rest.trim_start().strip_prefix('=')?.trim();
                return Some(value.trim_matches('"').to_owned());
            }
        }
    }
    None
}

/// Every directory under `crates/` that holds a `Cargo.toml`, with its
/// package name.
fn crate_directories(directory: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let manifest = directory.join("Cargo.toml");
    if let Ok(text) = std::fs::read_to_string(&manifest) {
        if let Some(name) = package_name(&text) {
            out.push((directory.to_path_buf(), name));
        }
    }
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && path.file_name().is_none_or(|name| name != "target") {
            crate_directories(&path, out);
        }
    }
}

fn rust_sources(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            if name == "target" || name == "tests" {
                continue;
            }
            rust_sources(&path, out);
        } else if kind.is_file()
            && name.ends_with(".rs")
            && name != "tests.rs"
            && !name.ends_with("_tests.rs")
        {
            out.push(path);
        }
    }
}

/// Every product word in the production text of the shared crates, as
/// (path, word) -> number of occurrences.
fn observed_names(root: &Path) -> BTreeMap<(String, &'static str), usize> {
    let mut crates = Vec::new();
    crate_directories(&root.join("crates"), &mut crates);
    let product_directories: Vec<String> = crates
        .iter()
        .filter(|(_, name)| names_a_product(name))
        .map(|(directory, _)| format!("{}/", relative(root, directory)))
        .collect();
    assert!(
        !product_directories.is_empty(),
        "no product crate found: the structural rule found nothing to exclude"
    );
    let mut files = Vec::new();
    rust_sources(&root.join("crates"), &mut files);
    assert!(!files.is_empty(), "no sources under {}", root.display());
    let mut found = BTreeMap::new();
    for file in files {
        let path = relative(root, &file);
        if path.starts_with(HOST_ADAPTERS)
            || product_directories
                .iter()
                .any(|directory| path.starts_with(directory))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let production = rust_source_text::production_text(&path, &text).to_ascii_lowercase();
        for word in PRODUCT_WORDS {
            let count = production.matches(word).count();
            if count > 0 {
                found.insert((path.clone(), word), count);
            }
        }
    }
    found
}

#[test]
fn shared_crates_name_no_hosted_worker_product_outside_the_pins() {
    let observed = observed_names(&workspace_root());
    let mut pinned = BTreeMap::new();
    for pin in PINS {
        assert!(!pin.row.is_empty(), "pin {} names no row", pin.path);
        assert!(
            pinned.insert((pin.path, pin.word), pin).is_none(),
            "pin {} {} is listed twice",
            pin.path,
            pin.word
        );
    }
    let mut violations = Vec::new();
    for ((path, word), count) in &observed {
        match pinned.get(&(path.as_str(), *word)) {
            None => violations.push(format!("{path}: names `{word}` {count} time(s), unpinned")),
            Some(pin) if pin.count != *count => violations.push(format!(
                "{path}: `{word}` occurs {count} time(s), the pin ({}) says {}",
                pin.row, pin.count
            )),
            Some(_) => {}
        }
    }
    for ((path, word), pin) in &pinned {
        if !observed.contains_key(&(path.to_string(), *word)) {
            violations.push(format!(
                "{path}: the pin for `{word}` ({}) no longer occurs; delete it",
                pin.row
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "shared native code names a hosted-worker product:\n{}",
        violations.join("\n")
    );
}

#[test]
fn no_shared_crate_depends_on_a_product_crate_outside_the_pinned_edges() {
    let root = workspace_root();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ])
        .current_dir(&root)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata is JSON");
    let packages = metadata["packages"].as_array().expect("packages");
    let product_packages: BTreeSet<&str> = packages
        .iter()
        .filter_map(|package| package["name"].as_str())
        .filter(|name| names_a_product(name))
        .collect();
    assert!(
        !product_packages.is_empty(),
        "no product package found: the structural rule found nothing to guard"
    );
    let mut edges = BTreeSet::new();
    for package in packages {
        let name = package["name"].as_str().expect("package name");
        let manifest = package["manifest_path"].as_str().expect("manifest path");
        // Product crates, host adapters and the workspace members under
        // `tests/` (end-to-end fixtures that nothing depends on) are out.
        if names_a_product(name)
            || manifest.contains(&format!("/{HOST_ADAPTERS}"))
            || Path::new(manifest).starts_with(root.join("tests"))
        {
            continue;
        }
        for dependency in package["dependencies"].as_array().expect("dependencies") {
            // A null kind is a normal dependency; "dev" is test-only.
            let kind = dependency["kind"].as_str();
            if kind == Some("dev") {
                continue;
            }
            let target = dependency["name"].as_str().expect("dependency name");
            if product_packages.contains(target) {
                edges.insert((name.to_owned(), target.to_owned()));
            }
        }
    }
    let mut pinned = BTreeSet::new();
    for pin in EDGE_PINS {
        assert!(
            !pin.row.is_empty(),
            "edge pin {} -> {} names no row",
            pin.package,
            pin.dependency
        );
        pinned.insert((pin.package.to_owned(), pin.dependency.to_owned()));
    }
    let mut violations = Vec::new();
    for (package, dependency) in edges.difference(&pinned) {
        violations.push(format!(
            "{package} depends on the product crate {dependency}, unpinned"
        ));
    }
    for (package, dependency) in pinned.difference(&edges) {
        violations.push(format!(
            "the pin {package} -> {dependency} no longer occurs; delete it"
        ));
    }
    assert!(
        violations.is_empty(),
        "a shared crate depends on a product crate:\n{}",
        violations.join("\n")
    );
}
