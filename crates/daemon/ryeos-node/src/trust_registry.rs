//! The only writer of the node signer registry.
//!
//! Records live in `<app_root>/.ai/node/trust/signers/<fingerprint>.yaml` and
//! key events in `<app_root>/.ai/node/trust/events/`. Every file is signed
//! with the node key. A caller proves it holds the node state lock by passing
//! it; nothing else in the process writes these directories.
//!
//! A change appends one hash-chained event per record it alters, writing the
//! event first and the record second. A request that died between the two is
//! finished by running it again: the event carries the request digest, so the
//! rerun finds it and completes only the missing record.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use lillux::crypto::{SigningKey, VerifyingKey};
use ryeos_engine::trust::{
    SignerRecord, SignerRole, SignerScope, SignerStatus, SuccessionStatement, TRUST_EVENT_SCHEMA,
    TrustEvent, TrustEventKind, TrustStore, cross_check_records_against_events, events_dir,
    load_event_chain, load_node_verifying_key, load_signer_records, registry_digest, signers_dir,
};
use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};

/// Stable refusal codes of the trust operations.
#[derive(Debug)]
pub enum TrustChangeError {
    NodeRunning,
    Unknown { fingerprint: String },
    ExistsDifferentScope { fingerprint: String },
    Retired { fingerprint: String },
    NodeKey { fingerprint: String },
    ScopeInvalid { reason: String },
    SuccessionInvalid { reason: String },
}

impl std::fmt::Display for TrustChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustChangeError::NodeRunning => write!(
                f,
                "node_running_trust_change: the node is running and holds its state lock, so \
                 its signer registry cannot change. Stop the node, or submit the request with \
                 `ryeos node offline-operation submit --stop`"
            ),
            TrustChangeError::Unknown { fingerprint } => write!(
                f,
                "trust_signer_unknown: no signer {fingerprint} is pinned. List signers with \
                 `ryeos trust list`"
            ),
            TrustChangeError::ExistsDifferentScope { fingerprint } => write!(
                f,
                "trust_signer_exists_different_scope: signer {fingerprint} is already pinned \
                 with a different role or scope. Change its scope with \
                 `ryeos trust scope {fingerprint} ...`"
            ),
            TrustChangeError::Retired { fingerprint } => write!(
                f,
                "trust_signer_retired: signer {fingerprint} is retired and cannot be pinned \
                 again. Inspect it with `ryeos trust show {fingerprint}` and pin a new key instead"
            ),
            TrustChangeError::NodeKey { fingerprint } => write!(
                f,
                "trust_key_is_node_key: {fingerprint} is this node's own key, the implicit \
                 root of trust; it has no record and cannot be pinned"
            ),
            TrustChangeError::ScopeInvalid { reason } => {
                write!(f, "trust_scope_invalid: {reason}")
            }
            TrustChangeError::SuccessionInvalid { reason } => {
                write!(f, "trust_succession_invalid: {reason}")
            }
        }
    }
}

impl std::error::Error for TrustChangeError {}

impl TrustChangeError {
    pub fn code(&self) -> &'static str {
        match self {
            TrustChangeError::NodeRunning => "node_running_trust_change",
            TrustChangeError::Unknown { .. } => "trust_signer_unknown",
            TrustChangeError::ExistsDifferentScope { .. } => "trust_signer_exists_different_scope",
            TrustChangeError::Retired { .. } => "trust_signer_retired",
            TrustChangeError::NodeKey { .. } => "trust_key_is_node_key",
            TrustChangeError::ScopeInvalid { .. } => "trust_scope_invalid",
            TrustChangeError::SuccessionInvalid { .. } => "trust_succession_invalid",
        }
    }
}

/// Refuse a trust change while a node holds the state lock.
pub fn refuse_while_running(app_root: &Path) -> Result<(), TrustChangeError> {
    match StateLock::acquire(&default_lock_path(app_root)) {
        Ok(_lock) => Ok(()),
        Err(_) => Err(TrustChangeError::NodeRunning),
    }
}

/// Who asked and under which request.
pub struct Actor<'a> {
    pub node_key: &'a SigningKey,
    /// Fingerprint of the operator who signed the request.
    pub operator: &'a str,
    /// Offline request digest, or the init digest.
    pub request_digest: &'a str,
}

/// One requested change.
pub enum Change {
    Pin {
        key: VerifyingKey,
        label: String,
        role: SignerRole,
        scope: SignerScope,
    },
    Scope {
        fingerprint: String,
        scope: SignerScope,
    },
    Retire {
        fingerprint: String,
    },
    Revoke {
        fingerprint: String,
    },
    Succeed {
        old_fingerprint: String,
        statement: String,
    },
}

