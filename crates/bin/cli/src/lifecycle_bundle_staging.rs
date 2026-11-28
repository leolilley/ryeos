//! `ryeos node bundle-staging apply`: the request builder for the signed
//! offline operation that activates a staged bundle.
//!
//! The command signs the request with the node's own operator key and queues
//! it in the node's inbox; it applies nothing. The daemon applies it at its
//! next start, before any other startup check, and the receipt (applied or
//! refused, with its closed code) is read with
//! `ryeos node offline-operation list`. A bundle is activated only by the
//! local operator: a key holding a delegated grant cannot sign this operation.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use ryeos_node::offline_operation::bundle_apply::{
    BundleApplyStagedArguments, bundle_set_after, installed_bundle_names,
};
use ryeos_node::offline_operation::deliver::{DeliveryError, accept_local, local_facts_report};
use ryeos_node::offline_operation::facts::MANDATORY_FACTS;
use ryeos_node::offline_operation::sign::{SignDraft, sign_request_now};
use ryeos_node::offline_operation::table::lookup;
use serde_json::{Value, json};

use crate::lifecycle_commands::parse_or_render_help;

const OPERATION: &str = "node.bundle.apply-staged";

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node bundle-staging apply",
    about = "Queue the signed request that activates a staged bundle at the node's next start",
    long_about = "Signs a `node.bundle.apply-staged` request with this node's operator key and queues it. Nothing is applied now. At its next start the node reads the stage, verifies its node-signed record and evidence, copies the tree into its own install staging directory, recomputes the tree digest on that copy, verifies the bundle's signers against its trust store as it stands then, installs it by the same path as `bundle install`, checks the digest again after the tree is in place, and removes the stage. The request pins the bundle set that results, so a bundle installed or removed in between refuses it. A refusal is a receipt with a closed code (see `ryeos node offline-operation list`), the stage stays and shows as apply_refused to the operator, and the node boots without the bundle. Only the node's own operator key can sign this operation. A stage that will not be applied is removed while the node runs with `ryeos bundle stage-clear`.",
    no_binary_name = true
)]
struct ApplyArgs {
    /// The bundle's name, as `bundle list` shows it for the stage.
    #[arg(long)]
    name: String,

    /// The stage's tree digest, as `bundle list` shows it (64 lowercase hex).
    #[arg(long)]
    digest: String,

    /// Install over an installed bundle of that name. Without it an installed
    /// name is refused.
    #[arg(long)]
    replace: bool,

    /// The operator signing key (PKCS#8 PEM). Defaults to the operator key
    /// under the app root.
    #[arg(long)]
    signing_key: Option<PathBuf>,

    /// How long the request stays valid, in minutes. Defaults to 60; at most
    /// 1440.
    #[arg(long)]
    lifetime_minutes: Option<u32>,

    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

pub fn run_apply(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<ApplyArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let key_path = crate::lifecycle_offline_operation::operator_key_path(
        Some(app_root.clone()),
        args.signing_key.clone(),
    )?;
    let key = lillux::crypto::load_signing_key(&key_path)
        .with_context(|| format!("load the signing key {}", key_path.display()))?;
    let signer = lillux::signature::compute_fingerprint(&key.verifying_key());

    let report = local_facts_report(&app_root, Some(&signer))?;
    if report.pointer("/signer/local_operator") != Some(&Value::Bool(true)) {
        bail!(
            "{signer} is not this node's own operator key; only the local operator may sign {OPERATION}"
        );
    }
    let node_fingerprint = report
        .get("node_fingerprint")
        .and_then(Value::as_str)
        .context("the node's facts do not name the node")?
        .to_owned();
    let spec = lookup(OPERATION).context("this build has no staged-bundle apply operation")?;
    let facts = report
        .get("facts")
        .and_then(Value::as_object)
        .context("the node's facts report has no `facts` object")?;
    let mut pins = BTreeMap::new();
    for fact in MANDATORY_FACTS.iter().chain(spec.preconditions) {
        let value = facts
            .get(*fact)
            .and_then(Value::as_str)
            .with_context(|| format!("the node's facts do not report `{fact}`"))?;
        pins.insert((*fact).to_owned(), value.to_owned());
    }

    let installed = installed_bundle_names(&app_root)?;
    let arguments = BundleApplyStagedArguments {
        name: args.name.clone(),
        staged_digest: args.digest.clone(),
        replace: args.replace,
        expected_bundle_set: bundle_set_after(&installed, &args.name, &args.digest)?,
    };
    arguments.validate()?;
    let delivery = sign_request_now(
        SignDraft {
            node_fingerprint,
            operation: OPERATION.to_owned(),
            arguments: serde_json::to_value(&arguments)?,
            inputs: BTreeMap::new(),
            expected_result: BTreeMap::new(),
            pins,
            lifetime_minutes: args.lifetime_minutes,
            remote: None,
        },
        &key,
    )?;
    let accepted = accept_local(&app_root, &delivery.request, &BTreeMap::new()).map_err(
        |error| match error {
            DeliveryError::Refused(refusal) => {
                anyhow::anyhow!("the node refused the request: {refusal}")
            }
            DeliveryError::Failed(error) => error.context("queue the request"),
        },
    )?;

    if args.json {
        return crate::tty::write_json(&json!({
            "status": "queued",
            "operation": accepted.operation,
            "request_digest": accepted.request_digest,
            "ledger_head": accepted.ledger_head,
            "already_queued": accepted.already_queued,
            "name": arguments.name,
            "staged_digest": arguments.staged_digest,
            "replace": arguments.replace,
            "expected_bundle_set": arguments.expected_bundle_set,
        }))
        .map_err(Into::into);
    }
    let mut banner = crate::tty::StatusBanner::new(
        crate::tty::Tone::Success,
        if accepted.already_queued {
            "ALREADY QUEUED"
        } else {
            "QUEUED"
        },
    );
    banner.detail = Some(
        "nothing is installed yet; the node applies the request at its next start and records a receipt, applied or refused"
            .to_owned(),
    );
    banner.rows = vec![
        crate::tty::Row::key_value("bundle", arguments.name.clone()),
        crate::tty::Row::key_value("stage", arguments.staged_digest.clone()),
        crate::tty::Row::key_value("replace", arguments.replace.to_string()),
        crate::tty::Row::key_value("request", accepted.request_digest.clone()),
    ];
    console.status(&banner)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operation_the_sugar_signs_is_in_the_compiled_table() {
        let spec = lookup(OPERATION).unwrap();
        assert!(spec.local_signer_only);
    }

    #[test]
    fn help_names_the_flags() {
        let help = <ApplyArgs as clap::CommandFactory>::command()
            .render_long_help()
            .to_string();
        for flag in ["--name", "--digest", "--replace", "--signing-key", "--json"] {
            assert!(help.contains(flag), "{flag}");
        }
    }
}
