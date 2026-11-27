//! Every request field of a service that names a host path, a directory, a
//! URL, a principal or a key must be classified in `CLASSIFIED`, and the
//! handler file the entry names must still contain the call that enforces its
//! class. A field that carries one of those meanings without an entry fails
//! here, so a new service or a new field cannot reach a handler unjudged.
//!
//! The classes are:
//! * `Admitted`: the value is admitted as a live project path before use.
//! * `OperatorOnly`: the service is refused to any key that is not the node's
//!   operator before the request is read.
//! * `OwnerFiltered`: the value is only honoured against what the caller's
//!   owner holds, or the caller acts under its own grant.
//! * `LocalOnly`: the service is `availability: local`, so it has no daemon
//!   handler at all (see `local_only_services.rs`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[path = "../../../../tests/support/rust_source_text.rs"]
mod rust_source_text;

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Admitted,
    OperatorOnly,
    OwnerFiltered,
    LocalOnly,
}

impl Class {
    /// The call texts a handler file may show as the enforcement of this
    /// class. `LocalOnly` has no handler, so its only text is the empty one.
    fn enforcing_calls(self) -> &'static [&'static str] {
        match self {
            Self::Admitted => &["admit_handler_live_path(", "admit_live_project_path("],
            Self::OperatorOnly => &[
                "require_admitted_operator(",
                "require_local_configured_operator(",
                "resolve_admitted(",
                "operator_caller(",
            ],
            Self::OwnerFiltered => &[
                "speaks_for(",
                "owner_for(",
                "owner_principal(",
                "is_owner(",
                "require_owner(",
                "key_owns_project(",
                "caller_authorized_by(",
                "was_granted_by(",
                "named_remote_client(",
                "require_authenticated_source_node(",
            ],
            Self::LocalOnly => &[""],
        }
    }
}

/// The words that mark a field. A field is marked when its whole name is one
/// of them or its name ends with `_` and one of them (`project_path`,
/// `output_dir`, `outbound_principal`, `expected_signing_key`), never on a
/// substring (`keyword`, `directory`). The field named exactly `project` is
/// marked too: the remote handlers use it for a path.
const MARKER_WORDS: &[&str] = &[
    "path",
    "dir",
    "root",
    "checkout",
    "url",
    "uri",
    "endpoint",
    "principal",
    "fingerprint",
    "owner",
    "signer",
    "key",
];

fn is_marked(field: &str) -> bool {
    field == "project"
        || MARKER_WORDS
            .iter()
            .any(|word| field == *word || field.ends_with(&format!("_{word}")))
}