/// What a change did.
#[derive(Debug)]
pub struct Outcome {
    /// Records as they stand after the change, one per record it touches.
    pub records: Vec<SignerRecord>,
    /// Events this call wrote; empty when the registry already held the
    /// requested state.
    pub events_written: usize,
    /// Registry digest after the change.
    pub registry_digest: String,
}

/// The registry as the trust verbs read it: records plus the verified chain.
pub struct RegistryView {
    pub records: Vec<SignerRecord>,
    pub chain: Vec<TrustEvent>,
    pub registry_digest: String,
}

/// Load and cross-check the registry. Fails with the typed engine error on
/// any bad record, a broken chain, or a record that names an event that does
/// not commit to it.
pub fn load_view(app_root: &Path) -> Result<RegistryView> {
    let node_key = load_node_verifying_key(app_root)?;
    let records = load_signer_records(&signers_dir(app_root), &node_key)?;
    let chain = load_event_chain(&events_dir(app_root), &node_key)?;
    cross_check_records_against_events(&records, &chain)?;
    let digest = registry_digest(records.iter());
    Ok(RegistryView {
        records,
        chain,
        registry_digest: digest,
    })
}

/// Digest of the registry records as `TrustStore::digest` reports it.
pub fn current_digest(app_root: &Path) -> Result<String> {
    Ok(TrustStore::load_node_registry(app_root)?.digest())
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let directory = lillux::PinnedDirectory::open_or_create(dir)
        .with_context(|| format!("establish trust directory {}", dir.display()))?;
    let _lock = directory
        .lock_exclusive()
        .with_context(|| format!("lock trust directory {}", dir.display()))?;
    let name = OsStr::new(name);
    let expected = directory.open_pinned_regular(name, false)?;
    directory
        .atomic_write_pinned_if_same(
            name,
            expected.as_ref(),
            bytes,
            0o600,
            crate::NODE_TEMP_PURPOSE,
        )
        .with_context(|| format!("publish {}", dir.join(name).display()))?;
    Ok(())
}

struct Step {
    kind: TrustEventKind,
    record: SignerRecord,
}

fn replaced(
    current: &SignerRecord,
    scope: Option<SignerScope>,
    status: Option<SignerStatus>,
) -> SignerRecord {
    let mut next = current.clone();
    if let Some(scope) = scope {
        next.scope = scope;
    }
    if let Some(status) = status {
        next.status = status;
    }
    next
}

fn find<'a>(records: &'a [SignerRecord], fingerprint: &str) -> Option<&'a SignerRecord> {
    records.iter().find(|r| r.fingerprint == fingerprint)
}

fn require_valid_scope(scope: &SignerScope) -> Result<(), TrustChangeError> {
    scope
        .validate()
        .map_err(|reason| TrustChangeError::ScopeInvalid { reason })
}

