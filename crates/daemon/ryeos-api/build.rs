fn main() {
    // Cargo's compilation target, not the machine running this build script.
    // In particular, retain the ABI component (gnu/musl) for cross builds.
    let target = std::env::var("TARGET").expect("Cargo must provide the compilation target");
    println!("cargo:rustc-env=RYEOS_CONSUMER_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
