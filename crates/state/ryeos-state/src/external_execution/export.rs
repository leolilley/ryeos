//! Candidate-only content assembly using the existing project CAS contract.
//!
//! A complete content closure is not a trusted writer-exclusion certificate,
//! an evaluation, or permission to publish. The owning completion path must
//! independently verify those claims and root the result before releasing its
//! CAS mutation guard. No archive extraction, guest pathname or URL is accepted.

use std::collections::BTreeSet;

use super::*;
use crate::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
use crate::project_materialization::VerifiedProjectSnapshotClosure;
use crate::{CasMutationGuard, PinnedStateAuthority};

const MAX_TRANSFER_MEMBERS: usize = 100_000;
const MAX_SINGLE_MEMBER_BYTES: u64 = 32 * 1024 * 1024;

struct PartialMember {
    kind: ExportContentKind,
    hash: String,
    bytes: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn authenticated(
        binding: &ExecutionChannelBinding,
        key: &SigningKey,
        direction: ChannelDirection,
        payload: ExecutionChannelPayload,
    ) -> AuthenticatedExecutionFrame {
        let frame = ExecutionFrame {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            direction,
            sequence: 1,
            previous_frame_digest: None,
            acknowledged_peer_sequence: 0,
            payload,
        };
        let signed = SignedExecutionFrame::sign(frame, binding, key).unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(signed).unwrap()).unwrap();
        SignedExecutionFrame::decode_and_verify(wire.as_bytes(), binding, 50).unwrap()
    }

    fn base(cas: &lillux::CasStore) -> (String, ProjectSnapshot) {
        let policy = ProjectSnapshotPolicy::new(
            crate::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            Default::default(),
        )
        .unwrap();
        let tree = ProjectTree {
            files: Default::default(),
        };
        let snapshot = ProjectSnapshot {
            project_tree_hash: cas.store_object(&tree.to_value()).unwrap(),
            effective_policy_hash: cas.store_object(&policy.to_value()).unwrap(),
            parent_hashes: vec![],
            created_at: "2026-09-19T00:00:00Z".into(),
            message: None,
            source: "external-export-test".into(),
        };
        (cas.store_object(&snapshot.to_value()).unwrap(), snapshot)
    }

    #[test]
    fn external_export_imports_existing_project_contract_without_publication() {
        let root = tempfile::tempdir().unwrap();
        let db = crate::StateDb::open(root.path(), Arc::new(crate::TrustStore::new())).unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let cas = authority.cas_store().unwrap();
        let (base_hash, mut candidate) = base(&cas);
        let (mut binding, owner, supervisor) = super::super::tests::binding();
        binding.base_snapshot_hash = base_hash.clone();
        candidate.parent_hashes = vec![base_hash];
        let candidate_bytes = lillux::canonical_json(&candidate.to_value()).unwrap();
        let candidate_hash = lillux::sha256_hex(candidate_bytes.as_bytes());
        let completion = "9".repeat(64);
        let quiesce = authenticated(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            ExecutionChannelPayload::Quiesce {
                completion_request_digest: completion.clone(),
            },
        );
        let mut assembler =
            CandidateExportAssembler::new(&authority, &guard, binding.clone(), &quiesce).unwrap();
        let data = authenticated(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            ExecutionChannelPayload::ExportObjectChunk {
                content_kind: ExportContentKind::Object,
                object_hash: candidate_hash.clone(),
                offset: 0,
                bytes_base64: STANDARD.encode(candidate_bytes),
                final_chunk: true,
            },
        );
        assert!(assembler.accept(&data).unwrap().is_none());
        let evidence = NativeWriterExclusionObservation {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            base_snapshot_hash: binding.base_snapshot_hash.clone(),
            completion_request_digest: completion.clone(),
            mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
            exit: NativeNamespaceExit::Signal(9),
        };
        let evidence_bytes =
            lillux::canonical_json(&serde_json::to_value(evidence).unwrap()).unwrap();
        let evidence_hash = lillux::sha256_hex(evidence_bytes.as_bytes());
        let data = authenticated(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            ExecutionChannelPayload::ExportObjectChunk {
                content_kind: ExportContentKind::Blob,
                object_hash: evidence_hash.clone(),
                offset: 0,
                bytes_base64: STANDARD.encode(evidence_bytes),
                final_chunk: true,
            },
        );
        assert!(assembler.accept(&data).unwrap().is_none());
        let sealed = authenticated(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: candidate_hash.clone(),
                completion_request_digest: completion,
                writer_exclusion_evidence_hash: evidence_hash,
            },
        );
        let imported = assembler.accept(&sealed).unwrap().unwrap();
        assert_eq!(imported.snapshot_hash(), candidate_hash);
        imported
            .validate_retention(&authority, &guard, &binding)
            .unwrap();
        let mut other_binding = binding.clone();
        other_binding.channel_nonce = "8".repeat(64);
        assert!(
            imported
                .validate_retention(&authority, &guard, &other_binding)
                .is_err()
        );
        assert!(assembler.accept(&sealed).is_err());
        let roots = crate::gc::AdditionalCasRoots {
            object_hashes: vec![imported.snapshot_hash().to_owned()],
            blob_hashes: vec![imported.claimed_writer_exclusion_evidence_hash().to_owned()],
        };
        let unused = cas.store_blob(b"unretained-transfer-fragment").unwrap();
        drop(assembler);
        drop(guard);
        // Exercise actual object traversal and separate blob-root retention.
        // RuntimeDb/StateStore must persist these roots before this release;
        // this unit test is not a substitute for installed-owner acceptance.
        crate::gc::run_gc_with_additional_roots(
            root.path(),
            &crate::TrustStore::new(),
            None,
            &crate::gc::GcParams::default(),
            &roots,
        )
        .unwrap();
        assert!(cas.open_blob(&unused).unwrap().is_none());
        drop(db);
        let reopened =
            crate::StateDb::open(root.path(), Arc::new(crate::TrustStore::new())).unwrap();
        let reopened_authority = reopened.pinned_authority().unwrap();
        let reopened_guard = reopened_authority.acquire_shared_guard().unwrap();
        imported
            .validate_retention(&reopened_authority, &reopened_guard, &binding)
            .unwrap();
    }

    #[test]
    fn external_export_rejects_authority_objects_and_stays_failed() {
        let root = tempfile::tempdir().unwrap();
        let db = crate::StateDb::open(root.path(), Arc::new(crate::TrustStore::new())).unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let (base_hash, _) = base(&authority.cas_store().unwrap());
        let (mut binding, owner, supervisor) = super::super::tests::binding();
        binding.base_snapshot_hash = base_hash;
        let quiesce = authenticated(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            ExecutionChannelPayload::Quiesce {
                completion_request_digest: "9".repeat(64),
            },
        );
        let mut assembler =
            CandidateExportAssembler::new(&authority, &guard, binding.clone(), &quiesce).unwrap();
        let bytes = br#"{"kind":"thread_snapshot"}"#;
        let data = authenticated(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            ExecutionChannelPayload::ExportObjectChunk {
                content_kind: ExportContentKind::Object,
                object_hash: lillux::sha256_hex(bytes),
                offset: 0,
                bytes_base64: STANDARD.encode(bytes),
                final_chunk: true,
            },
        );
        assert!(assembler.accept(&data).is_err());
        assert!(assembler.accept(&data).is_err());
    }
}