/// Derive the steps a change needs from the current records. An empty list is
/// an idempotent no-op.
fn plan(
    change: &Change,
    records: &[SignerRecord],
    node_fingerprint: &str,
) -> Result<Vec<Step>, anyhow::Error> {
    let revoked = |r: &SignerRecord| {
        anyhow!(ryeos_engine::trust::TrustError::SignerRevoked {
            fingerprint: r.fingerprint.clone(),
            label: r.label.clone(),
        })
    };
    match change {
        Change::Pin {
            key,
            label,
            role,
            scope,
        } => {
            require_valid_scope(scope)?;
            let candidate =
                SignerRecord::new(key, label, *role, scope.clone(), SignerStatus::Active);
            if candidate.fingerprint == node_fingerprint {
                return Err(TrustChangeError::NodeKey {
                    fingerprint: candidate.fingerprint,
                }
                .into());
            }
            match find(records, &candidate.fingerprint) {
                None => Ok(vec![Step {
                    kind: TrustEventKind::Pinned,
                    record: candidate,
                }]),
                Some(existing) if existing.status == SignerStatus::Revoked => {
                    Err(revoked(existing))
                }
                Some(existing) if existing.status == SignerStatus::Retired => {
                    Err(TrustChangeError::Retired {
                        fingerprint: existing.fingerprint.clone(),
                    }
                    .into())
                }
                Some(existing) if existing.role == *role && existing.scope == *scope => Ok(vec![]),
                Some(existing) => Err(TrustChangeError::ExistsDifferentScope {
                    fingerprint: existing.fingerprint.clone(),
                }
                .into()),
            }
        }
        Change::Scope { fingerprint, scope } => {
            require_valid_scope(scope)?;
            let existing = find(records, fingerprint).ok_or_else(|| TrustChangeError::Unknown {
                fingerprint: fingerprint.clone(),
            })?;
            if existing.status == SignerStatus::Revoked {
                return Err(revoked(existing));
            }
            if existing.scope == *scope {
                return Ok(vec![]);
            }
            Ok(vec![Step {
                kind: TrustEventKind::ScopeChanged,
                record: replaced(existing, Some(scope.clone()), None),
            }])
        }
        Change::Retire { fingerprint } => {
            let existing = find(records, fingerprint).ok_or_else(|| TrustChangeError::Unknown {
                fingerprint: fingerprint.clone(),
            })?;
            match existing.status {
                SignerStatus::Revoked => Err(revoked(existing)),
                SignerStatus::Retired => Ok(vec![]),
                SignerStatus::Active => Ok(vec![Step {
                    kind: TrustEventKind::Retired,
                    record: replaced(existing, None, Some(SignerStatus::Retired)),
                }]),
            }
        }
        Change::Revoke { fingerprint } => {
            let existing = find(records, fingerprint).ok_or_else(|| TrustChangeError::Unknown {
                fingerprint: fingerprint.clone(),
            })?;
            if existing.status == SignerStatus::Revoked {
                return Ok(vec![]);
            }
            Ok(vec![Step {
                kind: TrustEventKind::Revoked,
                record: replaced(existing, None, Some(SignerStatus::Revoked)),
            }])
        }
        Change::Succeed {
            old_fingerprint,
            statement,
        } => {
            let old = find(records, old_fingerprint).ok_or_else(|| TrustChangeError::Unknown {
                fingerprint: old_fingerprint.clone(),
            })?;
            if old.status == SignerStatus::Revoked {
                return Err(revoked(old));
            }
            let parsed = SuccessionStatement::parse_and_verify(statement, old)
                .map_err(|reason| TrustChangeError::SuccessionInvalid { reason })?;
            let new_key = parsed
                .new_verifying_key()
                .map_err(|reason| TrustChangeError::SuccessionInvalid { reason })?;
            let new_record = SignerRecord::new(
                &new_key,
                &old.label,
                old.role,
                old.scope.clone(),
                SignerStatus::Active,
            );
            if new_record.fingerprint == node_fingerprint {
                return Err(TrustChangeError::NodeKey {
                    fingerprint: new_record.fingerprint,
                }
                .into());
            }
            let mut steps = Vec::new();
            match find(records, &new_record.fingerprint) {
                None => steps.push(Step {
                    kind: TrustEventKind::Succeeded,
                    record: new_record,
                }),
                Some(existing) if existing.status == SignerStatus::Revoked => {
                    return Err(revoked(existing));
                }
                Some(_) => {}
            }
            if old.status == SignerStatus::Active {
                steps.push(Step {
                    kind: TrustEventKind::Retired,
                    record: replaced(old, None, Some(SignerStatus::Retired)),
                });
            }
            Ok(steps)
        }
    }
}

/// Apply one change under the node state lock.
pub fn apply_change(
    app_root: &Path,
    _lock: &StateLock,
    actor: &Actor<'_>,
    change: &Change,
) -> Result<Outcome> {
    let view = load_view(app_root)?;
    let node_key = load_node_verifying_key(app_root)?;
    let node_fingerprint = ryeos_engine::trust::compute_fingerprint(&node_key);
    let steps = plan(change, &view.records, &node_fingerprint)?;
    write_steps(app_root, actor, view, steps)
}

/// Record the signers a fresh or re-run `ryeos init` pins. Existing records
/// for the same key are kept as they are; a revoked key is refused.
pub fn record_init_signers(
    app_root: &Path,
    lock: &StateLock,
    actor: &Actor<'_>,
    signers: &[(VerifyingKey, String, SignerRole, SignerScope)],
) -> Result<Outcome> {
    let mut combined = Outcome {
        records: Vec::new(),
        events_written: 0,
        registry_digest: String::new(),
    };
    for (key, label, role, scope) in signers {
        let view = load_view(app_root)?;
        let fingerprint = ryeos_engine::trust::compute_fingerprint(key);
        if let Some(existing) = find(&view.records, &fingerprint) {
            if existing.status == SignerStatus::Revoked {
                return Err(anyhow!(ryeos_engine::trust::TrustError::SignerRevoked {
                    fingerprint,
                    label: existing.label.clone(),
                }));
            }
            combined.records.push(existing.clone());
            continue;
        }
        let node_key = load_node_verifying_key(app_root)?;
        let node_fingerprint = ryeos_engine::trust::compute_fingerprint(&node_key);
        let steps = plan(
            &Change::Pin {
                key: *key,
                label: label.clone(),
                role: *role,
                scope: scope.clone(),
            },
            &view.records,
            &node_fingerprint,
        )?;
        let _ = lock;
        let outcome = write_steps(app_root, actor_for_init(actor), view, steps)?;
        combined.events_written += outcome.events_written;
        combined.records.extend(outcome.records);
    }
    combined.registry_digest = current_digest(app_root)?;
    Ok(combined)
}

