//! `ryeos node host-access grant|revoke|list`: the operator's grants of
//! host-direct execution.
//!
//! A grant hands one exact item operator-equivalent authority: the process
//! runs on the host, unconfined, as the daemon user. The grant pins the
//! item's canonical ref, its signer and the exact digests of its source
//! closure and runtime, so any edit needs a new grant. These commands compute
//! those digests from a project, edit the `host_access` node-policy section
//! and install it through the operator-signed `node.policy-apply` operation,
//! like `ryeos node policy-apply`. The node must be stopped; a grant takes
//! effect at the next start.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use ryeos_node_runtime::node_policy::sections::host_access::{
    HostAccessContainment, HostAccessGrant, HostAccessOrigin, HostAccessRuntimePin,
    NodeHostAccessPolicy, SECTION_NAME,
};

use crate::lifecycle_commands::parse_or_render_help;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node host-access grant",
    about = "Grant one exact host-authority tool host-direct execution",
    long_about = "Resolve the item in a project, compute its signer, source-closure and runtime digests, and install a host_access grant pinned to exactly those bytes through the operator-signed node.policy-apply operation. A host-direct process runs on the host, unconfined, as the daemon user: it can read the operator key, the node key and the vault. Only grant items you would run as the operator. The node must be stopped; the grant takes effect at the next start. Re-run after any edit to the item, its source closure or its runtime.",
    no_binary_name = true
)]
struct HostAccessGrantArgs {
    /// Canonical item ref, for example tool:ryeos/development/guarded-cargo.
    item: String,

    /// Project the item resolves in.
    #[arg(long)]
    project: PathBuf,

    /// Grant id (policy-name grammar). Defaults to the item's bare id with `/` as `-`.
    #[arg(long)]
    id: Option<String>,

    /// Launch origin the grant admits: direct, graph_child or schedule.
    /// Repeat for several. Defaults to direct; schedule is never implied.
    #[arg(long = "origin", value_parser = parse_origin)]
    origins: Vec<HostAccessOrigin>,

    /// Name delivered from the daemon's own boot environment (not your
    /// shell). Repeat for several. Nothing else is inherited.
    #[arg(long = "env")]
    host_env: Vec<String>,

    /// scope_required (a cgroup scope, so cancel reaches every descendant)
    /// or group_allowed (process group only).
    #[arg(long, value_parser = parse_containment, default_value = "scope_required")]
    containment: HostAccessContainment,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node host-access revoke",
    about = "Remove one host_access grant",
    long_about = "Remove the named grant from the host_access node-policy section through the operator-signed node.policy-apply operation. The node must be stopped.",
    no_binary_name = true
)]
struct HostAccessRevokeArgs {
    /// Grant id.
    id: String,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node host-access list",
    about = "List the host_access grants",
    long_about = "Print every host_access grant and the node lane's host_direct value. With --project, resolve each granted item in that project and report whether the grant still matches its bytes (current) or needs a re-grant (stale).",
    no_binary_name = true
)]
struct HostAccessListArgs {
    /// Resolve each granted item here and compare its digests.
    #[arg(long)]
    project: Option<PathBuf>,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

fn parse_origin(value: &str) -> std::result::Result<HostAccessOrigin, String> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|_| format!("unknown origin `{value}`; expected direct, graph_child or schedule"))
}

fn parse_containment(value: &str) -> std::result::Result<HostAccessContainment, String> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|_| {
        format!("unknown containment `{value}`; expected scope_required or group_allowed")
    })
}

/// The exact bytes a grant pins, as resolved in one project.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct HostAccessSubjectDigests {
    pub item: String,
    pub signer: String,
    pub source_closure: Option<String>,
    pub runtime_ref: String,
    pub runtime_content_hash: String,
}

