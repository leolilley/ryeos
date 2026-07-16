//! Bundle-authored maintenance schedule application.
//!
//! The system has no daemon-internal GC trigger. Rather than inventing one, GC
//! becomes a normal scheduled thread: the standard bundle authors a maintenance
//! schedule *declaration* (cadence, item ref, params, granted capabilities), and
//! this module reconciles it into a signed node schedule spec at daemon init —
//! the same shape `scheduler.register` / project sync produce, so the scheduler
//! reconcile that runs immediately afterwards projects and fires it like any
//! other schedule.
//!
//! Why a declaration + init-time reconcile instead of shipping the final spec:
//! the node spec must carry `execution.requester_fingerprint` (the acting
//! principal at dispatch) and a node signature. Those are per-install, so they
//! can't be baked into a portable bundle artifact — the daemon fills its own
//! identity here.
//!
//! Ownership & operator control: generated specs carry a specific `managed_by`
//! marker. On every boot, declarations refresh every declaration-owned field
//! and remove marked specs no longer declared. An existing marked spec's
//! `enabled` value is the one operator override: `scheduler pause` / `resume`
//! survives restarts. Unmarked schedule specs are never adopted or removed.
//! Cadence, timezone, misfire/overlap behavior, lateness, initial enablement,
//! parameters, and capabilities are all required signed declaration fields;
//! this adapter supplies no behavioral defaults.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use ryeos_app::identity::NodeIdentity;
use ryeos_app::node_config::writer;
use ryeos_app::state::AppState;

/// Relative location (under `.ai/node/`) of the bundle-authored declaration.
const DECLARATION_REL: &str = "maintenance/schedules.yaml";

