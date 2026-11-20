//! Retire a stale binding on a stopped node.
//!
//! A binding signed under a schema number this node retired
//! ([`coherence::RETIRED_BINDING_SCHEMAS`]) is set aside by the loader and
//! authorizes nothing. This operation removes such a file so the binding can
//! be composed again. It works on the file, not on a loaded snapshot: it
//! verifies the signature the way the loader does (current node), refuses a
//! file whose signed schema is still admitted, and removes the file through
//! the pinned section directory.
//!
//! The retired document does not decode under the current structs, so the
//! secret handle it carried is read from the verified body as a generic
//! value, through the closed [`RETIRED_HANDLE_SHAPES`] table: one explicit
//! entry per retired schema number. A shape the table cannot read is
//! reported as such, never guessed.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Serialize;
use serde_json::Value;

use crate::node_config::coherence::{self, BINDING_NOT_RETIRED_SCHEMA};
use crate::node_config::recompose::BindingRef;
use crate::node_config::sections::external_execution::compose::{
    load_node_identity, validate_document_id, verify_node_signed,
};
use crate::node_config::sections::{
    external_execution, runtime_snapshot_production, runtime_snapshot_qualification,
};

/// The named binding file is not installed.
pub const RETIRE_BINDING_NOT_INSTALLED: &str = "retire_binding_not_installed";

/// A retire refused; nothing was removed.
#[derive(Debug, Clone)]
pub struct RetireRefusal {
    pub code: &'static str,
    pub binding: BindingRef,
    pub detail: String,
}

impl std::fmt::Display for RetireRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}: {}; nothing was removed",
            self.code, self.binding, self.detail
        )
    }
}

impl std::error::Error for RetireRefusal {}

/// Where a retired schema number records the secret it carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandleShape {
    /// Top-level `backend`, `account` and `credential_generation`: the vault
    /// key of the credential is owned by the signer, backend and account, at
    /// that generation.
    CredentialGeneration,
    /// The binding carries no secret handle; the reason says why.
    Carries(&'static str),
}

/// One entry per retired schema number, by section. A number added to
/// [`coherence::RETIRED_BINDING_SCHEMAS`] without an entry here fails a test.
const RETIRED_HANDLE_SHAPES: &[(&str, u64, HandleShape)] = &[
    (
        external_execution::SECTION_NAME,
        17,
        HandleShape::CredentialGeneration,
    ),
    (
        external_execution::SECTION_NAME,
        18,
        HandleShape::CredentialGeneration,
    ),
    (
        external_execution::SECTION_NAME,
        19,
        HandleShape::CredentialGeneration,
    ),
    (
        runtime_snapshot_production::SECTION_NAME,
        1,
        HandleShape::CredentialGeneration,
    ),
    (
        runtime_snapshot_qualification::SECTION_NAME,
        5,
        HandleShape::Carries(
            "a qualification binding names its production binding and carries no credential of \
             its own",
        ),
    ),
];

/// The secret handle a retired binding carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RetiredHandles {
    /// The vault credential the binding named. Composing the replacement
    /// mints its own generation; provision it with
    /// `ryeos node external-execution provision-credential`.
    Credential {
        backend: String,
        account: String,
        credential_generation: String,
    },
    /// The retired shape carries no secret handle.
    NoneCarried { reason: String },
    /// The document does not hold the handle fields its schema promises.
    NotExtractable { reason: String },
}

