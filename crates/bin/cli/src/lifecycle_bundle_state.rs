//! `ryeos node bundle-state {export,import,remove}`: request builders for the
//! signed offline operations that move or delete a bundle's node-owned state.
//!
//! Each verb applies its operation to the stopped local node through the same
//! library the daemon uses at startup; a running node holds the state lock and
//! the operation refuses.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;
use serde_json::json;

use crate::lifecycle_commands::parse_or_render_help;

/// Largest archive an import reads; the operation's input slot allows the same.
const ARCHIVE_MAX_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node bundle-state export",
    about = "Write a node-signed archive of a bundle's state home from a stopped node",
    long_about = "Exports every namespace of one bundle's state home, or the one named, as a tar holding the files and a node-signed manifest (bundle id, source node, file list with sha256 of each file). The node must be stopped. The output path must not already exist.",
    no_binary_name = true
)]
struct ExportArgs {
    /// Bundle whose state to export.
    #[arg(long)]
    bundle: String,

    /// Export only this namespace.
    #[arg(long)]
    namespace: Option<String>,

    /// Absolute path of the archive to create.
    #[arg(long)]
    output: PathBuf,

    #[arg(long)]
    app_root: Option<PathBuf>,

    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node bundle-state import",
    about = "Install a verified state archive into a stopped node",
    long_about = "Installs every namespace of an archive produced by `ryeos node bundle-state export`. The node must be stopped. Refuses unless the archive's manifest verifies under this node's key or a pinned peer node key, the bundle is installed and its signed manifest declares a state home backing each namespace, and each target namespace is absent. An existing namespace is replaced only when named with --replace-namespace.",
    no_binary_name = true
)]
struct ImportArgs {
    /// The archive to install.
    #[arg(long)]
    archive: PathBuf,

    /// Replace this existing namespace (repeatable).
    #[arg(long = "replace-namespace", action = clap::ArgAction::Append)]
    replace_namespaces: Vec<String>,

    #[arg(long)]
    app_root: Option<PathBuf>,

    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node bundle-state remove",
    about = "Delete a bundle's state home, or one namespace, from a stopped node",
    long_about = "Deletes one namespace, or the bundle's whole state home. The node must be stopped. A ledger receipt naming the digest of the directory listing is written before anything is deleted. Removing a bundle never deletes its state; this is the only way to.",
    no_binary_name = true
)]
struct RemoveArgs {
    /// Bundle whose state to delete.
    #[arg(long)]
    bundle: String,

    /// Delete only this namespace.
    #[arg(long)]
    namespace: Option<String>,

    /// Required acknowledgement that the state is deleted.
    #[arg(long)]
    confirm: bool,

    #[arg(long)]
    app_root: Option<PathBuf>,

    #[arg(long)]
    json: bool,
}

fn app_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    Ok(ryeos_node::NodeConfig::load_local(explicit)?.app_root)
}

pub fn run(verb: &str, argv: &[String], console: &crate::tty::Console) -> Result<()> {
    match verb {
        "export" => run_export(argv, console),
        "import" => run_import(argv, console),
        "remove" => run_remove(argv, console),
        other => bail!("unknown bundle-state verb `{other}`"),
    }
}

fn report(
    console: &crate::tty::Console,
    json: bool,
    verb: &str,
    result: &serde_json::Value,
    receipt: &str,
) -> Result<()> {
    if json {
        return crate::tty::write_json(
            &json!({ "status": "applied", "verb": verb, "result": result, "receipt": receipt }),
        )
        .map_err(Into::into);
    }
    console.text(&format!(
        "bundle-state {verb} applied\n{}\nreceipt {receipt}\n",
        serde_json::to_string_pretty(result)?
    ))?;
    Ok(())
}

fn run_export(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<ExportArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = app_root(args.app_root.clone())?;
    let output = args.output.to_str().ok_or_else(|| anyhow::anyhow!("--output is not UTF-8"))?;
    let arguments = ryeos_node::offline_operation::handlers::BundleStateExportArguments {
        bundle_id: args.bundle.clone(),
        namespace: args.namespace.clone(),
        output: output.to_owned(),
    };
    arguments.validate()?;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.bundle-state.export",
        serde_json::to_value(arguments)?,
        BTreeMap::new(),
        BTreeMap::new(),
    )?;
    report(console, args.json, "export", &applied.result, &applied.receipt.digest()?)
}

fn run_import(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<ImportArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = app_root(args.app_root.clone())?;
    let archive =
        crate::offline_apply::read_input(&args.archive, "bundle state archive", ARCHIVE_MAX_BYTES)?;
    let arguments = ryeos_node::offline_operation::handlers::BundleStateImportArguments {
        replace_namespaces: args.replace_namespaces.clone(),
    };
    arguments.validate()?;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.bundle-state.import",
        serde_json::to_value(arguments)?,
        BTreeMap::from([("archive".to_owned(), archive)]),
        BTreeMap::new(),
    )?;
    report(console, args.json, "import", &applied.result, &applied.receipt.digest()?)
}

fn run_remove(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<RemoveArgs>(argv, console)? else {
        return Ok(());
    };
    if !args.confirm {
        bail!("deleting a bundle's state requires --confirm");
    }
    let app_root = app_root(args.app_root.clone())?;
    let arguments = ryeos_node::offline_operation::handlers::BundleStateRemoveArguments {
        bundle_id: args.bundle.clone(),
        namespace: args.namespace.clone(),
    };
    arguments.validate()?;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.bundle-state.remove",
        serde_json::to_value(arguments)?,
        BTreeMap::new(),
        BTreeMap::new(),
    )?;
    report(console, args.json, "remove", &applied.result, &applied.receipt.digest()?)
}
