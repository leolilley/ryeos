//! `ryeos trust {pin,scope,retire,revoke,succeed}`: request builders for the
//! signed offline trust operations.
//!
//! Each verb turns its flags into an operation, arguments and inputs and
//! applies it to the stopped local node through the same library the daemon
//! uses at startup. A running node refuses with `node_running_trust_change`.
//! `ryeos trust list` and `ryeos trust show` are read-only service commands
//! and are not handled here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::json;

use crate::lifecycle_commands::parse_or_render_help;

/// Largest trust or succession document the verbs read; the operations'
/// input slots allow the same.
const DOCUMENT_MAX_BYTES: u64 = 16 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos trust pin",
    about = "Pin a signer key on a stopped node from a PUBLISHER_TRUST.toml",
    long_about = "Writes a node-signed signer record and a key event. The node must be stopped. A publisher defaults to bundle space for every kind; a project author defaults to project space, limited to the --project roots given.",
    no_binary_name = true
)]
struct PinArgs {
    /// The signer's PUBLISHER_TRUST.toml.
    #[arg(long = "from")]
    from: PathBuf,

    /// publisher, operator, project_author or authoring.
    #[arg(long)]
    role: String,

    /// Project root the signer may sign for (repeatable, absolute).
    #[arg(long = "project", action = clap::ArgAction::Append)]
    projects: Vec<String>,

    /// Space the signer may sign in: bundle, project or node (repeatable).
    #[arg(long = "space", action = clap::ArgAction::Append)]
    spaces: Vec<String>,

    /// Item kind the signer may sign (repeatable); default every kind.
    #[arg(long = "kind", action = clap::ArgAction::Append)]
    kinds: Vec<String>,

    #[arg(long)]
    app_root: Option<PathBuf>,

    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos trust scope",
    about = "Narrow or widen the scope of a pinned signer",
    long_about = "Replaces the spaces, kinds and projects a pinned signer may sign for. Flags left out keep the record's current values; give --project to replace the project list.",
    no_binary_name = true
)]
struct ScopeArgs {
    /// Fingerprint of the pinned signer.
    fingerprint: String,

    #[arg(long = "space", action = clap::ArgAction::Append)]
    spaces: Vec<String>,

    #[arg(long = "kind", action = clap::ArgAction::Append)]
    kinds: Vec<String>,

    #[arg(long = "project", action = clap::ArgAction::Append)]
    projects: Vec<String>,

    #[arg(long)]
    app_root: Option<PathBuf>,

    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos trust retire",
    about = "Retire a pinned signer: it still verifies existing items",
    no_binary_name = true
)]
struct RetireArgs {
    fingerprint: String,
    #[arg(long)]
    app_root: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos trust revoke",
    about = "Revoke a pinned signer: it verifies nothing, permanently",
    no_binary_name = true
)]
struct RevokeArgs {
    fingerprint: String,
    #[arg(long)]
    app_root: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos trust succeed",
    about = "Replace a pinned signer by the key named in a statement the old key signed",
    long_about = "The new key takes over the old record's role and scope and the old record is retired.",
    no_binary_name = true
)]
struct SucceedArgs {
    /// The signed succession statement.
    #[arg(long = "from")]
    from: PathBuf,

    /// Fingerprint of the key being replaced.
    #[arg(long = "old")]
    old: String,

    #[arg(long)]
    app_root: Option<PathBuf>,

    #[arg(long)]
    json: bool,
}

fn app_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    Ok(ryeos_node::NodeConfig::load_local(explicit)?.app_root)
}

fn refuse_running(app_root: &std::path::Path) -> Result<()> {
    ryeos_node::trust_registry::refuse_while_running(app_root).map_err(anyhow::Error::new)
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
    let written = result["events_written"].as_u64().unwrap_or(0);
    let records = result["records"].as_array().cloned().unwrap_or_default();
    if written == 0 {
        console.text(&format!(
            "trust {verb}: registry already holds the requested state\n"
        ))?;
    }
    for record in &records {
        console.text(&format!(
            "{} {} ({}, {})\n",
            record["fingerprint"].as_str().unwrap_or("?"),
            record["label"].as_str().unwrap_or("?"),
            record["role"].as_str().unwrap_or("?"),
            record["status"].as_str().unwrap_or("?"),
        ))?;
    }
    console.text(&format!(
        "registry digest {}\n",
        result["registry_digest"].as_str().unwrap_or("?")
    ))?;
    Ok(())
}

