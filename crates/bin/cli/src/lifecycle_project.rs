//! `ryeos project {register,unregister}`: request builders for the signed
//! offline project registration operations.
//!
//! A registration records that an owner registered a live checkout under a
//! project name on this node. It is inert until the owner+name trust cut:
//! only `ryeos node doctor` reads it. The node must be stopped; a running
//! node refuses with `node_running_trust_change`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;
use serde_json::json;

use crate::lifecycle_commands::parse_or_render_help;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos project register",
    about = "Register a live checkout under its project name on a stopped node",
    long_about = "Writes a node-signed registration of the checkout under fp:<owner>/<name>. The checkout's .ai/project.yaml must declare the name. The owner defaults to the node's operator. The node must be stopped. Registrations are inert until the owner+name trust cut; at the cut they become what lets a live checkout state its project identity.",
    no_binary_name = true
)]
struct RegisterArgs {
    /// The checkout directory (absolute or relative to the current directory).
    path: PathBuf,

    /// The project name; defaults to the name the checkout declares.
    #[arg(long)]
    name: Option<String>,

    /// The owning principal, fp:<64 hex>; defaults to the node's operator.
    #[arg(long)]
    owner: Option<String>,

    #[arg(long)]
    app_root: Option<PathBuf>,

    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos project unregister",
    about = "Remove a checkout from its project registration on a stopped node",
    long_about = "Removes the checkout from the owner's registration under the project name; the record is deleted when no checkout remains. A checkout that no longer exists can be unregistered by naming --name. The node must be stopped.",
    no_binary_name = true
)]
struct UnregisterArgs {
    /// The checkout directory as it was registered.
    path: PathBuf,

    /// The registered project name; required when the checkout no longer
    /// declares one.
    #[arg(long)]
    name: Option<String>,

    /// The owning principal, fp:<64 hex>; defaults to the node's operator.
    #[arg(long)]
    owner: Option<String>,

    #[arg(long)]
    app_root: Option<PathBuf>,

    #[arg(long)]
    json: bool,
}

fn absolute(path: PathBuf) -> Result<String> {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("the checkout path is not UTF-8"))
}

fn report(
    console: &crate::tty::Console,
    json: bool,
    verb: &str,
    result: &serde_json::Value,
) -> Result<()> {
    if json {
        return crate::tty::write_json(
            &json!({ "status": "applied", "verb": verb, "result": result }),
        )
        .map_err(Into::into);
    }
    let identity = result["identity"].as_str().unwrap_or("?");
    let path = result["path"].as_str().unwrap_or("?");
    let changed = result["changed"].as_bool().unwrap_or(false);
    let verb_text = match (verb, changed) {
        ("register", true) => "registered",
        ("register", false) => "already registered",
        (_, true) => "unregistered",
        (_, false) => "was not registered",
    };
    console.text(&format!("{identity}: {path} {verb_text}\n"))?;
    let paths = result["paths"].as_array().cloned().unwrap_or_default();
    if paths.is_empty() {
        console.text("no checkout remains registered under this name\n")?;
    }
    for path in paths {
        console.text(&format!("  {}\n", path.as_str().unwrap_or("?")))?;
    }
    Ok(())
}

pub fn run(verb: &str, argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let (operation, path, name, owner, app_root, json) = match verb {
        "register" => {
            let Some(args) = parse_or_render_help::<RegisterArgs>(argv, console)? else {
                return Ok(());
            };
            (
                "node.project.register",
                args.path,
                args.name,
                args.owner,
                args.app_root,
                args.json,
            )
        }
        "unregister" => {
            let Some(args) = parse_or_render_help::<UnregisterArgs>(argv, console)? else {
                return Ok(());
            };
            (
                "node.project.unregister",
                args.path,
                args.name,
                args.owner,
                args.app_root,
                args.json,
            )
        }
        other => bail!("unknown project verb `{other}`"),
    };
    let app_root = ryeos_node::NodeConfig::load_local(app_root)?.app_root;
    ryeos_node::trust_registry::refuse_while_running(&app_root).map_err(anyhow::Error::new)?;
    let arguments = ryeos_node::offline_operation::handlers::ProjectRegistrationArguments {
        path: absolute(path)?,
        name,
        owner,
    };
    arguments.validate()?;
    let applied = crate::offline_apply::CliHost::new().apply(
        &app_root,
        operation,
        serde_json::to_value(arguments)?,
        BTreeMap::new(),
        BTreeMap::new(),
    )?;
    report(console, json, verb, &applied.result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_unregister_parse_their_flags() {
        let args = RegisterArgs::try_parse_from(["/work/demo", "--name", "demo"]).unwrap();
        assert_eq!(args.path, PathBuf::from("/work/demo"));
        assert_eq!(args.name.as_deref(), Some("demo"));
        assert!(args.owner.is_none());
        let args = UnregisterArgs::try_parse_from([
            "/gone/demo",
            "--owner",
            "fp:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ])
        .unwrap();
        assert!(args.name.is_none());
        assert!(args.owner.is_some());
        assert!(RegisterArgs::try_parse_from(["--name", "demo"]).is_err());
    }

    #[test]
    fn relative_paths_are_made_absolute_before_signing() {
        let made = absolute(PathBuf::from("some/checkout")).unwrap();
        assert!(PathBuf::from(&made).is_absolute(), "{made}");
        assert!(made.ends_with("some/checkout"), "{made}");
    }
}