/// (service id, field, class, handler file relative to the workspace root,
/// the call text the handler's production source must contain).
const CLASSIFIED: &[(&str, &str, Class, &str, &str)] = &[
    (
        "bundle-release/authority-calibrate",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_local_configured_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "core_seed_qualification_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "native_qualification_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "portable_qualification_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "publisher_executable_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "publisher_fingerprint",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "substrate_build_signer_fingerprint",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "substrate_qualification_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/core-seed-inspect",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/input-inspect",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/submit",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "admit_handler_live_path(",
    ),
    (
        "bundle-release/substrate-build",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/substrate-qualify",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    ("bundle/profile-sign", "key", Class::LocalOnly, "", ""),
    ("bundle/publish", "owner", Class::LocalOnly, "", ""),
    ("bundle/publish", "registry_root", Class::LocalOnly, "", ""),
    (
        "bundle/pull",
        "url",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_pull.rs",
        "operator_caller(",
    ),
    ("bundle/sign", "registry_root", Class::LocalOnly, "", ""),
    (
        "bundle/verify",
        "registry_root",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_verify.rs",
        "require_local_configured_operator(",
    ),
    (
        "commands/dispatch",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/routes/response_modes/execute_mode.rs",
        "admit_live_project_path(",
    ),
    (
        "endorsements/list",
        "project",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/endorsements.rs",
        "key_owns_project(",
    ),
    (
        "external-content/activate",
        "offline_archive_root",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_activate.rs",
        "require_admitted_operator(",
    ),
    (
        "external-content/bind",
        "product_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/bind",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/bindings",
        "product_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/bindings",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/import",
        "binding_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-app/src/operator_external_content.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/import",
        "path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-app/src/operator_external_content.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/import",
        "root",
        Class::OperatorOnly,
        "crates/daemon/ryeos-app/src/operator_external_content.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/rebind",
        "product_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/rebind",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    ("fetch", "project_path", Class::LocalOnly, "", ""),
    (
        "identity/authorize-client",
        "public_key",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/authorize_client.rs",
        "require_local_configured_operator(",
    ),
    (
        "identity/authorize-key",
        "public_key",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/authorize_key.rs",
        "caller_authorized_by(",
    ),
    (
        "identity/renew-key",
        "fingerprint",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/renew_key.rs",
        "was_granted_by(",
    ),
    (
        "identity/revoke-key",
        "fingerprint",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/revoke_key.rs",
        "was_granted_by(",
    ),
    (
        "items/effective",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/items_effective.rs",
        "admit_handler_live_path(",
    ),
    (
        "items/revised",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/items_revised.rs",
        "admit_handler_live_path(",
    ),
    ("node/doctor", "registry_root", Class::LocalOnly, "", ""),
    ("node/logs", "app_root", Class::LocalOnly, "", ""),
    (
        "objects/closure/describe",
        "root",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "owner_principal(",
    ),
    (
        "objects/closure/get",
        "root",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "owner_principal(",
    ),
    (
        "objects/get",
        "root",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "owner_principal(",
    ),
    (
        "objects/has",
        "root",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "owner_principal(",
    ),
    (
        "objects/put",
        "project",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/objects_put.rs",
        "owner_principal(",
    ),
    (
        "project/apply-published",
        "checkout",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/project_apply_published.rs",
        "owner_principal(",
    ),
    (
        "project/apply-snapshot",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/project_apply_snapshot.rs",
        "admit_handler_live_path(",
    ),
    (
        "project/snapshot-status",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-app/src/runtime_project_snapshot_service.rs",
        "require_local_configured_operator(",
    ),
    (
        "project/status",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/project_status.rs",
        "admit_handler_live_path(",
    ),
    (
        "remote/authorize",
        "public_key",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/remote_authorize.rs",
        "named_remote_client(",
    ),
    (
        "remote/bind-project",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_bind_project.rs",
        "require_local_configured_operator(",
    ),
    (
        "remote/configure",
        "expect_fingerprint",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_configure.rs",
        "require_local_configured_operator(",
    ),
    (
        "remote/configure",
        "url",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_configure.rs",
        "require_local_configured_operator(",
    ),
    (
        "remote/descriptor",
        "url",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_descriptor.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/doctor",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_doctor.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/execute",
        "project",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/remote_execute.rs",
        "admit_handler_live_path(",
    ),
    (
        "remote/project-status",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_project_status.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/pull",
        "output_dir",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_pull.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/pull",
        "root",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_pull.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/pull-worker-result",
        "project",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/remote_pull_worker_result.rs",
        "speaks_for(",
    ),
    (
        "remote/push",
        "outbound_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_push.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/push",
        "project",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/remote_push.rs",
        "admit_handler_live_path(",
    ),
    (
        "remote/reconcile-project-head",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_reconcile_project_head.rs",
        "require_local_configured_operator(",
    ),
    (
        "remote/revoke",
        "fingerprint",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/remote_revoke.rs",
        "named_remote_client(",
    ),
    (
        "remote/run",
        "outbound_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_run.rs",
        "resolve_admitted(",
    ),
    (
        "remote/run",
        "project",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/remote_run.rs",
        "admit_handler_live_path(",
    ),
    (
        "remote/sync-project-ai",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_sync_project_ai.rs",
        "require_admitted_operator(",
    ),
    (
        "revisions/get",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/revisions.rs",
        "admit_handler_live_path(",
    ),
    (
        "revisions/list",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/revisions.rs",
        "admit_handler_live_path(",
    ),
    (
        "scheduler/register",
        "project_root",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/scheduler_register.rs",
        "admit_handler_live_path(",
    ),
    (
        "system/push-head",
        "project",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/push_head.rs",
        "owner_principal(",
    ),
    (
        "threads/set_facet",
        "key",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/threads_set_facet.rs",
        "require_owner(",
    ),
    (
        "tool/env-check",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/tool_env_check.rs",
        "admit_handler_live_path(",
    ),
    ("verify", "project_path", Class::LocalOnly, "", ""),
    (
        "worker-executions/command",
        "idempotency_key",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/dedicated_sessions.rs",
        "require_owner(",
    ),
    (
        "worker-placements/preflight",
        "owner_principal",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/worker_placements.rs",
        "require_authenticated_source_node(",
    ),
    (
        "worker-placements/preflight",
        "target_project_path",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/worker_placements.rs",
        "require_authenticated_source_node(",
    ),
    (
        "worker-placements/prepare",
        "target_project_path",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/worker_placements.rs",
        "require_authenticated_source_node(",
    ),
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "yaml") {
            out.push(path);
        }
    }
}

