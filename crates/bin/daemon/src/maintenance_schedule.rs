//! Node-policy maintenance schedule materialization.
//!
//! The system has no daemon-internal GC trigger. Rather than inventing one, GC
//! becomes a normal scheduled thread. The node's complete signed policy
//! generation owns cadence, item ref, parameters, and granted capabilities;
//! this module reconciles that typed policy into a signed node schedule spec —
//! the same shape `scheduler.register` / project sync produce, so the scheduler
//! reconcile that runs immediately afterwards projects and fires it like any
//! other schedule.
//!
//! Why policy + init-time reconcile instead of storing only the final spec:
//! the node spec must carry `execution.requester_fingerprint` (the acting
//! principal at dispatch) and a node signature. Those are per-install, so they
//! cannot be supplied by an init profile — the daemon fills its own identity.
//!
//! Ownership & operator control: generated specs carry a specific `managed_by`
//! marker. On every boot, policy refreshes every policy-owned field and removes
//! marked specs no longer authorized. An existing marked spec's
//! `enabled` value is the one operator override: `scheduler pause` / `resume`
//! survives restarts. Unmarked schedule specs are never adopted or removed.
//! Cadence, timezone, misfire/overlap behavior, lateness, initial enablement,
//! parameters, and capabilities are all required signed policy fields;
//! this adapter supplies no behavioral defaults.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use ryeos_app::identity::NodeIdentity;
use ryeos_app::node_document;
use ryeos_app::node_policy::sections::maintenance::{
    NodeMaintenancePolicy, NodeMaintenanceSchedulePolicy,
};
use ryeos_app::state::AppState;

/// Exact ownership discriminator written into generated schedule specs.
/// Both fields must match before reconciliation may mutate or remove a spec.
const MANAGED_BY_TYPE: &str = "node_maintenance_policy";
const MANAGED_BY_SOURCE: &str = ryeos_scheduler::types::NODE_MAINTENANCE_POLICY_SOURCE;

/// Materialize the mandatory node maintenance policy as signed schedule specs.
pub fn ensure_maintenance_schedule(state: &AppState) -> Result<()> {
    let node_dir = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node");
    let policy = state
        .node_policy
        .require::<NodeMaintenancePolicy>()
        .context("node has no compiled maintenance policy")?;
    apply_maintenance_schedules(
        &node_dir,
        &state.identity,
        &state.engine.trust_store,
        policy,
    )
}