/// Finite assembler underneath the durable frame-application owner. Only
/// newly claimed frames may be passed here. If its owning process dies during
/// an application, recovery must reconcile/quarantine the exact obligation;
/// constructing a fresh assembler is not permission to replay claimed input.
pub struct CandidateExportAssembler<'a> {
    authority: &'a PinnedStateAuthority,
    guard: &'a CasMutationGuard,
    binding: ExecutionChannelBinding,
    completion_request_digest: String,
    partial: Option<PartialMember>,
    received: BTreeSet<(ExportContentKind, String)>,
    bytes: u64,
    sealed: bool,
    failed: bool,
}

pub struct ImportedCandidateContent {
    closure: VerifiedProjectSnapshotClosure,
    channel_binding_digest: String,
    completion_request_digest: String,
    /// Authenticated supervisor claim; not yet independently qualified.
    claimed_writer_exclusion_evidence_hash: String,
}

impl ImportedCandidateContent {
    pub fn snapshot_hash(&self) -> &str {
        self.closure.snapshot_hash()
    }
    pub fn channel_binding_digest(&self) -> &str {
        &self.channel_binding_digest
    }
    pub fn completion_request_digest(&self) -> &str {
        &self.completion_request_digest
    }
    pub fn claimed_writer_exclusion_evidence_hash(&self) -> &str {
        &self.claimed_writer_exclusion_evidence_hash
    }