fn extract_handles(section: &str, schema: u64, body: &Value) -> RetiredHandles {
    let Some((_, _, shape)) = RETIRED_HANDLE_SHAPES
        .iter()
        .find(|(name, number, _)| *name == section && *number == schema)
    else {
        return RetiredHandles::NotExtractable {
            reason: format!("no handle layout is recorded for {section} schema {schema}"),
        };
    };
    match shape {
        HandleShape::Carries(reason) => RetiredHandles::NoneCarried {
            reason: (*reason).to_owned(),
        },
        HandleShape::CredentialGeneration => {
            let field = |name: &str| body.get(name).and_then(Value::as_str);
            match (
                field("backend"),
                field("account"),
                field("credential_generation"),
            ) {
                (Some(backend), Some(account), Some(generation))
                    if lillux::valid_hash(generation) =>
                {
                    RetiredHandles::Credential {
                        backend: backend.to_owned(),
                        account: account.to_owned(),
                        credential_generation: generation.to_owned(),
                    }
                }
                _ => RetiredHandles::NotExtractable {
                    reason: "`backend`, `account` and a hash `credential_generation` are not all \
                             present as text in the signed body"
                        .to_owned(),
                },
            }
        }
    }
}

/// One operator request to retire a stale binding on a stopped node.
pub struct RetireRequest<'a> {
    pub app_root: &'a Path,
    /// Proof that the node is stopped.
    pub state_lock: &'a ryeos_node_runtime::state_lock::StateLock,
    pub binding: BindingRef,
    /// Check and report only; remove nothing.
    pub dry_run: bool,
}

/// What a retire removed, or would remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RetireReport {
    pub dry_run: bool,
    pub binding: BindingRef,
    /// The retired schema number the file was signed under.
    pub schema: u64,
    pub file: String,
    pub signer: String,
    pub removed: bool,
    /// The secret handle the binding carried, so the operator knows what
    /// composing it again must provision.
    pub handles: RetiredHandles,
}

