# ryeos:signed:2026-09-23T04:29:46Z:9ba854f186b09d453cf57c239480dd5fe6ffd62403993559eb5adf7b4d17342c:CffgY0nLqHxLangUqZA35JpS/KADIf9R2EBuvVjQ75Bud5E6fK48YVdF3UeMjFYX2FxXxnUhk1/hhoW4y+yCDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Declared native Cargo environment, including host build-script linkage."""
from pathlib import Path

PLATFORM = "/ryeos/realizations/platform"
TARGET = "x86_64-unknown-linux-gnu"


def validate_source_configuration(root):
    root = Path(root).resolve(strict=True)
    for directory in (root, *root.parents):
        for name in ("config", "config.toml"):
            candidate = directory / ".cargo" / name
            if candidate.exists() or candidate.is_symlink():
                raise ValueError("release source ancestors may not provide Cargo configuration")


def build_environment(private_root, triple, static_sysroot=None):
    if triple != TARGET:
        raise ValueError("release toolchain requires its qualified x86-64 GNU target")
    private_root = Path(private_root)
    if not private_root.is_absolute():
        raise ValueError("private build environment must be absolute")
    for name in ("home", "cargo", "xdg", "tmp", "linker", "zig-global", "zig-local"):
        (private_root / name).mkdir(parents=True, exist_ok=False)
    git_config = private_root / "gitconfig"
    git_config.write_text("")
    static_lib = None
    sysroot_spec = f"--sysroot={PLATFORM}/sysroot"
    if static_sysroot is not None:
        static_sysroot = str(static_sysroot)
        if static_sysroot != "/ryeos/realizations/static-link-inputs":
            raise ValueError("static inputs require the declared realization mount")
        static_lib = static_sysroot + "/usr/lib/x86_64-linux-gnu"
        sysroot_spec = (f"%{{static|static-pie:--sysroot={static_sysroot}}} "
                        f"%{{!static:%{{!static-pie:--sysroot={PLATFORM}/sysroot}}}}")
    # GCC's native specs mechanism supplies host linkage too: Cargo deliberately
    # does not apply target RUSTFLAGS to build scripts/proc macros with --target.
    # This is recipe data, never an executable wrapper or a rewritten compiler.
    specs = f"""*self_spec:
-fno-use-linker-plugin %<fuse-ld=* {sysroot_spec}

*linker:
{PLATFORM}/native/bin/ld.lld

*startfile_prefix_spec:
{PLATFORM}/lib/ {static_lib + '/' if static_lib else ''}

*link:
+ --sysroot=%R %{{!shared:%{{!static:%{{!static-pie:-dynamic-linker {PLATFORM}/lib/ld-linux-x86-64.so.2}}}}}}

*endfile:
+ %{{!static:%{{!static-pie:{PLATFORM}/lib/libc_nonshared.a}}}}

"""
    (private_root / "linker/specs").write_text(specs)
    return {
        "PATH": PLATFORM + "/native/bin:" + PLATFORM + "/rust/bin",
        "HOME": str(private_root / "home"),
        "CARGO_HOME": str(private_root / "cargo"),
        "XDG_CONFIG_HOME": str(private_root / "xdg"),
        "GIT_CONFIG_GLOBAL": str(git_config), "GIT_CONFIG_SYSTEM": str(git_config),
        "GIT_CONFIG_NOSYSTEM": "1",
        "RUSTUP_HOME": str(private_root / "rustup-disabled"),
        "TMPDIR": str(private_root / "tmp"),
        "RUSTC": PLATFORM + "/rust/bin/rustc", "RUSTDOC": PLATFORM + "/rust/bin/rustdoc",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER": PLATFORM + "/native/bin/gcc",
        "GCC_EXEC_PREFIX": str(private_root / "linker") + "/",
        "COMPILER_PATH": PLATFORM + "/native/bin",
        "LIBRARY_PATH": PLATFORM + "/lib" + (":" + static_lib if static_lib else ""),
        "LD_LIBRARY_PATH": ":".join((PLATFORM + "/lib", PLATFORM + "/rust/lib",
                                     PLATFORM + "/rust/lib/rustlib/" + TARGET + "/lib")),
        "AR": PLATFORM + "/native/bin/ar",
        "CC": PLATFORM + "/zig/zig cc -target x86_64-linux-gnu",
        "CXX": PLATFORM + "/zig/zig c++ -target x86_64-linux-gnu",
        # cc-rs appends its LLVM/Rust target spelling after CC's arguments.
        # Its documented final environment flags select Zig's target spelling
        # without disabling optimization/PIC/default flags or adding a wrapper.
        "CFLAGS": "--target=x86_64-linux-gnu",
        "CXXFLAGS": "--target=x86_64-linux-gnu",
        "ZIG_GLOBAL_CACHE_DIR": str(private_root / "zig-global"),
        "ZIG_LOCAL_CACHE_DIR": str(private_root / "zig-local"),
        "RUSTFLAGS": "",
        "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "TZ": "UTC",
        "PYTHONHASHSEED": "0", "SOURCE_DATE_EPOCH": "1",
        "CARGO_NET_OFFLINE": "true", "CARGO_INCREMENTAL": "0",
    }
