use std::process::Command;

fn main() {
    // Re-run when HEAD moves or the provenance env vars change so the embedded
    // revision/date stay accurate across rebuilds.
    println!("cargo:rerun-if-changed=../../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../../.git/refs");
    println!("cargo:rerun-if-env-changed=RYEOS_VCS_REF");
    println!("cargo:rerun-if-env-changed=RYEOS_BUILD_DATE");

    // Prefer an explicitly injected ref (container builds pass it through
    // `--build-arg VCS_REF=...`), else the local git short SHA, else "unknown".
    let vcs_ref = std::env::var("RYEOS_VCS_REF")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
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
