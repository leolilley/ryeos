//! `ryeos node external-execution recompose`: re-pin installed bindings to
//! the installed bundles after a payload change, on a stopped node.
//!
//! A recompose is an offline operation applied under the node's state lock,
//! like the composes. `--dry-run` is not an operation: it changes nothing, so
//! it holds the existing state lock read-only to prove the daemon is stopped
//! and prints what a recompose would publish.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use ryeos_app::node_config::recompose::{BindingRef, RecomposeReport};
use ryeos_node::offline_operation::handlers::RecomposeArguments;

use crate::lifecycle_external_snapshot_compose::{parse_args, read_bytes};

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node external-execution recompose",
    about = "Re-pin installed runtime_snapshot_production, runtime_snapshot_qualification and \
             external_execution bindings to the installed bundles after a payload change, on a \
             stopped node. Production bindings come first, then the qualifications that name \
             them, then placements. Every operator-stated fact and the credential generation \
             are carried, so no secret is provisioned again. Restart the node to load them.",
    no_binary_name = true
)]
struct RecomposeArgs {
    /// Binding to recompose, as `<section>/<id>` (repeatable). Without any,
    /// every binding whose pins `ryeos node doctor` fails is recomposed.
    #[arg(long = "binding", action = clap::ArgAction::Append, value_parser = BindingRef::parse)]
    bindings: Vec<BindingRef>,

    /// JSON file mapping scenario id to a consumer verifier selection: the
    /// running node's consumer qualification context derive output. Required
    /// when a qualification to recompose selects consumer verifiers.
    #[arg(long)]
    consumer_verifiers: Option<PathBuf>,

    /// Print what would be recomposed; publish nothing.
    #[arg(long)]
    dry_run: bool,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

/// Entry point. `argv` excludes the verb words.
pub fn run_recompose(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_args::<RecomposeArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let consumer_bytes = args
        .consumer_verifiers
        .as_ref()
        .map(|path| read_bytes(path, "consumer verifiers"))
        .transpose()?;
    let report: RecomposeReport = if args.dry_run {
        let consumer_verifiers = consumer_bytes
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .context("parse consumer verifiers")?;
        let state_lock = ryeos_node_runtime::state_lock::StateLock::acquire_existing_read_only(
            &ryeos_node_runtime::state_lock::default_lock_path(&app_root),
        )
        .context("a recompose requires the daemon to be stopped")?;
        ryeos_app::node_config::recompose::recompose_bindings(
            ryeos_app::node_config::recompose::RecomposeRequest {
                app_root: &app_root,
                state_lock: &state_lock,
                bindings: args.bindings.clone(),
                consumer_verifiers,
                dry_run: true,
            },
        )?
    } else {
        let mut inputs = BTreeMap::new();
        if let Some(bytes) = consumer_bytes {
            // Parsed here only to refuse a malformed document before a request
            // is built; the operation parses the same bytes again.
            let _: serde_json::Value =
                serde_json::from_slice(&bytes).context("parse consumer verifiers")?;
            inputs.insert("consumer_verifiers".to_owned(), bytes);
        }
        let host = crate::offline_apply::CliHost::new();
        let applied = host.apply(
            &app_root,
            "node.external-execution.recompose",
            serde_json::to_value(RecomposeArguments {
                bindings: args.bindings.clone(),
            })?,
            inputs,
            BTreeMap::new(),
        )?;
        serde_json::from_value(applied.result).context("read the recompose report")?
    };
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "app_root": app_root,
            "recompose": report,
        }))?;
        return Ok(());
    }
    console.status(&banner(&app_root, &report))?;
    Ok(())
}

fn banner(app_root: &std::path::Path, report: &RecomposeReport) -> crate::tty::StatusBanner {
    let title = match (report.dry_run, report.recomposed.is_empty()) {
        (_, true) => "NOTHING TO RECOMPOSE".to_owned(),
        (true, false) => format!("WOULD RECOMPOSE {} BINDING(S)", report.recomposed.len()),
        (false, false) => format!("RECOMPOSED {} BINDING(S)", report.recomposed.len()),
    };
    let mut banner = crate::tty::StatusBanner::new(crate::tty::Tone::Success, title);
    banner.rows.push(crate::tty::Row::key_value(
        "app root",
        app_root.display().to_string(),
    ));
    for entry in &report.recomposed {
        banner.rows.push(crate::tty::Row::key_value(
            entry.binding.to_string(),
            format!(
                "{} -> {} ({})",
                entry.old_digest,
                entry.new_digest,
                entry.fields_changed.join(", ")
            ),
        ));
        if let Some(generation) = &entry.credential_generation {
            banner.rows.push(crate::tty::Row::text(format!(
                "  credential generation {generation} carried; no secret to provision"
            )));
        }
    }
    for binding in &report.unchanged {
        banner
            .rows
            .push(crate::tty::Row::key_value(binding.to_string(), "unchanged"));
    }
    for binding in &report.credentials_absent {
        banner.rows.push(crate::tty::Row::text(format!(
            "{binding}: the vault holds no credential for its generation; provision it with \
             `ryeos node external-execution provision-credential`"
        )));
    }
    if !report.dry_run && !report.recomposed.is_empty() {
        banner.rows.push(crate::tty::Row::text(
            "restart the node to load the recomposed bindings",
        ));
    }
    banner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_are_named_by_section_and_id() {
        let args = RecomposeArgs::try_parse_from([
            "--binding",
            "runtime_snapshot_production/render-snapshot",
            "--binding",
            "external_execution/codex-qualification-session",
            "--dry-run",
        ])
        .unwrap();
        assert_eq!(args.bindings.len(), 2);
        assert!(args.dry_run);
        assert!(RecomposeArgs::try_parse_from(["--binding", "bundles/core"]).is_err());
        assert!(RecomposeArgs::try_parse_from(["--binding", "render-snapshot"]).is_err());
    }
}
