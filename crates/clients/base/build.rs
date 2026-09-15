fn main() {
    // Git is a review/export projection, not a build input. Official builds
    // inject the release version explicitly; source-local builds carry no
    // fabricated release identity and the UI labels that state honestly.
    println!("cargo:rerun-if-env-changed=RYEOS_BUILD_VERSION");
    if let Ok(version) = std::env::var("RYEOS_BUILD_VERSION") {
        if version.is_empty() || version.bytes().any(|byte| byte.is_ascii_control()) {
            panic!("RYEOS_BUILD_VERSION is not canonical");
        }
        println!("cargo:rustc-env=RYEOS_BUILD_VERSION={version}");
    }
}
