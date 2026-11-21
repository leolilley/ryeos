//! `ryeos node external-execution retire <section>/<id>`: remove one installed
//! binding that is signed under a retired schema, on a stopped node.
//!
//! The verb refuses a binding whose signed schema is still admitted. It is an
//! offline operation applied under the node's state lock, like the composes.
//! `--dry-run` is not an operation: it changes nothing, so it holds the
//! existing state lock read-only to prove the daemon is stopped and reports
//! what a retire would remove.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use ryeos_app::node_config::recompose::BindingRef;
use ryeos_app::node_config::retire::{RetireReport, RetiredHandles};
use ryeos_node::offline_operation::handlers::RetireArguments;

use crate::lifecycle_external_snapshot_compose::parse_args;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node external-execution retire",
    about = "Remove one installed binding signed under a retired schema from a stopped node: \
             `<section>/<id>`, where the section is external_execution, \
             runtime_snapshot_production or runtime_snapshot_qualification. The file's signature \
             is verified and a binding that is still current is refused. The report names the \
             secret handle the binding carried, so compose it again and provision its \
             credential.",
    no_binary_name = true
)]
struct RetireArgs {
    /// Binding to retire, as `<section>/<id>`. `ryeos node doctor` lists the
    /// stale ones with this exact spelling in its fix.
    #[arg(value_parser = BindingRef::parse)]
    binding: BindingRef,

    /// Print what would be removed; remove nothing.
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
pub fn run_retire(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_args::<RetireArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let report: RetireReport = if args.dry_run {
        let state_lock = ryeos_node_runtime::state_lock::StateLock::acquire_existing_read_only(
            &ryeos_node_runtime::state_lock::default_lock_path(&app_root),
        )
        .context("a retire requires the daemon to be stopped")?;
        ryeos_app::node_config::retire::retire_binding(
            ryeos_app::node_config::retire::RetireRequest {
                app_root: &app_root,
                state_lock: &state_lock,
                binding: args.binding.clone(),
                dry_run: true,
            },
        )?
    } else {
        let host = crate::offline_apply::CliHost::new();
        let applied = host.apply(
            &app_root,
            "node.external-execution.retire",
            serde_json::to_value(RetireArguments {
                binding: args.binding.clone(),
            })?,
            BTreeMap::new(),
            BTreeMap::new(),
        )?;
        serde_json::from_value(applied.result).context("read the retire report")?
    };
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "app_root": app_root,
            "retire": report,
        }))?;
        return Ok(());
    }
    console.status(&banner(&app_root, &report))?;
    Ok(())
}

fn banner(app_root: &std::path::Path, report: &RetireReport) -> crate::tty::StatusBanner {
    let title = if report.dry_run {
        format!("WOULD RETIRE {}", report.binding)
    } else {
        format!("RETIRED {}", report.binding)
    };
    let mut banner = crate::tty::StatusBanner::new(crate::tty::Tone::Success, title);
    for (key, value) in [
        ("app root", app_root.display().to_string()),
        ("file", report.file.clone()),
        ("schema", report.schema.to_string()),
        ("signer", report.signer.clone()),
    ] {
        banner.rows.push(crate::tty::Row::key_value(key, value));
    }
    match &report.handles {
        RetiredHandles::Credential {
            backend,
            account,
            credential_generation,
        } => {
            banner.rows.push(crate::tty::Row::key_value(
                "secret handle",
                format!("backend {backend}, account {account}, generation {credential_generation}"),
            ));
            banner.rows.push(crate::tty::Row::text(
                "compose the binding again; it mints its own credential generation, which \
                 `ryeos node external-execution provision-credential` then provisions. The old \
                 vault entry stays in place.",
            ));
        }
        RetiredHandles::NoneCarried { reason } => {
            banner.rows.push(crate::tty::Row::key_value(
                "secret handle",
                format!("none: {reason}"),
            ));
        }
        RetiredHandles::NotExtractable { reason } => {
            banner.rows.push(crate::tty::Row::key_value(
                "secret handle",
                format!("not extractable: {reason}"),
            ));
        }
    }
    if !report.dry_run {
        banner.rows.push(crate::tty::Row::text(
            "compose the binding again, then restart the node",
        ));
    }
    banner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binding_is_one_section_and_id() {
        let args =
            RetireArgs::try_parse_from(["external_execution/w9", "--dry-run", "--json"]).unwrap();
        assert_eq!(args.binding.section, "external_execution");
        assert_eq!(args.binding.id, "w9");
        assert!(args.dry_run && args.json);
        assert!(RetireArgs::try_parse_from(["w9"]).is_err());
        assert!(RetireArgs::try_parse_from(["bundles/core"]).is_err());
        assert!(RetireArgs::try_parse_from(["external_execution/../x"]).is_err());
        assert!(RetireArgs::try_parse_from([] as [&str; 0]).is_err());
    }
}
