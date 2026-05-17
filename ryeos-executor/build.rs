fn main() {
    println!("cargo:rerun-if-env-changed=PYO3_PYTHON");

    // Forward Cargo's TARGET (the rustc target triple this crate is being
    // compiled for) so the daemon can resolve `bin/<host_triple>/<name>` in
    // bundle manifests. Matches `rustc -vV | grep ^host:`, which is what the
    // build-bundle pipeline writes (see `ryeos-tools/tests/build_bundle_smoke.rs`
    // and `ryeos-bundles/standard/.ai/bin/<triple>/`).
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=RYEOSD_HOST_TRIPLE={target}");
    println!("cargo:rerun-if-env-changed=TARGET");
}
