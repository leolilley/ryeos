// Qualification fixture only: no RyeOS runtime/compiler adapter is built.
fn main() {
    // Explicit Cargo targets must keep static target flags off host programs.
    assert!(!cfg!(target_feature = "crt-static"));
    let out = std::env::var("OUT_DIR").unwrap();
    let zig = std::env::var("PROBE_ZIG").unwrap();
    let object = format!("{out}/native.o");
    assert!(
        std::process::Command::new(&zig)
            .args([
                "cc",
                "-target",
                "x86_64-linux-gnu",
                "-c",
                "native.c",
                "-o",
                &object
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new(&zig)
            .args(["ar", "rcs", &format!("{out}/libnative.a"), &object])
            .status()
            .unwrap()
            .success()
    );
    let target_static = std::env::var("CARGO_CFG_TARGET_FEATURE")
        .unwrap_or_default()
        .split(',')
        .any(|feature| feature == "crt-static");
    std::fs::write(
        format!("{out}/generated.rs"),
        format!("const EXPECTED: i32 = 42;\nconst EXPECTED_STATIC: bool = {target_static};\n"),
    ).unwrap();
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=native");
}