pub fn run(verb: &str, argv: &[String], console: &crate::tty::Console) -> Result<()> {
    match verb {
        "pin" => run_pin(argv, console),
        "scope" => run_scope(argv, console),
        "retire" => run_fingerprint::<RetireArgs>(argv, console, "retire", "node.trust.retire"),
        "revoke" => run_fingerprint::<RevokeArgs>(argv, console, "revoke", "node.trust.revoke"),
        "succeed" => run_succeed(argv, console),
        other => bail!("unknown trust verb `{other}`"),
    }
}

fn run_pin(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<PinArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = app_root(args.app_root.clone())?;
    refuse_running(&app_root)?;
    let signer =
        crate::offline_apply::read_input(&args.from, "signer trust document", DOCUMENT_MAX_BYTES)?;
    let arguments = ryeos_node::offline_operation::handlers::TrustPinArguments {
        role: args.role.clone(),
        spaces: args.spaces.clone(),
        kinds: args.kinds.clone(),
        projects: args.projects.clone(),
    };
    arguments.validate()?;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.trust.pin",
        serde_json::to_value(arguments)?,
        BTreeMap::from([("signer".to_owned(), signer)]),
        BTreeMap::new(),
    )?;
    report(console, args.json, "pin", &applied.result)
}

fn run_scope(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<ScopeArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = app_root(args.app_root.clone())?;
    refuse_running(&app_root)?;
    let view = ryeos_node::trust_registry::load_view(&app_root)?;
    let current = view
        .records
        .iter()
        .find(|record| record.fingerprint == args.fingerprint)
        .with_context(|| {
            format!(
                "trust_signer_unknown: no signer {} is pinned. List signers with `ryeos trust list`",
                args.fingerprint
            )
        })?;
    let spaces = if args.spaces.is_empty() {
        current
            .scope
            .spaces
            .iter()
            .map(|s| s.as_str().to_owned())
            .collect()
    } else {
        args.spaces.clone()
    };
    let kinds = if args.kinds.is_empty() {
        current.scope.kinds.clone()
    } else {
        args.kinds.clone()
    };
    let projects = if args.projects.is_empty() {
        current.scope.projects.clone()
    } else {
        args.projects.clone()
    };
    let arguments = ryeos_node::offline_operation::handlers::TrustScopeArguments {
        fingerprint: args.fingerprint.clone(),
        spaces,
        kinds,
        projects,
    };
    arguments.validate()?;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.trust.scope",
        serde_json::to_value(arguments)?,
        BTreeMap::new(),
        BTreeMap::new(),
    )?;
    report(console, args.json, "scope", &applied.result)
}

trait FingerprintArgs: Parser {
    fn fingerprint(&self) -> &str;
    fn app_root(&self) -> Option<PathBuf>;
    fn json(&self) -> bool;
}

macro_rules! fingerprint_args {
    ($name:ident) => {
        impl FingerprintArgs for $name {
            fn fingerprint(&self) -> &str {
                &self.fingerprint
            }
            fn app_root(&self) -> Option<PathBuf> {
                self.app_root.clone()
            }
            fn json(&self) -> bool {
                self.json
            }
        }
    };
}
fingerprint_args!(RetireArgs);
fingerprint_args!(RevokeArgs);

fn run_fingerprint<A: FingerprintArgs>(
    argv: &[String],
    console: &crate::tty::Console,
    verb: &str,
    operation: &str,
) -> Result<()> {
    let Some(args) = parse_or_render_help::<A>(argv, console)? else {
        return Ok(());
    };
    let app_root = app_root(args.app_root())?;
    refuse_running(&app_root)?;
    let arguments = ryeos_node::offline_operation::handlers::TrustFingerprintArguments {
        fingerprint: args.fingerprint().to_owned(),
    };
    arguments.validate()?;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        operation,
        serde_json::to_value(arguments)?,
        BTreeMap::new(),
        BTreeMap::new(),
    )?;
    report(console, args.json(), verb, &applied.result)
}

fn run_succeed(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<SucceedArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = app_root(args.app_root.clone())?;
    refuse_running(&app_root)?;
    let statement =
        crate::offline_apply::read_input(&args.from, "succession statement", DOCUMENT_MAX_BYTES)?;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.trust.succeed",
        serde_json::to_value(
            ryeos_node::offline_operation::handlers::TrustSucceedArguments {
                old_fingerprint: args.old.clone(),
            },
        )?,
        BTreeMap::from([("succession".to_owned(), statement)]),
        BTreeMap::new(),
    )?;
    report(console, args.json, "succeed", &applied.result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_pin_refuses_while_node_running() {
        let root = tempfile::tempdir().unwrap();
        let _held = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(root.path()),
        )
        .unwrap();
        let error = refuse_running(root.path()).unwrap_err();
        assert!(error.to_string().contains("node_running_trust_change"));
        assert!(
            error
                .to_string()
                .contains("offline-operation submit --stop")
        );
    }
}