struct Service {
    availability: String,
    fields: Vec<String>,
}

/// Every service item in every bundle, by its name under `.ai/services`.
fn services() -> BTreeMap<String, Service> {
    let mut found = BTreeMap::new();
    for bundle in std::fs::read_dir(repo().join("bundles")).unwrap().flatten() {
        let root = bundle.path().join(".ai/services");
        let mut files = Vec::new();
        collect(&root, &mut files);
        for file in files {
            let name = file
                .strip_prefix(&root)
                .unwrap()
                .with_extension("")
                .to_string_lossy()
                .into_owned();
            let value: Value = serde_yaml::from_str(&std::fs::read_to_string(&file).unwrap())
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            let availability = value
                .get("availability")
                .and_then(Value::as_str)
                .unwrap_or("daemon_only")
                .to_owned();
            let fields = value
                .get("schema")
                .and_then(Value::as_object)
                .map(|schema| schema.keys().cloned().collect())
                .unwrap_or_default();
            found.insert(
                name,
                Service {
                    availability,
                    fields,
                },
            );
        }
    }
    found
}

#[test]
fn every_path_principal_or_key_field_is_classified_and_enforced() {
    let services = services();
    assert!(
        services.len() > 250,
        "the sweep found {} services",
        services.len()
    );
    let mut problems = Vec::new();

    let mut seen = BTreeSet::new();
    for (service, field, ..) in CLASSIFIED {
        if !seen.insert((*service, *field)) {
            problems.push(format!("{service}.{field}: classified twice"));
        }
    }

    // (a) every marked field has an entry.
    for (name, service) in &services {
        for field in service.fields.iter().filter(|field| is_marked(field)) {
            if !CLASSIFIED
                .iter()
                .any(|(id, f, ..)| *id == name.as_str() && *f == field.as_str())
            {
                problems.push(format!(
                    "{name}.{field}: a marked field with no CLASSIFIED entry"
                ));
            }
        }
    }

    let mut sources: BTreeMap<&str, String> = BTreeMap::new();
    for (id, field, class, file, call) in CLASSIFIED {
        // (b) the entry still names a real, marked field.
        let Some(service) = services.get(*id) else {
            problems.push(format!("{id}.{field}: no such service"));
            continue;
        };
        if !service.fields.iter().any(|f| f.as_str() == *field) {
            problems.push(format!("{id}.{field}: the service has no such field"));
            continue;
        }
        if !is_marked(field) {
            problems.push(format!("{id}.{field}: the field is not a marked name"));
        }
        // The class must agree with the YAML's availability and with an
        // enforcing call of its own kind.
        let local = service.availability == "local";
        if local != (*class == Class::LocalOnly) {
            problems.push(format!(
                "{id}.{field}: class {class:?} against availability {}",
                service.availability
            ));
        }
        if !class.enforcing_calls().contains(call) {
            problems.push(format!(
                "{id}.{field}: `{call}` is not an enforcing call of class {class:?}"
            ));
        }
        if *class == Class::LocalOnly {
            if !file.is_empty() || !call.is_empty() {
                problems.push(format!("{id}.{field}: a local-only entry names no handler"));
            }
            continue;
        }
        // (c) the handler's production text carries the enforcing call.
        let text = sources.entry(*file).or_insert_with(|| {
            let source = std::fs::read_to_string(repo().join(file))
                .unwrap_or_else(|error| panic!("{file}: {error}"));
            rust_source_text::production_text(file, &source)
        });
        if !text.contains(call) {
            problems.push(format!(
                "{id}.{field}: {file} has no `{call}` outside its tests"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "request-field classification is out of step with the services:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_marker_list_catches_a_new_field_of_each_kind() {
    for marked in [
        "project_path",
        "path",
        "output_dir",
        "dir",
        "app_root",
        "root",
        "checkout",
        "url",
        "remote_url",
        "uri",
        "callback_uri",
        "endpoint",
        "peer_endpoint",
        "outbound_principal",
        "principal",
        "publisher_fingerprint",
        "fingerprint",
        "binding_owner",
        "owner",
        "build_signer",
        "signer",
        "expected_signing_key",
        "key",
        "project",
    ] {
        assert!(is_marked(marked), "{marked} must be marked");
    }
    for unmarked in [
        "keyword",
        "directory",
        "pathology",
        "rootless",
        "chain_root_id",
        "principal_class",
        "forwarding_key_id",
        "item_ref",
        "projects",
        "project_name",
        "reason",
        "ownership",
    ] {
        assert!(!is_marked(unmarked), "{unmarked} must not be marked");
    }
}
