//! Minimal target bridge for admitted persistent sessions.
//!
//! The bridge resolves one workspace-relative executable and replaces itself
//! with it. It has no RyeOS configuration, item, network, or filesystem
//! discovery surface; bundle publication requires this binary to be fully
//! static so the bridge itself adds no ambient dynamic-loader closure.

use std::path::{Component, Path, PathBuf};

fn main() {
    if let Err(error) = run(std::env::args_os().skip(1)) {
        eprintln!("ryeos-session-exec: {error}");
        std::process::exit(126);
    }
}

fn run(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    let executable = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "missing workspace-relative executable".to_owned())?;
    if executable.as_os_str().is_empty()
        || executable.is_absolute()
        || executable
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("executable must be one non-empty workspace-relative path".to_owned());
    }

    let workspace = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("resolve admitted session workspace: {error}"))?;
    let target = std::fs::canonicalize(workspace.join(&executable)).map_err(|error| {
        format!(
            "resolve admitted session executable {}: {error}",
            executable.display()
        )
    })?;
    if !strict_child(&workspace, &target) || !target.is_file() {
        return Err(
            "session executable resolves outside the admitted workspace or is not a file"
                .to_owned(),
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = std::process::Command::new(&target).args(arguments).exec();
        Err(format!(
            "exec admitted session target {}: {error}",
            target.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = (target, arguments);
        Err("persistent session execution requires Unix descriptor inheritance".to_owned())
    }
}

fn strict_child(root: &Path, candidate: &Path) -> bool {
    candidate != root && candidate.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_path_is_strictly_workspace_relative() {
        assert!(strict_child(
            Path::new("/workspace"),
            Path::new("/workspace/bin/tool")
        ));
        assert!(!strict_child(
            Path::new("/workspace"),
            Path::new("/workspace")
        ));
        assert!(!strict_child(
            Path::new("/workspace"),
            Path::new("/outside/tool")
        ));
    }
}
