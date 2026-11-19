//! Machine declarations: the operator's statement that these nodes are one
//! machine.
//!
//! Each declaration is one node-signed record,
//! `<app_root>/.ai/node/machines/<label>.yaml` (schema
//! `ryeos/machine-declaration/v1`), written and verified the way remote pins
//! are: the node key signs the record when the local configured operator
//! declares the machine, and the loader verifies that signature on every read,
//! so a declaration cannot be changed by editing a file.
//!
//! A declaration is evidence of nothing but the operator's word. It is graded
//! `declared`; no node admits anything on it.
//!
//! Members are site ids (`site:<name>`) only. An installed-not-connected node
//! has no site id here and cannot be named: a service name identifies an
//! install, not a node. A member need not be known to this node now; a named
//! node that is not seen shows as missing.
//!
//! One bad record never hides the others. A record that is unsigned, edited,
//! signed by another key, malformed or named for another label is reported
//! with a stable code and skipped, and the caller decides what to show. Records
//! that name the same node are set aside together by
//! [`ryeos_effect_contract::locality::exclude_conflicting_declarations`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ryeos_effect_contract::locality::{
    InvalidDeclaration, MachineDeclaration, validate_machine_label,
};
use serde::{Deserialize, Serialize};

use crate::handler_error::HandlerError;

/// Schema of a node-signed machine declaration record.
pub const MACHINE_DECLARATION_SCHEMA: &str = "ryeos/machine-declaration/v1";

/// Upper bound on one record.
const MAX_RECORD_BYTES: u64 = 64 * 1024;
/// Upper bound on the number of records.
const MAX_RECORDS: usize = 256;

/// The signed on-disk record for one declared machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineDeclarationRecord {
    schema: String,
    /// RFC 3339 time the machine was first declared.
    declared_at: String,
    /// Digest of the operator request that made the declaration, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request: Option<String>,
    machine: MachineDeclaration,
}

/// Why a declaration record was refused. Every variant has a stable code and
/// names the cure.
#[derive(Debug, thiserror::Error)]
pub enum MachineDeclarationError {
    #[error(
        "machine_declaration_unsigned: {path} carries no `# ryeos:signed:` line. Declarations \
         must be signed by this node's key. Delete the file and declare the machine again with \
         `ryeos node machine declare <label> --members <site-id>`"
    )]
    Unsigned { path: PathBuf },

    #[error(
        "machine_declaration_wrong_signer: {path} is signed by {signer}, not by this node's key \
         {node}. Delete the file and declare the machine again with \
         `ryeos node machine declare <label> --members <site-id>`"
    )]
    WrongSigner {
        path: PathBuf,
        signer: String,
        node: String,
    },

    #[error(
        "machine_declaration_tampered: {path} {reason}. The record was edited after signing or \
         is malformed. Delete the file and declare the machine again with \
         `ryeos node machine declare <label> --members <site-id>`"
    )]
    Tampered { path: PathBuf, reason: String },

    #[error(
        "machine_declaration_invalid: {path} {reason}. Delete the file and declare the machine \
         again with `ryeos node machine declare <label> --members <site-id>`"
    )]
    Invalid { path: PathBuf, reason: String },

    #[error("machine_declarations_unreadable: {path} cannot be read securely: {reason}")]
    Unreadable { path: PathBuf, reason: String },
}

impl MachineDeclarationError {
    /// Stable machine code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsigned { .. } => "machine_declaration_unsigned",
            Self::WrongSigner { .. } => "machine_declaration_wrong_signer",
            Self::Tampered { .. } => "machine_declaration_tampered",
            Self::Invalid { .. } => "machine_declaration_invalid",
            Self::Unreadable { .. } => "machine_declarations_unreadable",
        }
    }
}

/// A declaration that verified, with where it came from.
#[derive(Debug, Clone)]
pub struct LoadedDeclaration {
    pub declaration: MachineDeclaration,
    pub declared_at: String,
    pub path: PathBuf,
}

/// Every record found: the ones that verified and a report for each that
/// did not.
#[derive(Debug, Clone, Default)]
pub struct DeclarationsReport {
    pub declarations: Vec<LoadedDeclaration>,
    pub invalid: Vec<InvalidDeclaration>,
}

/// Directory holding the node-signed declaration records.
pub fn machine_declarations_dir(app_root: &Path) -> PathBuf {
    ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).machine_declarations_dir()
}

/// Path of one machine's declaration record.
pub fn machine_declaration_path(app_root: &Path, label: &str) -> PathBuf {
    machine_declarations_dir(app_root).join(format!("{label}.yaml"))
}

