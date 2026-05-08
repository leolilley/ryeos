//! One-off helper to migrate kind schemas to handler refs and re-sign.
//!
//! Reads each *.kind-schema.yaml under
//! ryeos-bundles/core/.ai/node/engine/kinds/, strips the signature
//! line, rewrites the `composer:` field from the legacy native-handler
//! ID style (`ryeos/core/<name_with_underscores>`) to the canonical
//! handler ref (`handler:ryeos/core/<name-with-hyphens>`), and re-signs
//! with the platform-author key.
//!
//! Run with:
//!   cargo run -p ryeos-tools --example migrate_composer_refs

use std::fs;
use std::path::PathBuf;

use lillux::crypto::load_signing_key;
use lillux::signature::{sign_content, strip_signature_lines};

fn main() {
    let key_path = PathBuf::from(format!(
        "{}/.ai/config/keys/signing/private_key.pem",
        std::env::var("HOME").unwrap()
    ));
    let sk = load_signing_key(&key_path).expect("load signing key");

    let kinds_dir = PathBuf::from("ryeos-bundles/core/.ai/node/engine/kinds");
    let kinds = [
        "config", "directive", "graph", "handler", "knowledge",
        "node", "parser", "runtime", "service", "tool",
    ];

    let mappings = [
        ("ryeos/core/extends_chain", "handler:ryeos/core/extends-chain"),
        (
            "ryeos/core/graph_permissions",
            "handler:ryeos/core/graph-permissions",
        ),
        ("ryeos/core/identity", "handler:ryeos/core/identity"),
    ];

    for kind in kinds {
        let path = kinds_dir
            .join(kind)
            .join(format!("{kind}.kind-schema.yaml"));
        let raw = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let body = strip_signature_lines(&raw);

        let mut new_body = body.clone();
        let mut changed = false;
        for (from, to) in mappings {
            let from_line = format!("composer: {from}\n");
            let to_line = format!("composer: {to}\n");
            if new_body.contains(&from_line) {
                new_body = new_body.replace(&from_line, &to_line);
                changed = true;
                println!("{}: {from} -> {to}", path.display());
                break;
            }
        }
        if !changed {
            println!("{}: no change", path.display());
            continue;
        }

        let signed = sign_content(&new_body, &sk, "#", None);
        fs::write(&path, signed).expect("write signed");
    }
}