    /// Revalidate under the receiving store's still-held CAS guard before
    /// creating durable roots. This value is private-constructed by assembly;
    /// a peer's ExportSealed frame alone cannot create retained candidate state.
    pub fn validate_retention(
        &self,
        authority: &PinnedStateAuthority,
        guard: &CasMutationGuard,
        binding: &ExecutionChannelBinding,
    ) -> Result<()> {
        authority.ensure_guard(guard)?;
        ensure!(
            self.channel_binding_digest == binding.digest()?,
            "import changed channel"
        );
        let cas = authority.cas_store()?;
        let base = VerifiedProjectSnapshotClosure::load(&cas, &binding.base_snapshot_hash)?;
        let candidate = VerifiedProjectSnapshotClosure::load(&cas, self.snapshot_hash())?;
        ensure!(
            candidate.snapshot().parent_hashes == [binding.base_snapshot_hash.clone()]
                && candidate.snapshot().effective_policy_hash
                    == base.snapshot().effective_policy_hash,
            "import changed base policy"
        );
        let (file, size) = cas
            .open_blob(self.claimed_writer_exclusion_evidence_hash())?
            .context("import lost writer observation")?;
        ensure!(size <= 8192, "import observation exceeds bound");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(file, 8193), &mut bytes)?;
        ensure!(
            bytes.len() <= 8192
                && lillux::sha256_hex(&bytes) == self.claimed_writer_exclusion_evidence_hash,
            "import observation bytes changed"
        );
        let observation: NativeWriterExclusionObservation = serde_json::from_slice(&bytes)?;
        observation.validate(binding, self.completion_request_digest())?;
        ensure!(
            lillux::canonical_json(&serde_json::to_value(observation)?)?.as_bytes() == bytes,
            "import observation is noncanonical"
        );
        Ok(())
    }
}

impl<'a> CandidateExportAssembler<'a> {
    pub fn new(
        authority: &'a PinnedStateAuthority,
        guard: &'a CasMutationGuard,
        binding: ExecutionChannelBinding,
        quiesce: &AuthenticatedExecutionFrame,
    ) -> Result<Self> {
        authority.ensure_guard(guard)?;
        let completion_request_digest = match &quiesce.frame().payload {
            ExecutionChannelPayload::Quiesce {
                completion_request_digest,
            } if quiesce.frame().direction == ChannelDirection::OwnerToSupervisor
                && quiesce.frame().binding_digest == binding.digest()? =>
            {
                completion_request_digest.clone()
            }
            _ => bail!("candidate export requires the exact authenticated quiesce command"),
        };
        // Verify the existing base before accepting any foreign bytes.
        VerifiedProjectSnapshotClosure::load(&authority.cas_store()?, &binding.base_snapshot_hash)?;
        Ok(Self {
            authority,
            guard,
            binding,
            completion_request_digest,
            partial: None,
            received: BTreeSet::new(),
            bytes: 0,
            sealed: false,
            failed: false,
        })
    }

    pub fn accept(
        &mut self,
        frame: &AuthenticatedExecutionFrame,
    ) -> Result<Option<ImportedCandidateContent>> {
        ensure!(!self.failed && !self.sealed, "candidate export is closed");
        let result = self.accept_inner(frame);
        if result.is_err() {
            self.failed = true;
            self.partial = None;
        }
        result
    }