fn map_trust_error(dir: &Path, error: ryeos_engine::trust::TrustError) -> MachineDeclarationError {
    use ryeos_engine::trust::TrustError;
    match error {
        TrustError::RecordUnsigned { path } => MachineDeclarationError::Unsigned { path },
        TrustError::RecordWrongSigner { path, signer, node } => {
            MachineDeclarationError::WrongSigner { path, signer, node }
        }
        TrustError::RecordTampered { path, reason } => {
            MachineDeclarationError::Tampered { path, reason }
        }
        TrustError::NodeIdentityUnavailable { path, reason } => {
            MachineDeclarationError::Unreadable { path, reason }
        }
        other => MachineDeclarationError::Invalid {
            path: dir.to_path_buf(),
            reason: other.to_string(),
        },
    }
}

/// Verify and validate one record read from `path`.
fn parse_record(
    path: &Path,
    bytes: &[u8],
    node_key: &lillux::crypto::VerifyingKey,
) -> Result<MachineDeclarationRecord, MachineDeclarationError> {
    let dir = path.parent().unwrap_or(path);
    let invalid = |reason: String| MachineDeclarationError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let body = ryeos_engine::trust::verify_node_signed_text(path, bytes, node_key)
        .map_err(|error| map_trust_error(dir, error))?;
    let record: MachineDeclarationRecord =
        serde_yaml::from_str(&body).map_err(|error| MachineDeclarationError::Tampered {
            path: path.to_path_buf(),
            reason: format!("does not match the machine declaration shape: {error}"),
        })?;
    if record.schema != MACHINE_DECLARATION_SCHEMA {
        return Err(invalid(format!(
            "declares schema `{}`, this node reads `{MACHINE_DECLARATION_SCHEMA}`",
            record.schema
        )));
    }
    record
        .machine
        .validate()
        .map_err(|error| invalid(error.to_string()))?;
    for member in &record.machine.members {
        ryeos_node_runtime::identity::validate_canonical_site_id(member).map_err(|error| {
            invalid(format!(
                "member `{member}` is not a canonical site id: {error}"
            ))
        })?;
    }
    let stem = path.file_stem().and_then(|stem| stem.to_str());
    if stem != Some(record.machine.label.as_str()) {
        return Err(invalid(format!(
            "declares machine `{}` but is named for `{}`",
            record.machine.label,
            stem.unwrap_or_default()
        )));
    }
    Ok(record)
}