/// Resolve `item` in `project` under the node's registered generation and
/// compute the digests a grant pins. Refuses an item that does not declare
/// host authority.
pub(crate) fn resolve_subject_digests(
    app_root: &Path,
    project: &Path,
    item: &str,
) -> Result<HostAccessSubjectDigests> {
    let project = std::fs::canonicalize(project)
        .with_context(|| format!("resolve project {}", project.display()))?;
    let config =
        ryeos_node_runtime::config::Config::load(&ryeos_node_runtime::config::ConfigSources {
            app_root: Some(app_root.to_path_buf()),
            ..Default::default()
        })?;
    let isolation = ryeos_node_runtime::engine_init::load_locked_registered_definition_isolation(
        &config.app_root,
    )
    .context("load node isolation policy")?;
    let _generation_guard = isolation.begin_registered_generation_operation()?;
    let node_trust = isolation
        .registered_generation_node_trust()
        .context("retained isolation generation omitted node trust")?;
    let snapshot =
        crate::node_descriptors::load_verified_snapshot_with_trust(app_root, node_trust)?;
    let node_policy =
        crate::node_descriptors::load_verified_policy_snapshot_with_trust(app_root, node_trust)?;
    let bundle_roots: Vec<PathBuf> = snapshot
        .bundles
        .iter()
        .map(|record| record.path.clone())
        .collect();
    let engine = ryeos_node_runtime::engine_init::build_registered_engine_for_roots(
        &config,
        &bundle_roots,
        Some(project.as_path()),
        std::sync::Arc::clone(&isolation),
        node_policy.require::<ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy>()?,
    )?;
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item)
        .with_context(|| format!("parse item ref `{item}`"))?;
    let authority = ryeos_engine::contracts::SubjectResolutionAuthority::for_live_project_root(
        Some(project.as_path()),
    );
    let (effective, output) = engine
        .effective_item_with_resolution(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: canonical,
            expected_kind: None,
            project_root: Some(project.clone()),
            subject_resolution_authority: authority.clone(),
        })
        .with_context(|| format!("resolve `{item}` in {}", project.display()))?;
    let execution = engine
        .kinds
        .get(&effective.kind)
        .and_then(|schema| schema.execution.as_ref())
        .with_context(|| format!("kind `{}` is not executable", effective.kind))?;
    let posture = execution.project_execution_posture(&output.composed.composed)?;
    ensure!(
        posture.is_host_direct(),
        "`{item}` does not declare host_authority: required; only a host-authority tool can be granted host access"
    );
    let signer = output
        .root
        .signer_fingerprint
        .clone()
        .with_context(|| format!("`{item}` is unsigned; host access needs a pinned signer"))?;
    let source_closure = ryeos_app::host_access::source_closure_digest(
        output
            .composed
            .derived
            .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY),
    )?;
    let executor_id = effective
        .composed_value
        .get("executor_id")
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("`{item}` declares no executor_id"))?;
    let terminal = engine.resolve_terminal_executor(
        &output.root.source_path,
        executor_id,
        &effective.kind,
        Some(project.clone()),
        &authority,
    )?;
    let runtime = terminal
        .chain_authorities
        .first()
        .with_context(|| format!("`{item}` resolved no runtime executor"))?;
    Ok(HostAccessSubjectDigests {
        item: output.root.resolved_ref.clone(),
        signer,
        source_closure,
        runtime_ref: runtime.canonical_ref.clone(),
        runtime_content_hash: runtime.content_hash.clone(),
    })
}

fn current_policy(app_root: &Path) -> Result<NodeHostAccessPolicy> {
    let snapshot = crate::node_descriptors::load_verified_policy_snapshot(app_root)?;
    Ok(snapshot.require::<NodeHostAccessPolicy>()?.clone())
}

/// Install `policy` as the node's `host_access` section through the
/// operator-signed `node.policy-apply` operation.
fn install(app_root: &Path, policy: &NodeHostAccessPolicy) -> Result<String> {
    policy.validate()?;
    let body = serde_yaml::to_string(policy).context("serialize host_access policy")?;
    let policy_generation =
        ryeos_node::offline_operation::handlers::prospective_policy_apply_digest(
            app_root,
            SECTION_NAME,
            body.as_bytes(),
        )?;
    let applied = crate::offline_apply::CliHost::new().apply(
        app_root,
        "node.policy-apply",
        serde_json::to_value(
            ryeos_node::offline_operation::handlers::PolicyApplyArguments {
                section: SECTION_NAME.to_owned(),
            },
        )?,
        std::collections::BTreeMap::from([("policy".to_owned(), body.into_bytes())]),
        std::collections::BTreeMap::from([(
            "policy_generation".to_owned(),
            policy_generation.clone(),
        )]),
    )?;
    applied.receipt.digest()
}

