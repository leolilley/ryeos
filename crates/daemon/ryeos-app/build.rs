use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

fn git(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .current_dir(manifest_dir)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn git_path(manifest_dir: &Path, name: &str) -> Option<PathBuf> {
    git(
        manifest_dir,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )
    .map(PathBuf::from)
}

fn watch_existing(paths: &mut BTreeSet<PathBuf>, path: PathBuf) {
    if path.exists() {
        paths.insert(path);
    }
}

fn watch_git(manifest_dir: &Path) {
    let Some(head) = git_path(manifest_dir, "HEAD") else {
        return;
    };
    let mut paths = BTreeSet::new();
    watch_existing(&mut paths, head.clone());
    if let Some(dir) = head.parent() {
        watch_existing(&mut paths, dir.join("commondir"));
    }
    // Linked worktrees use a .git indirection file, not a directory.
    if let Some(root) = git(manifest_dir, &["rev-parse", "--show-toplevel"]) {
        let pointer = PathBuf::from(root).join(".git");
        if pointer.is_file() {
            watch_existing(&mut paths, pointer);
        }
    }
    if let Some(reference) = git(manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(mut loose) = git_path(manifest_dir, &reference) {
            // Packed and unborn refs need an existing ancestor to detect creation.
            while !loose.exists() {
                if !loose.pop() {
                    break;
                }
            }
            watch_existing(&mut paths, loose);
        }
        if let Some(packed) = git_path(manifest_dir, "packed-refs") {
            watch_existing(&mut paths, packed);
        }
    }
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    // Re-run when HEAD moves or the provenance env vars change so the embedded
    // revision/date stay accurate across rebuilds.
    println!("cargo:rerun-if-changed=build.rs");
    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
    println!("cargo:rerun-if-env-changed=RYEOS_VCS_REF");
    println!("cargo:rerun-if-env-changed=RYEOS_BUILD_DATE");

    // Prefer an explicitly injected ref (container builds pass it through
    // `--build-arg VCS_REF=...`), else the local git short SHA, else "unknown".
    let vcs_ref = std::env::var("RYEOS_VCS_REF")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            // Exported sources nested in another checkout must not inherit its ref.
            let expected_root = manifest_dir.join("../../..").canonicalize().ok()?;
            let actual_root = PathBuf::from(git(&manifest_dir, &["rev-parse", "--show-toplevel"])?)
                .canonicalize()
                .ok()?;
            if actual_root != expected_root {
                return None;
            }
            watch_git(&manifest_dir);
            git(&manifest_dir, &["rev-parse", "--short=12", "HEAD"])
        })
        .unwrap_or_else(|| "unknown".to_string());

    // Build date is injected by container builds; local dev leaves it unknown
    // rather than baking a nondeterministic timestamp into every rebuild.
    let build_date = std::env::var("RYEOS_BUILD_DATE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=RYEOS_VCS_REF={vcs_ref}");
    println!("cargo:rustc-env=RYEOS_BUILD_DATE={build_date}");
}