fn apply_maintenance_schedules(
    node_dir: &Path,
    identity: &NodeIdentity,
    trust_store: &ryeos_engine::trust::TrustStore,
    policy: &NodeMaintenancePolicy,
) -> Result<()> {
    let declarations = &policy.schedules;
    let declared_ids = declarations
        .iter()
        .map(|schedule| schedule.schedule_id.clone())
        .collect::<HashSet<_>>();

    let node_directory = lillux::PinnedDirectory::open_or_create(node_dir)
        .context("establish no-follow node configuration root")?;
    let schedules_directory = node_directory
        .open_or_create_child(std::ffi::OsStr::new("schedules"), 0o777)
        .context("establish no-follow schedules directory")?;
    let _schedules_lock = schedules_directory.lock_exclusive()?;
    let schedules_dir = schedules_directory.path();
    let existing_files = scan_schedule_files(&schedules_directory)?;
    let managed_specs = load_managed_specs(&schedules_directory, &existing_files, trust_store)?;

    for decl in declarations {
        let target = schedules_dir.join(format!("{}.yaml", decl.schedule_id));
        let same_id_files = existing_files
            .get(&decl.schedule_id)
            .map_or(&[][..], Vec::as_slice);
        let (initial_enabled, initial_registered_at) = match same_id_files {
            [] => (decl.initial_enabled, lillux::time::timestamp_millis()),
            [existing] if existing == &target => {
                let managed = managed_specs.get(&decl.schedule_id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "maintenance schedule_id '{}' conflicts with an existing unowned spec at {}; refusing to adopt it",
                        decl.schedule_id,
                        existing.display()
                    )
                })?;
                (managed.enabled, managed.registered_at)
            }
            [existing] => {
                bail!(
                    "maintenance schedule_id '{}' conflicts with existing schedule file {}; managed schedules must use the canonical .yaml path",
                    decl.schedule_id,
                    existing.display()
                );
            }
            files => {
                let paths = files
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "maintenance schedule_id '{}' has multiple schedule files: {}",
                    decl.schedule_id,
                    paths
                );
            }
        };

        // Re-check ownership immediately before replacing an existing file,
        // and derive the pause override from this latest verified body rather
        // than from the initial directory scan.
        let target_name = target
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("schedule target has no filename"))?;
        let current_file = schedules_directory.open_regular(target_name, false)?;
        let current = match current_file.as_ref() {
            Some(file) => Some(load_managed_spec_file(
                &target,
                file.try_clone()?,
                trust_store,
            )?
            .ok_or_else(|| {
                    anyhow::anyhow!(
                        "maintenance schedule '{}' changed to an unowned spec during reconciliation; refusing to overwrite it",
                        decl.schedule_id
                    )
                })?),
            None => None,
        };
        let (enabled, registered_at) = current
            .as_ref()
            .map(|managed| (managed.enabled, managed.registered_at))
            .unwrap_or((initial_enabled, initial_registered_at));
        let desired_body = maintenance_spec_body(decl, enabled, registered_at, identity);
        if current
            .as_ref()
            .is_some_and(|managed| managed.body == desired_body)
        {
            tracing::debug!(
                schedule_id = %decl.schedule_id,
                "node-policy maintenance schedule already matches policy"
            );
            continue;
        }
        write_maintenance_spec(
            &schedules_directory,
            decl,
            &desired_body,
            identity,
            current_file.as_ref(),
        )
        .with_context(|| format!("reconcile maintenance schedule '{}'", decl.schedule_id))?;
        tracing::info!(
            schedule_id = %decl.schedule_id,
            item_ref = %decl.item_ref,
            expression = %decl.expression,
            enabled,
            "reconciled node-policy maintenance schedule"
        );
    }

    for (schedule_id, managed) in managed_specs {
        if declared_ids.contains(&schedule_id) {
            continue;
        }
        // Never remove based on a stale scan: re-open and verify the exact
        // inode immediately before descriptor-relative deletion.
        let name = managed
            .path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("managed schedule path has no filename"))?;
        let Some(file) = schedules_directory.open_regular(name, false)? else {
            continue;
        };
        if load_managed_spec_file(&managed.path, file.try_clone()?, trust_store)?.is_none() {
            bail!(
                "managed maintenance schedule '{}' changed ownership during reconciliation; refusing to remove {}",
                schedule_id,
                managed.path.display()
            );
        }
        schedules_directory
            .remove_if_same(name, &file)
            .with_context(|| {
                format!(
                    "remove undeclared maintenance schedule {}",
                    managed.path.display()
                )
            })?;
        tracing::info!(
            schedule_id = %schedule_id,
            path = %managed.path.display(),
            "removed undeclared bundle-managed maintenance schedule"
        );
    }

    Ok(())
}

#[derive(Debug)]
struct ManagedSpec {
    path: PathBuf,
    body: Value,
    enabled: bool,
    registered_at: i64,
}

fn scan_schedule_files(
    schedules_dir: &lillux::PinnedDirectory,
) -> Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut by_id: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for name in schedules_dir.entry_names()? {
        match schedules_dir.open_entry(&name, false)? {
            Some(lillux::PinnedDirectoryEntry::Directory(_)) => bail!(
                "schedule directory contains unsupported child directory {}",
                schedules_dir.path().join(&name).display()
            ),
            Some(lillux::PinnedDirectoryEntry::Regular(_)) => {}
            None => bail!("schedule directory entry disappeared"),
        }
        let path = schedules_dir.path().join(&name);
        if path.extension().and_then(|extension| extension.to_str()) != Some("yaml") {
            bail!(
                "schedule directory contains unsupported non-.yaml file {}",
                path.display()
            );
        }
        let schedule_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| anyhow::anyhow!("schedule filename must be UTF-8"))?;
        ryeos_scheduler::crontab::validate_schedule_id(schedule_id)?;
        by_id.entry(schedule_id.to_string()).or_default().push(path);
    }
    Ok(by_id)
}