/// Read every file under the declarations directory. A missing directory is
/// empty.
fn read_record_files(dir: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>, MachineDeclarationError> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    lillux::visit_regular_files_no_follow_bounded(
        dir,
        lillux::DirectoryTraversalBudget::new(MAX_RECORDS + 1, 1),
        |_, is_directory| Ok(is_directory),
        |relative, file| {
            if files.len() >= MAX_RECORDS {
                anyhow::bail!("more than {MAX_RECORDS} machine declaration records");
            }
            let bytes = lillux::read_open_regular_file_bounded(file, MAX_RECORD_BYTES)?;
            files.push((dir.join(relative), bytes));
            Ok(())
        },
    )
    .map_err(|error| MachineDeclarationError::Unreadable {
        path: dir.to_path_buf(),
        reason: format!("{error:#}"),
    })?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Load every declaration, reporting each record that fails verification or
/// validation instead of failing the load. Only an unreadable directory or
/// node identity fails it.
pub fn load_declarations(app_root: &Path) -> Result<DeclarationsReport> {
    let dir = machine_declarations_dir(app_root);
    let mut report = DeclarationsReport::default();
    let files = read_record_files(&dir)?;
    if files.is_empty() {
        return Ok(report);
    }
    let node_key = ryeos_engine::trust::load_node_verifying_key(app_root)
        .map_err(|error| map_trust_error(&dir, error))?;
    for (path, bytes) in files {
        let label = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        let is_yaml = path.extension().and_then(|ext| ext.to_str()) == Some("yaml");
        let outcome = if is_yaml {
            parse_record(&path, &bytes, &node_key)
        } else {
            Err(MachineDeclarationError::Invalid {
                path: path.clone(),
                reason: "is not a `<label>.yaml` machine declaration record".to_owned(),
            })
        };
        match outcome {
            Ok(record) => report.declarations.push(LoadedDeclaration {
                declaration: record.machine,
                declared_at: record.declared_at,
                path,
            }),
            Err(error) => {
                tracing::warn!(
                    target: "ryeos_api::machine_declaration",
                    path = %path.display(),
                    code = error.code(),
                    "skipping unusable machine declaration record"
                );
                report.invalid.push(InvalidDeclaration {
                    label,
                    code: error.code().to_owned(),
                    message: error.to_string(),
                });
            }
        }
    }
    Ok(report)
}

/// Write one machine's node-signed declaration, replacing any previous record
/// for the label. Callers must have admitted the local operator first.
///
/// Refused with a conflict when a member is already named by another usable
/// declaration: a node is on one machine.
pub fn write_declaration(
    app_root: &Path,
    node_key: &lillux::crypto::SigningKey,
    declaration: &MachineDeclaration,
    request_digest: Option<String>,
) -> Result<PathBuf> {
    declaration
        .validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    for member in &declaration.members {
        ryeos_node_runtime::identity::validate_canonical_site_id(member).map_err(|error| {
            HandlerError::BadRequest(format!(
                "member `{member}` is not a canonical site id: {error}"
            ))
        })?;
    }
    let dir = machine_declarations_dir(app_root);
    let directory = lillux::PinnedDirectory::open_or_create(&dir)
        .with_context(|| format!("open machine declaration directory {}", dir.display()))?;
    let _lock = directory.lock_exclusive()?;
    let path = machine_declaration_path(app_root, &declaration.label);
    let verifying = node_key.verifying_key();
    let mut declared_at = None;
    for (existing, bytes) in read_record_files(&dir)? {
        let Ok(record) = parse_record(&existing, &bytes, &verifying) else {
            continue;
        };
        if existing == path {
            declared_at = Some(record.declared_at);
        } else if let Some(shared) = record
            .machine
            .members
            .iter()
            .find(|member| declaration.members.contains(member))
        {
            return Err(HandlerError::Conflict(format!(
                "node `{shared}` is already named by machine `{}`; remove that declaration first",
                record.machine.label
            ))
            .into());
        }
    }
    let record = MachineDeclarationRecord {
        schema: MACHINE_DECLARATION_SCHEMA.to_owned(),
        declared_at: declared_at.unwrap_or_else(lillux::time::iso8601_now),
        request: request_digest,
        machine: declaration.clone(),
    };
    let body = serde_yaml::to_string(&record).context("serialize machine declaration record")?;
    let signed = lillux::signature::sign_content(&body, node_key, "#", None);
    lillux::atomic_write(&path, signed.as_bytes())
        .with_context(|| format!("write machine declaration record {}", path.display()))?;
    Ok(path)
}

/// Remove one machine's declaration record, whether or not it still verifies.
/// A label with no record is not found.
pub fn remove_declaration(app_root: &Path, label: &str) -> Result<PathBuf> {
    validate_machine_label(label).map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let path = machine_declaration_path(app_root, label);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(HandlerError::Conflict(format!(
                "{} is not a regular file; remove it by hand",
                path.display()
            ))
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(HandlerError::NotFound.into());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", path.display()));
        }
    }
    std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_fixture(app_root: &Path) -> lillux::crypto::SigningKey {
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(
            &ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).node_signing_key_path(),
        )
        .unwrap();
        identity
            .write_public_identity(&ryeos_engine::trust::node_public_identity_path(app_root))
            .unwrap();
        identity.signing_key().clone()
    }

    fn declaration(label: &str, members: &[&str]) -> MachineDeclaration {
        MachineDeclaration {
            label: label.to_owned(),
            members: members.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    fn setup() -> (tempfile::TempDir, lillux::crypto::SigningKey) {
        let tmp = tempfile::tempdir().unwrap();
        let key = node_fixture(tmp.path());
        (tmp, key)
    }

    #[test]
    fn a_declared_machine_loads_and_keeps_its_declaration_time() {
        let (tmp, key) = setup();
        let one = declaration("workstation", &["site:a", "site:b"]);
        let path = write_declaration(tmp.path(), &key, &one, None).unwrap();
        assert_eq!(path, machine_declaration_path(tmp.path(), "workstation"));
        let report = load_declarations(tmp.path()).unwrap();
        assert!(report.invalid.is_empty(), "{:?}", report.invalid);
        assert_eq!(report.declarations.len(), 1);
        assert_eq!(report.declarations[0].declaration, one);
        let first = report.declarations[0].declared_at.clone();

        let more = declaration("workstation", &["site:a", "site:b", "site:c"]);
        write_declaration(tmp.path(), &key, &more, Some("digest".into())).unwrap();
        let report = load_declarations(tmp.path()).unwrap();
        assert_eq!(report.declarations[0].declaration, more);
        assert_eq!(report.declarations[0].declared_at, first);
    }

    #[test]
    fn no_directory_is_no_declarations() {
        let (tmp, _key) = setup();
        let report = load_declarations(tmp.path()).unwrap();
        assert!(report.declarations.is_empty() && report.invalid.is_empty());
    }

    #[test]
    fn one_bad_record_is_reported_and_the_rest_still_load() {
        let (tmp, key) = setup();
        write_declaration(tmp.path(), &key, &declaration("good", &["site:g"]), None).unwrap();
        let victim =
            write_declaration(tmp.path(), &key, &declaration("edited", &["site:e"]), None).unwrap();
        let text = std::fs::read_to_string(&victim).unwrap();
        std::fs::write(&victim, text.replace("site:e", "site:other")).unwrap();
        let dir = machine_declarations_dir(tmp.path());
        std::fs::write(dir.join("unsigned.yaml"), "schema: x\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();

        // A record signed by another key.
        let other = lillux::crypto::SigningKey::from_bytes(&[7u8; 32]);
        let foreign = MachineDeclarationRecord {
            schema: MACHINE_DECLARATION_SCHEMA.to_owned(),
            declared_at: "2026-01-01T00:00:00Z".to_owned(),
            request: None,
            machine: declaration("foreign", &["site:f"]),
        };
        std::fs::write(
            dir.join("foreign.yaml"),
            lillux::signature::sign_content(
                &serde_yaml::to_string(&foreign).unwrap(),
                &other,
                "#",
                None,
            ),
        )
        .unwrap();

        // A record signed by the node but named for another label.
        let misnamed = MachineDeclarationRecord {
            machine: declaration("elsewhere", &["site:m"]),
            ..foreign
        };
        std::fs::write(
            dir.join("misnamed.yaml"),
            lillux::signature::sign_content(
                &serde_yaml::to_string(&misnamed).unwrap(),
                &key,
                "#",
                None,
            ),
        )
        .unwrap();

        let report = load_declarations(tmp.path()).unwrap();
        let loaded: Vec<_> = report
            .declarations
            .iter()
            .map(|loaded| loaded.declaration.label.as_str())
            .collect();
        assert_eq!(loaded, ["good"]);
        let mut codes: Vec<(&str, &str)> = report
            .invalid
            .iter()
            .map(|entry| (entry.label.as_str(), entry.code.as_str()))
            .collect();
        codes.sort_unstable();
        assert_eq!(
            codes,
            [
                ("edited", "machine_declaration_tampered"),
                ("foreign", "machine_declaration_wrong_signer"),
                ("misnamed", "machine_declaration_invalid"),
                ("notes", "machine_declaration_invalid"),
                ("unsigned", "machine_declaration_unsigned"),
            ]
        );
    }

    #[test]
    fn a_node_cannot_be_declared_into_two_machines() {
        let (tmp, key) = setup();
        write_declaration(
            tmp.path(),
            &key,
            &declaration("one", &["site:a", "site:b"]),
            None,
        )
        .unwrap();
        let error = write_declaration(tmp.path(), &key, &declaration("two", &["site:b"]), None)
            .unwrap_err();
        assert!(error.to_string().contains("already named by machine `one`"));
        // The same machine may be redeclared over itself.
        write_declaration(
            tmp.path(),
            &key,
            &declaration("one", &["site:a", "site:b"]),
            None,
        )
        .unwrap();
    }

    #[test]
    fn a_malformed_declaration_is_refused_before_anything_is_written() {
        let (tmp, key) = setup();
        for bad in [
            declaration("Bad", &["site:a"]),
            declaration("ok", &[]),
            declaration("ok", &["installed:ryeos-ab"]),
            declaration("ok", &["site:has space"]),
        ] {
            assert!(
                write_declaration(tmp.path(), &key, &bad, None).is_err(),
                "{bad:?}"
            );
        }
        assert!(
            !machine_declarations_dir(tmp.path())
                .join("ok.yaml")
                .exists()
        );
    }

    #[test]
    fn removing_a_declaration_deletes_its_record_and_a_missing_one_is_not_found() {
        let (tmp, key) = setup();
        write_declaration(tmp.path(), &key, &declaration("gone", &["site:a"]), None).unwrap();
        remove_declaration(tmp.path(), "gone").unwrap();
        assert!(
            load_declarations(tmp.path())
                .unwrap()
                .declarations
                .is_empty()
        );
        let error = remove_declaration(tmp.path(), "gone").unwrap_err();
        assert!(matches!(
            error.downcast_ref::<HandlerError>(),
            Some(HandlerError::NotFound)
        ));
        assert!(remove_declaration(tmp.path(), "../escape").is_err());
    }

    #[test]
    fn the_record_schema_is_pinned() {
        assert_eq!(MACHINE_DECLARATION_SCHEMA, "ryeos/machine-declaration/v1");
    }
}
