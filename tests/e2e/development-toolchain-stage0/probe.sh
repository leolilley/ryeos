#!/usr/bin/env bash
# Finite offline qualification fixture, NOT a worker command wrapper. Run only
# in a disposable publisher with the verified artifact mounted read-only at
# its declared path and this fixture mounted at /fixture. This demonstrates
# toolchain behavior; only admitted RyeOS Tools can prove Lillux confinement.
set -euo pipefail
platform=/ryeos/realizations/platform
[[ -f "$platform/RYEOS-ELF-TRANSFORMS" && -f /fixture/Cargo.toml ]]
[[ ! -e /tmp/project && ! -e /tmp/target && ! -e /tmp/cargo ]]
mkdir /tmp/project /tmp/target /tmp/cargo
cp -a /fixture/. /tmp/project/
runpath="$platform/lib:$platform/rust/lib:$platform/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib"
# libc.so is a verified DSO alias, not an ambient-path linker script. The
# nonshared archive is a required part of glibc's link contract, not a shim.
flags="-C linker=$platform/native/bin/gcc -C link-arg=-B$platform/native/bin/ -C link-arg=-B$platform/lib/ -C link-arg=--sysroot=$platform/sysroot -C link-arg=-fno-use-linker-plugin -C link-arg=-fuse-ld=lld -C link-arg=-Wl,--dynamic-linker,$platform/lib/ld-linux-x86-64.so.2 -C link-arg=-Wl,-rpath,$runpath -C link-arg=-Wl,-z,nodefaultlib -C link-arg=$platform/lib/libc_nonshared.a"
environment=(env -i
    "PATH=$platform/native/bin:$platform/rust/bin"
    "CARGO_HOME=/tmp/cargo" "CARGO_TARGET_DIR=/tmp/target"
    "RUSTC=$platform/rust/bin/rustc" "RUSTDOC=$platform/rust/bin/rustdoc"
    "RUSTFLAGS=$flags" "LIBRARY_PATH=$platform/lib"
    "COMPILER_PATH=$platform/native/bin" "PROBE_ZIG=$platform/zig/zig"
    "ZIG_GLOBAL_CACHE_DIR=/tmp/zig-cache" "ZIG_LOCAL_CACHE_DIR=/tmp/zig-local")
cd /tmp/project
# All dependencies are fixture-local. No registry acquisition or Cargo cache is
# needed to create its lockfile. Every compilation then enforces all three flags.
"${environment[@]}" "$platform/rust/bin/cargo" generate-lockfile --offline
"${environment[@]}" "$platform/rust/bin/cargo" test --locked --frozen --offline --workspace
"$platform/lib/ld-linux-x86-64.so.2" --list /tmp/target/debug/linkage-probe