fn load_managed_specs(
    schedules_dir: &lillux::PinnedDirectory,
    files: &BTreeMap<String, Vec<PathBuf>>,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<BTreeMap<String, ManagedSpec>> {
    let mut managed = BTreeMap::new();
    for paths in files.values() {
        for path in paths {
            let name = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("schedule path has no filename"))?;
            let file = schedules_dir
                .open_regular(name, false)?
                .ok_or_else(|| anyhow::anyhow!("schedule disappeared during reconciliation"))?;
            let Some(loaded) = load_managed_spec_file(path, file, trust_store)? else {
                continue;
            };
            let schedule_id = loaded_id(&loaded.body, path)?.to_string();
            if managed.insert(schedule_id, loaded).is_some() {
                bail!(
                    "multiple maintenance-managed schedule specs declare the same schedule_id in {}",
                    path.display()
                );
            }
        }
    }
    Ok(managed)
}

fn load_managed_spec_file(
    path: &Path,
    mut file: std::fs::File,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<Option<ManagedSpec>> {
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("read pinned schedule {}", path.display()))?;
    let verified =
        ryeos_scheduler::projection::verify_schedule_source_content(path, &content, trust_store)
            .with_context(|| format!("verify existing schedule {}", path.display()))?;
    let body = serde_json::to_value(verified.record)?;
    if !is_managed_maintenance_spec(&body) {
        return Ok(None);
    }
    let schedule_id = loaded_id(&body, path)?;
    let file_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow::anyhow!("schedule path {} has no UTF-8 stem", path.display()))?;
    if schedule_id != file_id {
        bail!(
            "maintenance-managed schedule_id '{}' does not match filename {}",
            schedule_id,
            path.display()
        );
    }
    let enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            anyhow::anyhow!("managed schedule '{}' has no boolean enabled", schedule_id)
        })?;
    let registered_at = body
        .get("registered_at")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "managed schedule '{}' has no integer registered_at",
                schedule_id
            )
        })?;
    Ok(Some(ManagedSpec {
        path: path.to_path_buf(),
        body,
        enabled,
        registered_at,
    }))
}

fn loaded_id<'a>(body: &'a Value, path: &Path) -> Result<&'a str> {
    body.get("schedule_id")
        .and_then(Value::as_str)
        .filter(|schedule_id| !schedule_id.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "maintenance-managed schedule {} has no schedule_id",
                path.display()
            )
        })
}

fn is_managed_maintenance_spec(body: &Value) -> bool {
    body.get("managed_by")
        .and_then(Value::as_object)
        .is_some_and(|managed_by| {
            managed_by.get("type").and_then(Value::as_str) == Some(MANAGED_BY_TYPE)
                && managed_by.get("source").and_then(Value::as_str) == Some(MANAGED_BY_SOURCE)
        })
}

fn maintenance_spec_body(
    decl: &NodeMaintenanceSchedulePolicy,
    enabled: bool,
    registered_at: i64,
    identity: &NodeIdentity,
) -> Value {
    // The node is both signer and acting principal for its own maintenance.
    serde_json::json!({
        "spec_version": 1,
        "schedule_id": decl.schedule_id,
        "item_ref": decl.item_ref,
        "ref_bindings": decl.ref_bindings,
        "schedule_type": decl.schedule_type,
        "expression": decl.expression,
        "timezone": decl.timezone,
        "enabled": enabled,
        "registered_at": registered_at,
        "misfire_policy": decl.misfire_policy,
        "overlap_policy": decl.overlap_policy,
        "lateness_grace_secs": decl.lateness_grace_secs,
        "params": decl.params,
        "project_root": Value::Null,
        "execution": {
            "requester_fingerprint": identity.fingerprint(),
            "capabilities": decl.capabilities,
        },
        "managed_by": {
            "type": MANAGED_BY_TYPE,
            "source": MANAGED_BY_SOURCE,
        },
    })
}