    fn accept_inner(
        &mut self,
        frame: &AuthenticatedExecutionFrame,
    ) -> Result<Option<ImportedCandidateContent>> {
        self.authority.ensure_guard(self.guard)?;
        ensure!(
            frame.frame().binding_digest == self.binding.digest()?
                && frame.frame().direction == ChannelDirection::SupervisorToOwner,
            "candidate export changed its authenticated channel"
        );
        match &frame.frame().payload {
            ExecutionChannelPayload::ExportObjectChunk {
                content_kind,
                object_hash,
                offset,
                bytes_base64,
                final_chunk,
            } => {
                let bytes = chunk(bytes_base64, *final_chunk)?;
                self.bytes = self
                    .bytes
                    .checked_add(bytes.len() as u64)
                    .context("candidate export byte overflow")?;
                ensure!(
                    self.bytes <= self.binding.max_bytes,
                    "candidate export exceeds byte budget"
                );
                ensure!(
                    !self
                        .received
                        .contains(&(*content_kind, object_hash.clone())),
                    "candidate export repeated a completed member"
                );
                if self.partial.is_none() {
                    ensure!(
                        *offset == 0 && self.received.len() < MAX_TRANSFER_MEMBERS,
                        "candidate export starts at nonzero offset or exceeds member budget"
                    );
                    self.partial = Some(PartialMember {
                        kind: *content_kind,
                        hash: object_hash.clone(),
                        bytes: Vec::new(),
                    });
                }
                let member = self
                    .partial
                    .as_mut()
                    .context("candidate export member missing")?;
                ensure!(
                    member.kind == *content_kind
                        && member.hash == *object_hash
                        && *offset == member.bytes.len() as u64,
                    "candidate export interleaved, repeated or skipped bytes"
                );
                ensure!(
                    offset
                        .checked_add(bytes.len() as u64)
                        .is_some_and(|n| n <= MAX_SINGLE_MEMBER_BYTES),
                    "candidate export member exceeds memory bound"
                );
                member.bytes.extend_from_slice(&bytes);
                if *final_chunk {
                    let member = self
                        .partial
                        .take()
                        .context("candidate export lost member")?;
                    ensure!(
                        lillux::sha256_hex(&member.bytes) == member.hash,
                        "candidate export content hash mismatch"
                    );
                    let cas = self.authority.cas_store()?;
                    let stored = match member.kind {
                        ExportContentKind::Blob => cas.store_blob(&member.bytes)?,
                        ExportContentKind::Object => {
                            let value: serde_json::Value = serde_json::from_slice(&member.bytes)?;
                            ensure!(
                                lillux::canonical_json(&value)?.as_bytes() == member.bytes,
                                "candidate export object is noncanonical"
                            );
                            match value.get("kind").and_then(serde_json::Value::as_str) {
                                Some("project_file") => {
                                    ProjectFile::from_value(&value)?;
                                }
                                Some("project_tree") => {
                                    ProjectTree::from_value(&value)?;
                                }
                                Some("project_snapshot_policy") => {
                                    ProjectSnapshotPolicy::from_value(&value)?;
                                }
                                Some("project_snapshot") => {
                                    ProjectSnapshot::from_value(&value)?;
                                }
                                _ => bail!(
                                    "candidate export cannot import non-project authority objects"
                                ),
                            }
                            cas.store_object(&value)?
                        }
                    };
                    ensure!(
                        stored == member.hash,
                        "candidate export CAS identity changed"
                    );
                    self.received.insert((member.kind, member.hash));
                }
                Ok(None)
            }
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash,
                completion_request_digest,
                writer_exclusion_evidence_hash,
            } => {
                ensure!(
                    self.partial.is_none(),
                    "candidate export sealed with incomplete member"
                );
                ensure!(
                    completion_request_digest == &self.completion_request_digest,
                    "candidate export changed its completion request"
                );
                let cas = self.authority.cas_store()?;
                let (evidence_file, evidence_size) = cas
                    .open_blob(writer_exclusion_evidence_hash)?
                    .context("candidate export lacks writer-exclusion observation")?;
                ensure!(
                    evidence_size <= 8192,
                    "writer-exclusion observation exceeds bound"
                );
                let mut evidence_bytes = Vec::new();
                std::io::Read::read_to_end(
                    &mut std::io::Read::take(evidence_file, 8193),
                    &mut evidence_bytes,
                )?;
                ensure!(
                    evidence_bytes.len() <= 8192
                        && lillux::sha256_hex(&evidence_bytes) == *writer_exclusion_evidence_hash,
                    "writer-exclusion observation changed during read"
                );
                let observation: NativeWriterExclusionObservation =
                    serde_json::from_slice(&evidence_bytes)?;
                observation.validate(&self.binding, completion_request_digest)?;
                ensure!(
                    lillux::canonical_json(&serde_json::to_value(&observation)?)?.as_bytes()
                        == evidence_bytes,
                    "writer-exclusion observation is not canonical"
                );
                let base =
                    VerifiedProjectSnapshotClosure::load(&cas, &self.binding.base_snapshot_hash)?;
                let candidate =
                    VerifiedProjectSnapshotClosure::load(&cas, candidate_snapshot_hash)?;
                ensure!(
                    candidate.snapshot().parent_hashes == [self.binding.base_snapshot_hash.clone()]
                        && candidate.snapshot().effective_policy_hash
                            == base.snapshot().effective_policy_hash,
                    "candidate export changed its base or qualification policy"
                );
                let mut allowed = BTreeSet::from([
                    (
                        ExportContentKind::Blob,
                        writer_exclusion_evidence_hash.clone(),
                    ),
                    (ExportContentKind::Object, candidate_snapshot_hash.clone()),
                    (
                        ExportContentKind::Object,
                        candidate.snapshot().project_tree_hash.clone(),
                    ),
                    (
                        ExportContentKind::Object,
                        candidate.snapshot().effective_policy_hash.clone(),
                    ),
                ]);
                for (path, file) in candidate.tree().files() {
                    allowed.insert((
                        ExportContentKind::Object,
                        candidate.tree().tree().files[path].clone(),
                    ));
                    allowed.insert((ExportContentKind::Blob, file.blob_hash.clone()));
                }
                ensure!(
                    self.received.is_subset(&allowed),
                    "candidate export includes content outside its declared closure"
                );
                self.sealed = true;
                Ok(Some(ImportedCandidateContent {
                    closure: candidate,
                    channel_binding_digest: self.binding.digest()?,
                    completion_request_digest: completion_request_digest.clone(),
                    claimed_writer_exclusion_evidence_hash: writer_exclusion_evidence_hash.clone(),
                }))
            }
            _ => bail!("candidate content assembler received a non-export frame"),
        }
    }
}
