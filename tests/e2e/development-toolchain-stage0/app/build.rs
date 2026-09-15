// Qualification fixture only: no RyeOS runtime/compiler adapter is built.
fn main() {
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
    std::fs::write(format!("{out}/generated.rs"), "const EXPECTED: i32 = 42;\n").unwrap();
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=native");
}