/// Exact ownership discriminator written into generated schedule specs.
/// Both fields must match before reconciliation may mutate or remove a spec.
const MANAGED_BY_TYPE: &str = "node_maintenance_declaration";
const MANAGED_BY_SOURCE: &str = DECLARATION_REL;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaintenanceDeclarationFile {
    /// Format guard — only `1` is understood.
    spec_version: u32,
    schedules: Vec<MaintenanceScheduleDeclaration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaintenanceScheduleDeclaration {
    schedule_id: String,
    item_ref: String,
    ref_bindings: BTreeMap<String, String>,
    schedule_type: String,
    expression: String,
    timezone: String,
    misfire_policy: String,
    overlap_policy: String,
    lateness_grace_secs: i64,
    enabled: bool,
    params: Value,
    /// Capabilities the schedule dispatches with — least privilege. The node
    /// signs the resulting spec, so these are trusted at fire time.
    capabilities: Vec<String>,
}

/// Reconcile the bundle-authored maintenance schedule into a signed node spec.
///
/// Absence is an explicit empty declaration set: previously generated specs
/// are removed, while all operator/project-owned specs remain untouched.
pub fn ensure_maintenance_schedule(state: &AppState) -> Result<()> {
    let node_dir = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node");
    apply_maintenance_schedules(&node_dir, &state.identity, &state.engine.trust_store)
}

fn apply_maintenance_schedules(
    node_dir: &Path,
    identity: &NodeIdentity,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<()> {
    let file = load_declaration(node_dir, trust_store)?;
    let declarations = match file {
        Some(file) => {
            if file.spec_version != 1 {
                bail!(
                    "maintenance declaration has unsupported spec_version {} (only 1)",
                    file.spec_version
                );
            }
            file.schedules
        }
        None => Vec::new(),
    };

    let mut declared_ids = HashSet::with_capacity(declarations.len());
    for declaration in &declarations {
        validate_declaration(declaration).with_context(|| {
            format!(
                "validate maintenance schedule declaration '{}'",
                declaration.schedule_id
            )
        })?;
        if !declared_ids.insert(declaration.schedule_id.clone()) {
            bail!(
                "maintenance declaration contains duplicate schedule_id '{}'",
                declaration.schedule_id
            );
        }
    }

    let node_directory = lillux::PinnedDirectory::open_or_create(node_dir)
        .context("establish no-follow node configuration root")?;
    let schedules_directory = node_directory
        .open_or_create_child(std::ffi::OsStr::new("schedules"), 0o777)
        .context("establish no-follow schedules directory")?;
    let _schedules_lock = schedules_directory.lock_exclusive()?;
    let schedules_dir = schedules_directory.path();
    let existing_files = scan_schedule_files(&schedules_directory)?;
    let managed_specs = load_managed_specs(&schedules_directory, &existing_files, trust_store)?;

    for decl in &declarations {
        let target = schedules_dir.join(format!("{}.yaml", decl.schedule_id));
        let same_id_files = existing_files
            .get(&decl.schedule_id)
            .map_or(&[][..], Vec::as_slice);
        let (initial_enabled, initial_registered_at) = match same_id_files {
            [] => (decl.enabled, lillux::time::timestamp_millis()),
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
                "bundle-authored maintenance schedule already matches declaration"
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
            "reconciled bundle-authored maintenance schedule"
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

/// Load the maintenance declaration at `.ai/node/maintenance/schedules.yaml`.
///
/// `None` when absent — and absent means NOTHING is scheduled. The
/// declaration is the only source of maintenance cadence: no declaration,
/// no scheduled GC. Deliberate; a node whose bundle doesn't declare
/// maintenance must never grow a background job it can't see in data.
fn load_declaration(
    node_dir: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<Option<MaintenanceDeclarationFile>> {
    let declaration_path = node_dir.join(DECLARATION_REL);
    match fs::symlink_metadata(&declaration_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                path = %declaration_path.display(),
                "no maintenance declaration; declared schedule set is empty"
            );
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect maintenance declaration {}",
                    declaration_path.display()
                )
            });
        }
    }
    let body =
        ryeos_app::node_config::loader::load_verified_node_yaml(&declaration_path, trust_store)?;
    serde_json::from_value(body).map(Some).with_context(|| {
        format!(
            "parse maintenance declaration {}",
            declaration_path.display()
        )
    })
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

fn validate_declaration(decl: &MaintenanceScheduleDeclaration) -> Result<()> {
    ryeos_scheduler::crontab::validate_schedule_id(&decl.schedule_id)?;
    ryeos_engine::canonical_ref::CanonicalRef::parse(&decl.item_ref)
        .with_context(|| format!("invalid item_ref '{}'", decl.item_ref))?;
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(&decl.ref_bindings)
        .with_context(|| {
            format!(
                "maintenance schedule '{}' has invalid ref_bindings",
                decl.schedule_id
            )
        })?;
    ryeos_scheduler::crontab::validate_expression(&decl.schedule_type, &decl.expression)?;
    ryeos_scheduler::crontab::validate_timezone(&decl.timezone)?;
    ryeos_scheduler::overlap::parse_overlap_policy(&decl.overlap_policy).with_context(|| {
        format!(
            "maintenance schedule '{}' has invalid overlap_policy",
            decl.schedule_id
        )
    })?;
    ryeos_scheduler::misfire::parse_misfire_policy(&decl.misfire_policy).with_context(|| {
        format!(
            "maintenance schedule '{}' has invalid misfire_policy",
            decl.schedule_id
        )
    })?;

    if decl.capabilities.is_empty()
        || decl
            .capabilities
            .iter()
            .any(|capability| capability.trim().is_empty())
    {
        bail!(
            "maintenance schedule '{}' must declare non-empty capabilities",
            decl.schedule_id
        );
    }
    if !decl.params.is_object() {
        bail!(
            "maintenance schedule '{}' params must be a mapping",
            decl.schedule_id
        );
    }
    if decl.lateness_grace_secs <= 0 {
        bail!(
            "maintenance schedule '{}' lateness_grace_secs must be positive",
            decl.schedule_id
        );
    }
    Ok(())
}

fn maintenance_spec_body(
    decl: &MaintenanceScheduleDeclaration,
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
    decl: &MaintenanceScheduleDeclaration,
    body: &Value,
    identity: &NodeIdentity,
    expected: Option<&std::fs::File>,
) -> Result<()> {
    let bytes = writer::render_signed_node_item("schedules", &decl.schedule_id, body, identity)?;
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

    fn write_declaration(node_dir: &Path, body: &str, identity: &NodeIdentity) {
        let dir = node_dir.join("maintenance");
        std::fs::create_dir_all(&dir).unwrap();
        let signed = lillux::signature::sign_content(body, identity.signing_key(), "#", None);
        std::fs::write(dir.join("schedules.yaml"), signed).unwrap();
    }

    fn read_schedule_body(path: &Path) -> Value {
        let content = std::fs::read_to_string(path).unwrap();
        let body = lillux::signature::strip_signature_lines(&content);
        serde_yaml::from_str(&body).unwrap()
    }

    fn write_operator_schedule(node_dir: &Path, identity: &NodeIdentity, schedule_id: &str) {
        writer::write_signed_node_item(
            node_dir,
            "schedules",
            schedule_id,
            &serde_json::json!({
                "spec_version": 1,
                "schedule_id": schedule_id,
                "item_ref": "service:operator/task",
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

    const DECL: &str = r#"spec_version: 1
schedules:
  - schedule_id: maintenance-gc
    item_ref: "service:maintenance/gc"
    schedule_type: cron
    expression: "0 0 4 * * *"
    timezone: UTC
    overlap_policy: skip
    misfire_policy: skip
    lateness_grace_secs: 60
    enabled: true
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
    fn applies_declaration_into_signed_node_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        write_declaration(&node_dir, DECL, &id);
        let trust = trust_store(&id);

        apply_maintenance_schedules(&node_dir, &id, &trust).unwrap();

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

        apply_maintenance_schedules(&node_dir, &id, &trust).unwrap();
        assert_eq!(
            std::fs::read_to_string(&spec_path).unwrap(),
            content,
            "an unchanged declaration must not churn the signed spec hash"
        );
    }

    #[test]
    fn refreshes_managed_fields_but_preserves_pause_and_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        write_declaration(&node_dir, DECL, &id);
        let trust = trust_store(&id);
        apply_maintenance_schedules(&node_dir, &id, &trust).unwrap();

        let existing = node_dir.join("schedules/maintenance-gc.yaml");
        let mut paused = read_schedule_body(&existing);
        paused["enabled"] = Value::Bool(false);
        paused["expression"] = Value::String("stale-expression".into());
        paused["params"]["deep"] = Value::Bool(false);
        let anchor = paused["registered_at"].as_i64().unwrap();
        writer::write_signed_node_item(&node_dir, "schedules", "maintenance-gc", &paused, &id)
            .unwrap();

        write_declaration(&node_dir, &DECL.replace("0 0 4 * * *", "0 0 5 * * *"), &id);

        apply_maintenance_schedules(&node_dir, &id, &trust).unwrap();

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
        write_declaration(&node_dir, DECL, &id);
        write_operator_schedule(&node_dir, &id, "maintenance-gc");
        let trust = trust_store(&id);
        let existing = node_dir.join("schedules/maintenance-gc.yaml");
        let before = std::fs::read(&existing).unwrap();

        let error = apply_maintenance_schedules(&node_dir, &id, &trust).unwrap_err();

        assert!(format!("{error:#}").contains("refusing to adopt"));
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            before,
            "unowned schedule must not be clobbered"
        );
    }

    #[test]
    fn absent_declaration_removes_managed_specs_only() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        write_declaration(&node_dir, DECL, &id);
        let trust = trust_store(&id);
        apply_maintenance_schedules(&node_dir, &id, &trust).unwrap();
        write_operator_schedule(&node_dir, &id, "operator-job");

        std::fs::remove_file(node_dir.join(DECLARATION_REL)).unwrap();
        apply_maintenance_schedules(&node_dir, &id, &trust).unwrap();

        assert!(
            !node_dir.join("schedules/maintenance-gc.yaml").exists(),
            "absence is an empty declaration set"
        );
        assert!(
            node_dir.join("schedules/operator-job.yaml").exists(),
            "unowned schedules must not be removed"
        );
    }

    #[test]
    fn empty_declaration_removes_specs_no_longer_declared() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        write_declaration(&node_dir, DECL, &id);
        let trust = trust_store(&id);
        apply_maintenance_schedules(&node_dir, &id, &trust).unwrap();

        write_declaration(&node_dir, "spec_version: 1\nschedules: []\n", &id);
        apply_maintenance_schedules(&node_dir, &id, &trust).unwrap();

        assert!(!node_dir.join("schedules/maintenance-gc.yaml").exists());
    }

    #[test]
    fn rejects_duplicate_ids_before_writing_any_specs() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        let duplicated = DECL.replace(
            "schedules:\n",
            "schedules:\n  - schedule_id: maintenance-gc\n    item_ref: \"service:maintenance/gc\"\n    schedule_type: cron\n    expression: \"0 0 3 * * *\"\n    timezone: UTC\n    misfire_policy: skip\n    overlap_policy: skip\n    lateness_grace_secs: 60\n    enabled: true\n    params: {}\n    capabilities:\n      - \"ryeos.execute.service.maintenance/gc\"\n",
        );
        write_declaration(&node_dir, &duplicated, &id);
        let trust = trust_store(&id);

        let error = apply_maintenance_schedules(&node_dir, &id, &trust).unwrap_err();

        assert!(format!("{error:#}").contains("duplicate schedule_id"));
        assert!(!node_dir.join("schedules/maintenance-gc.yaml").exists());
    }

    #[test]
    fn rejects_tampered_signed_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        write_declaration(&node_dir, DECL, &id);
        let declaration_path = node_dir.join(DECLARATION_REL);
        let tampered = std::fs::read_to_string(&declaration_path)
            .unwrap()
            .replace("0 0 4 * * *", "0 0 5 * * *");
        std::fs::write(&declaration_path, tampered).unwrap();
        let trust = trust_store(&id);

        assert!(apply_maintenance_schedules(&node_dir, &id, &trust).is_err());
        assert!(!node_dir.join("schedules/maintenance-gc.yaml").exists());
    }

    #[test]
    fn rejects_incomplete_schedule_policy_instead_of_defaulting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        let incomplete = DECL.replace("    lateness_grace_secs: 60\n", "");
        write_declaration(&node_dir, &incomplete, &id);
        let trust = trust_store(&id);

        let error = apply_maintenance_schedules(&node_dir, &id, &trust).unwrap_err();

        assert!(format!("{error:#}").contains("lateness_grace_secs"));
        assert!(!node_dir.join("schedules/maintenance-gc.yaml").exists());
    }

    #[test]
    fn rejects_empty_capabilities() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();
        write_declaration(
            &node_dir,
            r#"spec_version: 1
schedules:
  - schedule_id: maintenance-gc
    item_ref: "service:maintenance/gc"
    schedule_type: cron
    expression: "0 0 4 * * *"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    params: {}
    capabilities: []
"#,
            &id,
        );
        let trust = trust_store(&id);
        assert!(apply_maintenance_schedules(&node_dir, &id, &trust).is_err());
    }
}
