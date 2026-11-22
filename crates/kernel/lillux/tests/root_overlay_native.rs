//! Native root-overlay probe. Enable explicitly with LILLUX_ROOT_OVERLAY_NATIVE=1
//! as effective uid 0 on Linux; otherwise the probe reports a skip and passes.
//! The test binary re-executes itself so the namespace entry happens in a
//! fresh single-threaded process.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "linux")]
fn child(args: &[String]) -> Result<(), String> {
    use lillux::{PinnedDirectory, RootOverlay, TmpfsLimits, enter_root_overlay_namespace};
    let [source, rw, ro, scratch] = args else {
        return Err("child expects source, rw, ro and scratch paths".into());
    };
    let source_dir = PinnedDirectory::open(Path::new(source))
        .map_err(|error| error.to_string())?
        .ok_or("source absent")?;
    let rw_target = Path::new(rw);
    let ro_target = Path::new(ro);
    let scratch_target = Path::new(scratch);
    enter_root_overlay_namespace(&[
        RootOverlay::Bind {
            source: &source_dir,
            target: rw_target,
            read_only: false,
        },
        RootOverlay::Bind {
            source: &source_dir,
            target: ro_target,
            read_only: true,
        },
        RootOverlay::Tmpfs {
            target: scratch_target,
            limits: TmpfsLimits {
                size_bytes: 64 * 1024,
                inodes: 16,
            },
        },
    ])?;
    if std::fs::read(rw_target.join("seed")).map_err(|error| error.to_string())? != b"seed" {
        return Err("writable overlay does not show source content".into());
    }
    std::fs::write(rw_target.join("inside"), b"inside").map_err(|error| error.to_string())?;
    if std::fs::write(ro_target.join("refused"), b"x").is_ok() {
        return Err("read-only overlay accepted a write".into());
    }
    if std::fs::read(ro_target.join("inside")).map_err(|error| error.to_string())? != b"inside" {
        return Err("read-only overlay does not alias the same source".into());
    }
    std::fs::write(scratch_target.join("small"), vec![1_u8; 4096])
        .map_err(|error| error.to_string())?;
    if std::fs::write(scratch_target.join("large"), vec![1_u8; 1024 * 1024]).is_ok() {
        return Err("tmpfs overlay exceeded its size bound".into());
    }
    if lillux::enter_root_overlay_namespace(&[RootOverlay::Tmpfs {
        target: Path::new("/tmp"),
        limits: TmpfsLimits {
            size_bytes: 0,
            inodes: 1,
        },
    }])
    .is_ok()
    {
        return Err("zero tmpfs limits were accepted".into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn child(_args: &[String]) -> Result<(), String> {
    Err("root overlay probe is Linux only".into())
}

#[cfg(target_os = "linux")]
fn parent() -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("root overlay native probe skipped; requires effective uid 0");
        return Ok(());
    }
    let work = tempfile::tempdir().map_err(|error| error.to_string())?;
    let make = |name: &str| -> Result<PathBuf, String> {
        let path = work.path().join(name);
        std::fs::create_dir(&path).map_err(|error| error.to_string())?;
        Ok(path)
    };
    let (source, rw, ro, scratch) = (make("source")?, make("rw")?, make("ro")?, make("scratch")?);
    std::fs::write(source.join("seed"), b"seed").map_err(|error| error.to_string())?;
    let status = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .arg("child")
        .args([&source, &rw, &ro, &scratch])
        .status()
        .map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("overlay child failed: {status}"));
    }
    for (label, dir) in [("rw", &rw), ("ro", &ro), ("scratch", &scratch)] {
        let count = std::fs::read_dir(dir)
            .map_err(|error| error.to_string())?
            .count();
        if count != 0 {
            return Err(format!("{label} target changed outside the namespace"));
        }
    }
    if std::fs::read(source.join("inside")).map_err(|error| error.to_string())? != b"inside" {
        return Err("writable overlay did not write through to its source".into());
    }
    if source.join("refused").exists() {
        return Err("read-only overlay changed the source".into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn parent() -> Result<(), String> {
    eprintln!("root overlay native probe skipped; Linux only");
    Ok(())
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let result = if args.get(1).is_some_and(|arg| arg == "child") {
        child(&args[2..])
    } else if args.iter().any(|arg| arg == "--list") {
        println!("root_overlay_native: test");
        Ok(())
    } else if std::env::var("LILLUX_ROOT_OVERLAY_NATIVE").as_deref() == Ok("1") {
        parent()
    } else {
        eprintln!("root overlay native probe skipped; set LILLUX_ROOT_OVERLAY_NATIVE=1");
        Ok(())
    };
    if let Err(error) = result {
        eprintln!("root overlay native probe failed: {error}");
        std::process::exit(1);
    }
}