fn default_grant_id(item: &str) -> Result<String> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item)?;
    let id = canonical
        .bare_id
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_lowercase()
        .replace(['.', '_'], "-");
    ryeos_node_runtime::node_policy::generation::validate_policy_name("host_access grant", &id)
        .with_context(|| format!("derive a grant id from `{item}`; pass --id"))?;
    Ok(id)
}

pub fn run_grant(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<HostAccessGrantArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let digests = resolve_subject_digests(&app_root, &args.project, &args.item)?;
    let id = match args.id {
        Some(id) => id,
        None => default_grant_id(&digests.item)?,
    };
    let mut origins = if args.origins.is_empty() {
        vec![HostAccessOrigin::Direct]
    } else {
        args.origins
    };
    origins.sort();
    origins.dedup();
    let mut host_env = args.host_env;
    host_env.sort();
    host_env.dedup();
    let grant = HostAccessGrant {
        id: id.clone(),
        item: digests.item.clone(),
        signer: digests.signer.clone(),
        source_closure: digests.source_closure.clone(),
        runtime: HostAccessRuntimePin {
            runtime_ref: digests.runtime_ref.clone(),
            content_hash: digests.runtime_content_hash.clone(),
        },
        origins,
        host_env,
        containment: args.containment,
    };
    let mut policy = current_policy(&app_root)?;
    // Re-granting the same item or id replaces that one grant.
    policy
        .grants
        .retain(|existing| existing.id != grant.id && existing.item != grant.item);
    policy.grants.push(grant.clone());
    policy.grants.sort_by(|left, right| left.id.cmp(&right.id));
    let receipt = install(&app_root, &policy)?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "status": "granted",
            "grant": grant,
            "receipt": receipt,
        }))?;
    } else {
        console.text(&format!(
            "Granted host-direct execution to {} as `{}`.\n  signer {}\n  source closure {}\n  runtime {}@{}\n  origins {}\n  containment {}\nIt runs on the host with the operator's authority. Takes effect at the next start.\n",
            grant.item,
            grant.id,
            grant.signer,
            grant.source_closure.as_deref().unwrap_or("none"),
            grant.runtime.runtime_ref,
            grant.runtime.content_hash,
            grant
                .origins
                .iter()
                .map(|origin| origin.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            grant.containment.as_str(),
        ))?;
    }
    Ok(())
}

pub fn run_revoke(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<HostAccessRevokeArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let mut policy = current_policy(&app_root)?;
    let before = policy.grants.len();
    policy.grants.retain(|grant| grant.id != args.id);
    if policy.grants.len() == before {
        bail!("no host_access grant has id `{}`", args.id);
    }
    let receipt = install(&app_root, &policy)?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "status": "revoked",
            "id": args.id,
            "receipt": receipt,
        }))?;
    } else {
        console.text(&format!(
            "Revoked host_access grant `{}`. Takes effect at the next start.\n",
            args.id
        ))?;
    }
    Ok(())
}

