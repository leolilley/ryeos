fn main() {
    println!("cargo:rerun-if-env-changed=PYO3_PYTHON");

    // Forward Cargo's TARGET (the rustc target triple this crate is being
    // compiled for) so the daemon can resolve `bin/<host_triple>/<name>` in
    // bundle manifests. Matches `rustc -vV | grep ^host:`, which is what the
    // build-bundle pipeline writes (see `crates/tools/core-tools/tests/build_bundle_smoke.rs`
    // and `bundles/<name>/.ai/bin/<triple>/`).
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=RYEOSD_HOST_TRIPLE={target}");
    println!("cargo:rerun-if-env-changed=TARGET");

    // The test-only callback probe is copied into a project and captured whole
    // by command isolation, which refuses a file over its per-file limit. Its
    // debug sections dwarf its code (dependencies carry line tables), so drop
    // them at link time for this one binary; the daemon keeps its line tables.
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux") => println!("cargo:rustc-link-arg-bin=ryeos-callback-probe=-Wl,--strip-debug"),
        Ok("macos") => println!("cargo:rustc-link-arg-bin=ryeos-callback-probe=-Wl,-S"),
        _ => {}
    }
}