fn actor_for_init<'a, 'b>(actor: &'b Actor<'a>) -> &'b Actor<'a> {
    actor
}

fn write_steps(
    app_root: &Path,
    actor: &Actor<'_>,
    view: RegistryView,
    steps: Vec<Step>,
) -> Result<Outcome> {
    let signers = signers_dir(app_root);
    let events = events_dir(app_root);
    let mut chain = view.chain;
    let mut outcome_records = Vec::new();
    let mut written = 0;
    for step in steps {
        let fingerprint = step.record.fingerprint.clone();
        let content_digest = step.record.content_digest();
        let already = chain
            .iter()
            .find(|e| e.request_digest == actor.request_digest && e.signer == fingerprint)
            .cloned();
        let mut record = step.record;
        match already {
            Some(event) => {
                // The event for this request was written before a crash; only
                // the record can be missing.
                if event.record_digest != content_digest {
                    return Err(anyhow!(
                        "trust_event_chain_broken: event {} for this request commits to a \
                         different record than the one the request now produces",
                        event.digest()
                    ));
                }
                record.event = event.digest();
            }
            None => {
                let event = TrustEvent {
                    schema: TRUST_EVENT_SCHEMA.to_string(),
                    seq: chain.len() as u64 + 1,
                    previous: chain.last().map(TrustEvent::digest),
                    kind: step.kind,
                    signer: fingerprint.clone(),
                    record_digest: content_digest,
                    request_digest: actor.request_digest.to_string(),
                    operator: actor.operator.to_string(),
                };
                write_file(
                    &events,
                    &event.file_name(),
                    event.render_signed(actor.node_key)?.as_bytes(),
                )?;
                record.event = event.digest();
                chain.push(event);
                written += 1;
            }
        }
        write_file(
            &signers,
            &record.file_name(),
            record.render_signed(actor.node_key)?.as_bytes(),
        )?;
        outcome_records.push(record);
    }
    Ok(Outcome {
        records: outcome_records,
        events_written: written,
        registry_digest: current_digest(app_root)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_setup() -> (tempfile::TempDir, SigningKey) {
        use base64::Engine as _;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".ai/node/identity");
        std::fs::create_dir_all(&dir).unwrap();
        let node = lillux::crypto::create_signing_key(&dir.join("private_key.pem")).unwrap();
        let node_vk = node.verifying_key();
        let doc = serde_json::json!({
            "kind": "identity/v1",
            "principal_id": format!("fp:{}", ryeos_engine::trust::compute_fingerprint(&node_vk)),
            "signing_key": format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(node_vk.as_bytes())
            ),
        });
        std::fs::write(dir.join("public-identity.json"), doc.to_string()).unwrap();
        (root, node)
    }

    fn actor<'a>(node: &'a SigningKey, request: &'a str) -> Actor<'a> {
        Actor {
            node_key: node,
            operator: "op",
            request_digest: request,
        }
    }

    fn lock(root: &Path) -> StateLock {
        StateLock::acquire(&default_lock_path(root)).unwrap()
    }

    fn pin(vk: VerifyingKey, scope: SignerScope) -> Change {
        Change::Pin {
            key: vk,
            label: "t".into(),
            role: SignerRole::ProjectAuthor,
            scope,
        }
    }

    fn project_scope(p: &str) -> SignerScope {
        SignerScope {
            spaces: vec![ryeos_engine::trust::SignerSpace::Project],
            kinds: vec!["*".into()],
            projects: vec![p.into()],
        }
    }

    #[test]
    fn trust_pin_writes_signed_record_and_event() {
        let (root, node) = node_setup();
        let l = lock(root.path());
        let vk = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let out = apply_change(
            root.path(),
            &l,
            &actor(&node, "r1"),
            &pin(vk, project_scope("/p")),
        )
        .unwrap();
        assert_eq!(out.events_written, 1);
        let store = TrustStore::load_node_registry(root.path()).unwrap();
        assert!(store.is_trusted(&ryeos_engine::trust::compute_fingerprint(&vk)));
    }

    #[test]
    fn trust_pin_is_idempotent_for_same_key_role_scope() {
        let (root, node) = node_setup();
        let l = lock(root.path());
        let vk = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        apply_change(
            root.path(),
            &l,
            &actor(&node, "r1"),
            &pin(vk, project_scope("/p")),
        )
        .unwrap();
        let again = apply_change(
            root.path(),
            &l,
            &actor(&node, "r2"),
            &pin(vk, project_scope("/p")),
        )
        .unwrap();
        assert_eq!(again.events_written, 0);
    }

    #[test]
    fn trust_pin_refuses_existing_different_scope() {
        let (root, node) = node_setup();
        let l = lock(root.path());
        let vk = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        apply_change(
            root.path(),
            &l,
            &actor(&node, "r1"),
            &pin(vk, project_scope("/p")),
        )
        .unwrap();
        let err = apply_change(
            root.path(),
            &l,
            &actor(&node, "r2"),
            &pin(vk, project_scope("/q")),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("trust_signer_exists_different_scope")
        );
        assert!(err.to_string().contains("ryeos trust scope"));
    }

    #[test]
    fn trust_revoke_is_terminal_and_retire_then_pin_is_refused() {
        let (root, node) = node_setup();
        let l = lock(root.path());
        let vk = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let fp = ryeos_engine::trust::compute_fingerprint(&vk);
        apply_change(
            root.path(),
            &l,
            &actor(&node, "r1"),
            &pin(vk, project_scope("/p")),
        )
        .unwrap();
        apply_change(
            root.path(),
            &l,
            &actor(&node, "r2"),
            &Change::Retire {
                fingerprint: fp.clone(),
            },
        )
        .unwrap();
        let err = apply_change(
            root.path(),
            &l,
            &actor(&node, "r3"),
            &pin(vk, project_scope("/p")),
        )
        .unwrap_err();
        assert!(err.to_string().contains("ryeos trust show"));
        apply_change(
            root.path(),
            &l,
            &actor(&node, "r4"),
            &Change::Revoke {
                fingerprint: fp.clone(),
            },
        )
        .unwrap();
        let err = apply_change(
            root.path(),
            &l,
            &actor(&node, "r5"),
            &Change::Retire { fingerprint: fp },
        )
        .unwrap_err();
        assert!(err.to_string().contains("trust_signer_revoked"));
    }

    #[test]
    fn rerun_after_event_before_record_completes_the_record() {
        let (root, node) = node_setup();
        let l = lock(root.path());
        let vk = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let fp = ryeos_engine::trust::compute_fingerprint(&vk);
        apply_change(
            root.path(),
            &l,
            &actor(&node, "r1"),
            &pin(vk, project_scope("/p")),
        )
        .unwrap();
        // Simulate a crash after the retire event but before its record.
        let view = load_view(root.path()).unwrap();
        let steps = plan(
            &Change::Retire {
                fingerprint: fp.clone(),
            },
            &view.records,
            "x",
        )
        .unwrap();
        let step = &steps[0];
        let event = TrustEvent {
            schema: TRUST_EVENT_SCHEMA.to_string(),
            seq: view.chain.len() as u64 + 1,
            previous: view.chain.last().map(TrustEvent::digest),
            kind: step.kind,
            signer: fp.clone(),
            record_digest: step.record.content_digest(),
            request_digest: "r2".into(),
            operator: "op".into(),
        };
        write_file(
            &events_dir(root.path()),
            &event.file_name(),
            event.render_signed(&node).unwrap().as_bytes(),
        )
        .unwrap();
        // The records no longer match the chain head, but each record still
        // names a committed event; the rerun finishes the retire.
        apply_change(
            root.path(),
            &l,
            &actor(&node, "r2"),
            &Change::Retire {
                fingerprint: fp.clone(),
            },
        )
        .unwrap();
        let store = TrustStore::load_node_registry(root.path()).unwrap();
        assert_eq!(store.signer(&fp).unwrap().status, SignerStatus::Retired);
    }

    #[test]
    fn event_chain_survives_many_changes() {
        let (root, node) = node_setup();
        let l = lock(root.path());
        for i in 0..3u8 {
            let vk = SigningKey::from_bytes(&[20 + i; 32]).verifying_key();
            apply_change(
                root.path(),
                &l,
                &actor(&node, &format!("r{i}")),
                &pin(vk, project_scope("/p")),
            )
            .unwrap();
        }
        let view = load_view(root.path()).unwrap();
        assert_eq!(view.chain.len(), 3);
    }
}