pub fn run_list(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<HostAccessListArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let snapshot = crate::node_descriptors::load_verified_policy_snapshot(&app_root)?;
    let policy = snapshot.require::<NodeHostAccessPolicy>()?;
    let lane = snapshot.require::<ryeos_engine::isolation::IsolationPolicy>()?;
    let mut rows = Vec::new();
    for grant in &policy.grants {
        let status = match &args.project {
            None => serde_json::Value::Null,
            Some(project) => match resolve_subject_digests(&app_root, project, &grant.item) {
                Ok(seen) => grant_freshness(grant, &seen),
                Err(error) => serde_json::json!({
                    "status": "unresolved",
                    "error": format!("{error:#}"),
                }),
            },
        };
        rows.push(serde_json::json!({ "grant": grant, "project": status }));
    }
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "host_class": lane.host_class,
            "host_direct": lane.host_direct.as_str(),
            "grants": rows,
        }))?;
        return Ok(());
    }
    console.text(&format!(
        "host class {}: host_direct {}\n",
        lane.host_class,
        lane.host_direct.as_str()
    ))?;
    if policy.grants.is_empty() {
        console.text("No host_access grants.\n")?;
    }
    for row in rows {
        let grant = &row["grant"];
        console.text(&format!(
            "{}  {}  signer {}  origins {}  containment {}{}\n",
            grant["id"].as_str().unwrap_or("?"),
            grant["item"].as_str().unwrap_or("?"),
            grant["signer"].as_str().unwrap_or("?"),
            grant["origins"],
            grant["containment"].as_str().unwrap_or("?"),
            row["project"]["status"]
                .as_str()
                .map(|status| format!("  [{status}]"))
                .unwrap_or_default(),
        ))?;
    }
    Ok(())
}

/// Compare a grant with the digests its item has now.
pub(crate) fn grant_freshness(
    grant: &HostAccessGrant,
    seen: &HostAccessSubjectDigests,
) -> serde_json::Value {
    let current = grant.item == seen.item
        && grant.signer == seen.signer
        && grant.source_closure == seen.source_closure
        && grant.runtime.runtime_ref == seen.runtime_ref
        && grant.runtime.content_hash == seen.runtime_content_hash;
    serde_json::json!({
        "status": if current { "current" } else { "stale" },
        "seen": seen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> HostAccessGrant {
        HostAccessGrant {
            id: "guarded-cargo".into(),
            item: "tool:ryeos/development/guarded-cargo".into(),
            signer: "a".repeat(64),
            source_closure: Some("b".repeat(64)),
            runtime: HostAccessRuntimePin {
                runtime_ref: "tool:ryeos/core/runtimes/python/script".into(),
                content_hash: "c".repeat(64),
            },
            origins: vec![HostAccessOrigin::Direct],
            host_env: vec!["PATH".into()],
            containment: HostAccessContainment::ScopeRequired,
        }
    }

    fn digests(grant: &HostAccessGrant) -> HostAccessSubjectDigests {
        HostAccessSubjectDigests {
            item: grant.item.clone(),
            signer: grant.signer.clone(),
            source_closure: grant.source_closure.clone(),
            runtime_ref: grant.runtime.runtime_ref.clone(),
            runtime_content_hash: grant.runtime.content_hash.clone(),
        }
    }

    #[test]
    fn host_access_list_reports_stale_grant_for_project() {
        let granted = grant();
        assert_eq!(
            grant_freshness(&granted, &digests(&granted))["status"],
            "current"
        );
        let mut edited = digests(&granted);
        edited.source_closure = Some("d".repeat(64));
        let report = grant_freshness(&granted, &edited);
        assert_eq!(report["status"], "stale");
        assert_eq!(report["seen"]["source_closure"], "d".repeat(64));
    }

    #[test]
    fn grant_arguments_parse_closed_origins_and_containment() {
        let args = HostAccessGrantArgs::try_parse_from([
            "tool:ryeos/development/guarded-cargo",
            "--project",
            "/tmp/p",
            "--origin",
            "graph_child",
            "--origin",
            "direct",
            "--env",
            "PATH",
            "--containment",
            "group_allowed",
        ])
        .unwrap();
        assert_eq!(
            args.origins,
            vec![HostAccessOrigin::GraphChild, HostAccessOrigin::Direct]
        );
        assert_eq!(args.containment, HostAccessContainment::GroupAllowed);
        assert!(
            HostAccessGrantArgs::try_parse_from([
                "tool:x/y",
                "--project",
                "/tmp/p",
                "--origin",
                "webhook",
            ])
            .is_err()
        );
        assert_eq!(
            default_grant_id("tool:ryeos/development/dev-host/guarded-cargo").unwrap(),
            "ryeos-development-dev-host-guarded-cargo"
        );
    }
}
