//! Explicit Docker runtime adapter. Docker/containerd retains lifecycle ownership;
//! this adapter attaches the installed host hooks and then replaces itself with
//! the administrator-selected runc executable. It never creates a delegation.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

const MAX_SPEC_BYTES: u64 = 1_048_576;

pub(super) fn run(arguments: Vec<OsString>) -> Result<()> {
    if arguments.len() < 4 || arguments[0] != "--runc" || arguments[2] != "--" {
        bail!(
            "usage: ryeos-lillux-oci-hook docker-runtime --runc /absolute/path/runc -- <runc arguments>"
        );
    }
    let runtime = Path::new(&arguments[1]);
    require_executable(runtime)?;
    let hook = std::env::current_exe()?;
    if runtime == hook {
        bail!("Docker runtime delegate cannot be the hook executable");
    }
    let delegated: Vec<String> = arguments[3..]
        .iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .context("runtime argument is not UTF-8")
        })
        .collect::<Result<_>>()?;
    if let Some(bundle) = creation_bundle(&delegated)? {
        install_hooks(&bundle, &hook)?;
    }
    Err(std::process::Command::new(runtime)
        .args(&arguments[3..])
        .exec())
    .context("execute installed runc")
}

fn require_executable(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("runtime executable must be absolute");
    }
    let directory = lillux::PinnedDirectory::open_owned_hierarchy(
        path.parent().context("runtime has no parent")?,
        0,
    )?
    .context("runtime executable parent is absent")?;
    let file = directory
        .open_pinned_regular(path.file_name().context("runtime has no filename")?, false)?
        .context("runtime executable is absent")?;
    file.require_owner(0)?;
    file.require_executable()?;
    Ok(())
}

/// Parse only the runc command boundary, not arbitrary occurrences of `create`
/// in log paths or container names. Unknown global options fail closed so a new
/// runc CLI cannot accidentally bypass creation interception.
fn creation_bundle(arguments: &[String]) -> Result<Option<PathBuf>> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(a, b)| (a, Some(b)));
        match flag {
            "--root" | "--log" | "--log-format" | "--rootless" => {
                if inline.is_none() {
                    index += 1;
                    arguments
                        .get(index)
                        .context("missing runc global option value")?;
                }
            }
            "--debug" | "--systemd-cgroup" if inline.is_none() => {}
            "--version" | "-v" | "--help" | "-h" if inline.is_none() => return Ok(None),
            _ if argument.starts_with('-') => bail!("unsupported runc global option: {argument}"),
            _ => break,
        }
        index += 1;
    }
    let command = arguments.get(index).context("missing runc command")?;
    match command.as_str() {
        "restore" | "checkpoint" | "run" => bail!(
            "contained Docker runtime supports create/start lifecycle only; {command} is unsupported"
        ),
        "create" => {}
        "start" | "delete" | "kill" | "state" | "exec" | "list" | "ps" | "events" | "pause"
        | "resume" | "update" | "features" | "help" => return Ok(None),
        _ => bail!("unsupported runc command: {command}"),
    }
    let mut bundle = None;
    let mut container = None;
    index += 1;
    while let Some(argument) = arguments.get(index) {
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(a, b)| (a, Some(b)));
        match flag {
            "--bundle" | "-b" | "--console-socket" | "--pid-file" | "--preserve-fds" => {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        index += 1;
                        arguments
                            .get(index)
                            .context("missing runc create option value")?
                    }
                };
                if matches!(flag, "--bundle" | "-b") {
                    if bundle.is_some() {
                        bail!("duplicate OCI bundle argument");
                    }
                    bundle = Some(PathBuf::from(value));
                }
            }
            "--no-pivot" | "--no-new-keyring" if inline.is_none() => {}
            _ if argument.starts_with('-') => bail!("unsupported runc create option: {argument}"),
            _ => {
                if container.replace(argument).is_some() {
                    bail!("multiple runc container IDs");
                }
            }
        }
        index += 1;
    }
    container.context("missing container ID")?;
    let bundle = bundle.context("contained runtime requires an explicit absolute OCI bundle")?;
    if !bundle.is_absolute() {
        bail!("OCI bundle must be absolute");
    }
    Ok(Some(bundle))
}