fn write_maintenance_spec(
    schedules_dir: &lillux::PinnedDirectory,
    decl: &NodeMaintenanceSchedulePolicy,
    body: &Value,
    identity: &NodeIdentity,
    expected: Option<&std::fs::File>,
) -> Result<()> {
    let bytes = node_document::render_signed_item("schedules", &decl.schedule_id, body, identity)?;
    let name = format!("{}.yaml", decl.schedule_id);
    schedules_dir.atomic_write_if_same(std::ffi::OsStr::new(&name), expected, &bytes, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::EncodePrivateKey;
    use rand::rngs::OsRng;

    fn identity() -> NodeIdentity {
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("identity/private_key.pem");
        std::fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        let key = lillux::crypto::SigningKey::generate(&mut OsRng);
        std::fs::write(
            &key_path,
            key.to_pkcs8_pem(Default::default()).unwrap().as_bytes(),
        )
        .unwrap();
        NodeIdentity::load(&key_path).unwrap()
    }

    fn trust_store(identity: &NodeIdentity) -> ryeos_engine::trust::TrustStore {
        ryeos_engine::trust::TrustStore::from_signers(vec![ryeos_engine::trust::TrustedSigner {
            fingerprint: identity.fingerprint().to_string(),
            verifying_key: *identity.verifying_key(),
            label: Some("test".into()),
        }])
    }

    fn policy(body: &str) -> NodeMaintenancePolicy {
        serde_yaml::from_str(body).unwrap()
    }

    fn read_schedule_body(path: &Path) -> Value {
        let content = std::fs::read_to_string(path).unwrap();
        let body = lillux::signature::strip_signature_lines(&content);
        serde_yaml::from_str(&body).unwrap()
    }

    fn write_operator_schedule(node_dir: &Path, identity: &NodeIdentity, schedule_id: &str) {
        node_document::write_signed_item(
            node_dir,
            "schedules",
            schedule_id,
            &serde_json::json!({
                "spec_version": 1,
                "schedule_id": schedule_id,
                "item_ref": "service:operator/task",
                "ref_bindings": {},
                "schedule_type": "cron",
                "expression": "0 0 1 * * *",
                "timezone": "UTC",
                "misfire_policy": "skip",
                "overlap_policy": "skip",
                "lateness_grace_secs": 60,
                "enabled": false,
                "params": {},
                "project_root": null,
                "registered_at": 1234,
                "execution": {
                    "requester_fingerprint": identity.fingerprint(),
                    "capabilities": ["ryeos.execute.service.operator/task"],
                },
                "managed_by": null,
            }),
            identity,
        )
        .unwrap();
    }

    const POLICY: &str = r#"schema: 1
schedules:
  - schedule_id: maintenance-gc
    item_ref: "service:maintenance/gc"
    ref_bindings: {}
    schedule_type: cron
    expression: "0 0 4 * * *"
    timezone: UTC
    overlap_policy: skip
    misfire_policy: skip
    lateness_grace_secs: 60
    initial_enabled: true
    params:
      deep: true
      schedule_fire_max_age_days: 30
      schedule_fire_max_count: 500
      sync_job_retention_days: 14
      seat_lease_grace_seconds: 600
    capabilities:
      - "ryeos.execute.service.maintenance/gc"
"#;

    #[test]
    fn materializes_policy_into_signed_node_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        let trust = trust_store(&id);
        let policy = policy(POLICY);

        apply_maintenance_schedules(&node_dir, &id, &trust, &policy).unwrap();

        let spec_path = node_dir.join("schedules").join("maintenance-gc.yaml");
        let content = std::fs::read_to_string(&spec_path).unwrap();
        // Signed by the node.
        assert!(
            content.starts_with("# ryeos:signed:"),
            "spec must be signed"
        );
        let body = lillux::signature::strip_signature_lines(&content);
        let parsed: serde_json::Value = serde_yaml::from_str(&body).unwrap();
        assert_eq!(parsed["item_ref"], "service:maintenance/gc");
        assert_eq!(parsed["overlap_policy"], "skip");
        assert_eq!(parsed["params"]["deep"], true);
        assert_eq!(parsed["params"]["schedule_fire_max_age_days"], 30);
        assert_eq!(parsed["params"]["schedule_fire_max_count"], 500);
        assert_eq!(parsed["params"]["sync_job_retention_days"], 14);
        assert_eq!(parsed["params"]["seat_lease_grace_seconds"], 600);
        assert_eq!(
            parsed["execution"]["requester_fingerprint"],
            id.fingerprint()
        );
        assert_eq!(
            parsed["execution"]["capabilities"][0],
            "ryeos.execute.service.maintenance/gc"
        );
        assert_eq!(parsed["managed_by"]["type"], MANAGED_BY_TYPE);
        assert_eq!(parsed["managed_by"]["source"], MANAGED_BY_SOURCE);
        assert!(parsed["registered_at"].is_i64());

        apply_maintenance_schedules(&node_dir, &id, &trust, &policy).unwrap();
        assert_eq!(
            std::fs::read_to_string(&spec_path).unwrap(),
            content,
            "an unchanged policy must not churn the signed spec hash"
        );
    }

    #[test]
    fn refreshes_managed_fields_but_preserves_pause_and_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        let trust = trust_store(&id);
        let initial_policy = policy(POLICY);
        apply_maintenance_schedules(&node_dir, &id, &trust, &initial_policy).unwrap();

        let existing = node_dir.join("schedules/maintenance-gc.yaml");
        let mut paused = read_schedule_body(&existing);
        paused["enabled"] = Value::Bool(false);
        paused["expression"] = Value::String("0 0 3 * * *".into());
        paused["params"]["deep"] = Value::Bool(false);
        let anchor = paused["registered_at"].as_i64().unwrap();
        node_document::write_signed_item(&node_dir, "schedules", "maintenance-gc", &paused, &id)
            .unwrap();

        let updated = policy(&POLICY.replace("0 0 4 * * *", "0 0 5 * * *"));
        apply_maintenance_schedules(&node_dir, &id, &trust, &updated).unwrap();

        let refreshed = read_schedule_body(&existing);
        assert_eq!(refreshed["enabled"], false, "operator pause must survive");
        assert_eq!(refreshed["registered_at"], anchor);
        assert_eq!(refreshed["expression"], "0 0 5 * * *");
        assert_eq!(refreshed["params"]["deep"], true);
        assert_eq!(refreshed["managed_by"]["type"], MANAGED_BY_TYPE);
    }

    #[test]
    fn refuses_to_claim_existing_operator_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        write_operator_schedule(&node_dir, &id, "maintenance-gc");
        let trust = trust_store(&id);
        let policy = policy(POLICY);
        let existing = node_dir.join("schedules/maintenance-gc.yaml");
        let before = std::fs::read(&existing).unwrap();

        let error = apply_maintenance_schedules(&node_dir, &id, &trust, &policy).unwrap_err();

        assert!(format!("{error:#}").contains("refusing to adopt"));
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            before,
            "unowned schedule must not be clobbered"
        );
    }

    #[test]
    fn empty_policy_removes_managed_specs_only() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        let trust = trust_store(&id);
        let policy = policy(POLICY);
        apply_maintenance_schedules(&node_dir, &id, &trust, &policy).unwrap();
        write_operator_schedule(&node_dir, &id, "operator-job");

        let empty = NodeMaintenancePolicy {
            schema: 1,
            schedules: Vec::new(),
        };
        apply_maintenance_schedules(&node_dir, &id, &trust, &empty).unwrap();

        assert!(
            !node_dir.join("schedules/maintenance-gc.yaml").exists(),
            "empty policy must remove no-longer-authorized managed specs"
        );
        assert!(
            node_dir.join("schedules/operator-job.yaml").exists(),
            "unowned schedules must not be removed"
        );
    }
}
