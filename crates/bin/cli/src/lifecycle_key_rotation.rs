//! `ryeos node operator rotate` and `ryeos node authoring rotate`: request
//! builders for the signed offline key-rotation operations.
//!
//! The command stages a new key inside the key's home, has the key being
//! replaced sign a succession statement naming it, and applies the rotation
//! operation to the stopped local node. A running node refuses with
//! `node_running_trust_change`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use ryeos_node::key_rotation::{RotatedKey, prepare_rotation};
use serde_json::json;

use crate::lifecycle_commands::parse_or_render_help;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node operator rotate",
    about = "Rotate the operator key on a stopped node",
    long_about = "Generates a new operator key, records the succession from the old key to the new in the signer registry (the old key is retired and still verifies what it signed), re-issues the operator's authorized-key grant under the new key, replaces the operator genesis and reseals the init completion. The old private key is destroyed when the new key is installed.",
    no_binary_name = true
)]
struct OperatorArgs {
    #[arg(long)]
    app_root: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node authoring rotate",
    about = "Rotate the authoring key on a stopped node",
    long_about = "Generates a new authoring key and records the succession from the old key to the new in the signer registry. The new key takes over the old key's scope; the old key is retired and items it signed keep verifying.",
    no_binary_name = true
)]
struct AuthoringArgs {
    #[arg(long)]
    app_root: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

pub fn run_operator(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<OperatorArgs>(argv, console)? else {
        return Ok(());
    };
    rotate(RotatedKey::Operator, args.app_root, args.json, console)
}

pub fn run_authoring(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<AuthoringArgs>(argv, console)? else {
        return Ok(());
    };
    rotate(RotatedKey::Authoring, args.app_root, args.json, console)
}

fn rotate(
    key: RotatedKey,
    app_root: Option<PathBuf>,
    json: bool,
    console: &crate::tty::Console,
) -> Result<()> {
    let app_root = ryeos_node::NodeConfig::load_local(app_root)?.app_root;
    rotate_at(key, &app_root, json, console)
}

fn rotate_at(
    key: RotatedKey,
    app_root: &std::path::Path,
    json: bool,
    console: &crate::tty::Console,
) -> Result<()> {
    ryeos_node::trust_registry::refuse_while_running(app_root).map_err(anyhow::Error::new)?;
    let prepared = prepare_rotation(app_root, key)?;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        app_root,
        key.operation(),
        json!({}),
        BTreeMap::from([("succession".to_owned(), prepared.statement)]),
        BTreeMap::new(),
    )?;
    if json {
        return crate::tty::write_json(
            &json!({ "status": "applied", "key": key.name(), "result": applied.result }),
        )
        .map_err(Into::into);
    }
    console.text(&format!(
        "{} key rotated: {} -> {}\n",
        key.name(),
        prepared.old_fingerprint,
        prepared.new_fingerprint
    ))?;
    if let Some(digest) = applied.result["registry_digest"].as_str() {
        console.text(&format!("registry digest {digest}\n"))?;
    }
    if key == RotatedKey::Operator {
        console.text(
            "the old operator key is retired and its private key is gone; start the node with `ryeos start`\n",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console() -> crate::tty::Console {
        crate::tty::Console::new(crate::tty::TerminalCapabilities::plain(80))
    }

    #[test]
    fn rotation_refuses_while_the_node_is_running() {
        let root = tempfile::tempdir().unwrap();
        let _held = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(root.path()),
        )
        .unwrap();
        for key in [RotatedKey::Operator, RotatedKey::Authoring] {
            let error = rotate_at(key, root.path(), false, &console()).unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains("node_running_trust_change"), "{text}");
            assert!(text.contains("offline-operation submit --stop"), "{text}");
        }
    }

    #[test]
    fn rotation_without_a_key_names_init() {
        let root = tempfile::tempdir().unwrap();
        let error = rotate_at(RotatedKey::Authoring, root.path(), false, &console()).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("key_rotation_no_current_key"), "{text}");
        assert!(text.contains("ryeos init"), "{text}");
    }
}