/// Retire one stale binding file. Reads no snapshot.
pub fn retire_binding(request: RetireRequest<'_>) -> Result<RetireReport> {
    request
        .state_lock
        .ensure_protects_app_root(request.app_root)?;
    let binding = request.binding;
    let refuse = |code: &'static str, detail: String| -> anyhow::Error {
        RetireRefusal {
            code,
            binding: binding.clone(),
            detail,
        }
        .into()
    };
    anyhow::ensure!(
        [
            external_execution::SECTION_NAME,
            runtime_snapshot_production::SECTION_NAME,
            runtime_snapshot_qualification::SECTION_NAME,
        ]
        .contains(&binding.section.as_str()),
        "`{}` is not a binding section",
        binding.section
    );
    validate_document_id(&binding.id, "binding")?;
    let identity = load_node_identity(request.app_root)?;
    let not_installed = || {
        refuse(
            RETIRE_BINDING_NOT_INSTALLED,
            "no binding file with this id is installed".to_owned(),
        )
    };
    let node_directory =
        lillux::PinnedDirectory::open(&request.app_root.join(ryeos_engine::AI_DIR).join("node"))
            .context("open the node document root")?
            .ok_or_else(not_installed)?;
    let section = node_directory
        .open_child_directory(OsStr::new(&binding.section))
        .with_context(|| format!("open the {} section", binding.section))?
        .ok_or_else(not_installed)?;
    let _directory_lock = if request.dry_run {
        None
    } else {
        Some(section.lock_exclusive()?)
    };
    let filename = format!("{}.yaml", binding.id);
    let filename = OsStr::new(&filename);
    let file = section
        .open_regular(filename, false)?
        .ok_or_else(not_installed)?;
    let signed = String::from_utf8(lillux::read_open_regular_file_bounded(
        file.try_clone()?,
        ryeos_node_runtime::node_document::MAX_ITEM_BYTES,
    )?)
    .with_context(|| format!("binding `{binding}` is not UTF-8"))?;
    let body = verify_node_signed(&signed, &identity, &format!("binding `{binding}`"))?;
    let Some(schema) = coherence::retired_binding_schema(&binding.section, &body) else {
        let seen = body
            .get("schema")
            .map_or_else(|| "none".to_owned(), Value::to_string);
        return Err(refuse(
            BINDING_NOT_RETIRED_SCHEMA,
            format!(
                "its signed schema ({seen}) is not a retired one; retiring a live binding is a \
                 different operation this verb does not perform"
            ),
        ));
    };
    let handles = extract_handles(&binding.section, schema, &body);
    let file_path = section.path().join(filename).display().to_string();
    if !request.dry_run {
        section
            .remove_if_same(filename, &file)
            .with_context(|| format!("remove {file_path}"))?;
    }
    Ok(RetireReport {
        dry_run: request.dry_run,
        binding,
        schema,
        file: file_path,
        signer: identity.fingerprint().to_owned(),
        removed: !request.dry_run,
        handles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_config::sections::runtime_snapshot_production::compose::tests::{
        StoppedNode, install_production, stopped_node,
    };

    fn write_signed(
        node: &StoppedNode,
        section: &str,
        id: &str,
        body: &Value,
    ) -> std::path::PathBuf {
        let identity = load_node_identity(node.root.path()).unwrap();
        let dir = node.root.path().join(".ai/node").join(section);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.yaml"));
        std::fs::write(
            &path,
            ryeos_node_runtime::node_document::render_signed_item(section, id, body, &identity)
                .unwrap(),
        )
        .unwrap();
        path
    }

    fn old_body(section: &str, schema: u64) -> Value {
        let mut body = serde_json::json!({
            "kind": "node",
            "schema": schema,
            "protocol": "any",
            "backend": "old-backend",
            "account": "old-account",
        });
        if section != runtime_snapshot_qualification::SECTION_NAME {
            body["credential_generation"] = Value::from("c".repeat(64));
        }
        body
    }

    fn request<'a>(node: &'a StoppedNode, binding: &str, dry_run: bool) -> RetireRequest<'a> {
        RetireRequest {
            app_root: node.root.path(),
            state_lock: &node.lock,
            binding: BindingRef::parse(binding).unwrap(),
            dry_run,
        }
    }

    fn refusal_code(error: &anyhow::Error) -> &'static str {
        error.downcast_ref::<RetireRefusal>().unwrap().code
    }

    #[test]
    fn every_retired_schema_has_exactly_one_handle_layout() {
        let retired = coherence::RETIRED_BINDING_SCHEMAS
            .iter()
            .flat_map(|(section, numbers)| numbers.iter().map(move |n| (*section, *n)))
            .collect::<std::collections::BTreeSet<_>>();
        let described = RETIRED_HANDLE_SHAPES
            .iter()
            .map(|(section, number, _)| (*section, *number))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(retired, described);
        assert_eq!(RETIRED_HANDLE_SHAPES.len(), described.len());
    }

    #[test]
    fn retire_removes_a_stale_file_and_reports_the_handle_for_each_retired_schema() {
        for (section, schema, shape) in RETIRED_HANDLE_SHAPES {
            let node = stopped_node();
            let path = write_signed(&node, section, "old", &old_body(section, *schema));
            let binding = format!("{section}/old");
            let dry = retire_binding(request(&node, &binding, true)).unwrap();
            assert!(dry.dry_run && !dry.removed && path.exists());
            let report = retire_binding(request(&node, &binding, false)).unwrap();
            assert!(report.removed && !path.exists(), "{section} {schema}");
            assert_eq!(report.schema, *schema);
            assert_eq!(report.binding.to_string(), binding);
            match shape {
                HandleShape::CredentialGeneration => assert_eq!(
                    report.handles,
                    RetiredHandles::Credential {
                        backend: "old-backend".into(),
                        account: "old-account".into(),
                        credential_generation: "c".repeat(64),
                    },
                    "{section} {schema}"
                ),
                HandleShape::Carries(_) => assert!(
                    matches!(report.handles, RetiredHandles::NoneCarried { .. }),
                    "{section} {schema}"
                ),
            }
            let error = retire_binding(request(&node, &binding, false)).unwrap_err();
            assert_eq!(refusal_code(&error), RETIRE_BINDING_NOT_INSTALLED);
        }
    }

    #[test]
    fn a_stale_document_without_its_handle_fields_is_not_guessed() {
        let node = stopped_node();
        let mut body = old_body(external_execution::SECTION_NAME, 19);
        body.as_object_mut()
            .unwrap()
            .remove("credential_generation");
        write_signed(&node, external_execution::SECTION_NAME, "odd", &body);
        let report = retire_binding(request(&node, "external_execution/odd", false)).unwrap();
        assert!(matches!(
            report.handles,
            RetiredHandles::NotExtractable { .. }
        ));
    }

    #[test]
    fn retire_refuses_a_current_binding_with_a_typed_code() {
        let node = stopped_node();
        let report = install_production(&node, "producer");
        let error = retire_binding(request(
            &node,
            "runtime_snapshot_production/producer",
            false,
        ))
        .unwrap_err();
        assert_eq!(refusal_code(&error), BINDING_NOT_RETIRED_SCHEMA);
        assert!(error.to_string().contains("binding_not_retired_schema"));
        assert!(report.path.exists());
    }

    #[test]
    fn retire_refuses_a_bad_signature_a_missing_id_and_an_escaping_path() {
        let node = stopped_node();
        let path = write_signed(
            &node,
            external_execution::SECTION_NAME,
            "old",
            &old_body(external_execution::SECTION_NAME, 19),
        );
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("old-account", "other-account")).unwrap();
        let error = retire_binding(request(&node, "external_execution/old", false)).unwrap_err();
        assert!(format!("{error:#}").contains("signature"), "{error:#}");
        assert!(path.exists());

        let error = retire_binding(request(&node, "external_execution/absent", false)).unwrap_err();
        assert_eq!(refusal_code(&error), RETIRE_BINDING_NOT_INSTALLED);

        // The operator's spelling cannot name a path outside the section.
        assert!(BindingRef::parse("external_execution/../identity/private_key").is_err());
        let escaping = BindingRef {
            section: external_execution::SECTION_NAME.to_owned(),
            id: "../runtime_snapshot_production/x".to_owned(),
        };
        let outside = write_signed(
            &node,
            runtime_snapshot_production::SECTION_NAME,
            "x",
            &old_body(runtime_snapshot_production::SECTION_NAME, 1),
        );
        let error = retire_binding(RetireRequest {
            app_root: node.root.path(),
            state_lock: &node.lock,
            binding: escaping,
            dry_run: false,
        })
        .unwrap_err();
        assert!(error.to_string().contains("id is invalid"), "{error:#}");
        assert!(outside.exists());
    }

    #[test]
    fn retire_needs_the_state_lock_of_this_node() {
        let node = stopped_node();
        let other = stopped_node();
        write_signed(
            &node,
            external_execution::SECTION_NAME,
            "old",
            &old_body(external_execution::SECTION_NAME, 19),
        );
        // A lock for another root proves nothing about this one.
        let error = retire_binding(RetireRequest {
            app_root: node.root.path(),
            state_lock: &other.lock,
            binding: BindingRef::parse("external_execution/old").unwrap(),
            dry_run: false,
        })
        .unwrap_err();
        assert!(!format!("{error:#}").is_empty());
        assert!(
            node.root
                .path()
                .join(".ai/node/external_execution/old.yaml")
                .exists()
        );
        // A running node holds the lock: nobody else can take it.
        assert!(
            ryeos_node_runtime::state_lock::StateLock::acquire_with_timeout(
                &ryeos_node_runtime::state_lock::default_lock_path(node.root.path()),
                std::time::Duration::from_millis(100),
            )
            .is_err()
        );
    }

    #[test]
    fn a_binding_can_be_composed_again_after_it_is_retired() {
        let node = stopped_node();
        write_signed(
            &node,
            runtime_snapshot_production::SECTION_NAME,
            "producer",
            &old_body(runtime_snapshot_production::SECTION_NAME, 1),
        );
        retire_binding(request(
            &node,
            "runtime_snapshot_production/producer",
            false,
        ))
        .unwrap();
        let report = install_production(&node, "producer");
        assert!(report.path.exists());
    }
}