fn install_hooks(bundle: &Path, executable: &Path) -> Result<()> {
    let directory = lillux::PinnedDirectory::open_owned_hierarchy(bundle, 0)?
        .context("administrator-owned OCI bundle is absent")?;
    let file = directory
        .open_pinned_regular("config.json".as_ref(), false)?
        .context("OCI bundle config is absent")?;
    file.require_owner(0)?;
    let observation = file.observation()?;
    let mut spec: Value =
        serde_json::from_slice(&file.read_stable_bounded(&observation, MAX_SPEC_BYTES)?)?;
    attach_hooks(&mut spec, executable)?;
    directory.atomic_write_pinned_if_same(
        "config.json".as_ref(),
        Some(&file),
        &serde_json::to_vec(&spec)?,
        0o600,
    )?;
    Ok(())
}

fn attach_hooks(spec: &mut Value, executable: &Path) -> Result<()> {
    let expected = json!([
        "/usr/bin/tini",
        "--",
        "/usr/local/bin/contained-workflow-entrypoint"
    ]);
    if spec.pointer("/process/args") != Some(&expected)
        || spec.pointer("/process/user/uid").and_then(Value::as_u64) != Some(0)
        || spec.pointer("/process/user/gid").and_then(Value::as_u64) != Some(0)
    {
        bail!("contained runtime requires the fixed contained-workflow bootstrap as root");
    }
    let namespaces = spec
        .pointer("/linux/namespaces")
        .and_then(Value::as_array)
        .context("contained runtime requires Linux namespaces")?;
    for required in ["mount", "pid"] {
        let matches: Vec<_> = namespaces
            .iter()
            .filter(|item| item["type"] == required)
            .collect();
        if matches.len() != 1 || matches[0].get("path").is_some() {
            bail!("contained runtime requires a private {required} namespace");
        }
    }
    if namespaces.iter().any(|item| item["type"] == "user") {
        bail!("contained runtime does not support remapped controller accounts");
    }
    let object = spec
        .as_object_mut()
        .context("OCI specification must be an object")?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("OCI hooks must be an object")?;
    let executable = executable.to_str().context("hook path is not UTF-8")?;
    for operation in ["prestart", "poststop"] {
        let hook = json!({"path": executable, "args": [executable, operation], "timeout": 60});
        let list = hooks
            .entry(operation)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("OCI hook list must be an array")?;
        // A repeated create can reuse only our exact existing hook, at the end
        // after any pre-existing runtime preparation. Never add duplicate hooks.
        if list.iter().any(|item| item["path"] == executable) {
            if list.last() != Some(&hook)
                || list
                    .iter()
                    .filter(|item| item["path"] == executable)
                    .count()
                    != 1
            {
                bail!("conflicting installed {operation} hook");
            }
        } else {
            list.push(hook);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn docker_runtime_intercepts_creation_after_global_arguments() {
        assert_eq!(
            creation_bundle(&args(&[
                "--log",
                "create",
                "--root=/run/runc",
                "create",
                "--bundle",
                "/run/bundle",
                "node"
            ]))
            .unwrap(),
            Some(PathBuf::from("/run/bundle"))
        );
        assert_eq!(creation_bundle(&args(&["delete", "create"])).unwrap(), None);
        for values in [
            vec!["restore", "node"],
            vec!["create", "node"],
            vec!["create", "-b", "relative", "node"],
            vec!["--unknown", "create"],
        ] {
            assert!(creation_bundle(&args(&values)).is_err());
        }
    }

    #[test]
    fn docker_runtime_requires_contained_entry_and_private_namespaces() {
        let spec = json!({"process": {"args": ["/usr/bin/tini", "--", "/usr/local/bin/contained-workflow-entrypoint"], "user": {"uid": 0, "gid": 0}}, "linux": {"namespaces": [{"type": "mount"}, {"type": "pid"}]}});
        let path = Path::new("/usr/lib/ryeos/ryeos-lillux-oci-hook");
        let mut accepted = spec.clone();
        attach_hooks(&mut accepted, path).unwrap();
        let first = accepted.clone();
        attach_hooks(&mut accepted, path).unwrap();
        assert_eq!(first, accepted);
        assert_eq!(accepted["hooks"]["prestart"].as_array().unwrap().len(), 1);
        let mut ordinary = spec.clone();
        ordinary["process"]["args"] = json!(["/entrypoint.sh"]);
        assert!(attach_hooks(&mut ordinary, path).is_err());
        let mut shared = spec.clone();
        shared["linux"]["namespaces"][1]["path"] = json!("/proc/1/ns/pid");
        assert!(attach_hooks(&mut shared, path).is_err());
        let mut conflict = first;
        conflict["hooks"]["prestart"][0]["timeout"] = json!(0);
        assert!(attach_hooks(&mut conflict, path).is_err());
    }
}
