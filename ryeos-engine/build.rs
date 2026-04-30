fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo:rustc-env=RYEOS_ENGINE_HOST_TRIPLE={target}");
    println!("cargo:rerun-if-env-changed=TARGET");
}
