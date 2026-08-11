//! Daemon-owned acquisition and publication of admitted executable source.
//!
//! Engine code defines the authority-neutral capture/testimony contracts.
//! This layer selects a typed project or bundle authority, applies the shared
//! durable capture floor, publishes immutable blobs and objects under a
//! staged root, and inserts only the bounded derived projection.

use std::path::{Path, PathBuf};

use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace};
use ryeos_engine::launch::plan_builder::ExecutorSourcePolicyProjection;
use ryeos_engine::project_content::{AuthoritativeProjectContent, ProjectContentEntry};
use ryeos_engine::source_closure::{
    AuthoritativeSourceContent, CapturedSourceCandidate, SourceClosureProof, SourceClosureStore,
    SourceRootRequest, SourceRootSelection,
};
use ryeos_state::PendingCasPublication;

use crate::state::AppState;

pub struct AdmittedSourceClosure {
    proof: SourceClosureProof,
    store: RetainedSourceClosureStore,
    publication: Option<PendingCasPublication>,
    binding: ryeos_state::objects::EffectiveSourceBinding,
    manifest: ryeos_state::objects::SourceClosureManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceClosureValidationPreview {
    pub owner_ref: String,
    pub expected_digest: Option<String>,
    pub observed_digest: String,
    pub binding_digest: Option<String>,
    pub file_count: usize,
    pub total_bytes: u64,
    pub ready_for_admission: bool,
    pub status: String,
}

impl AdmittedSourceClosure {
    pub fn finalization_evidence(&self) -> (&SourceClosureProof, &dyn SourceClosureStore) {
        (&self.proof, &self.store)
    }

    pub fn binding(&self) -> &ryeos_state::objects::EffectiveSourceBinding {
        &self.binding
    }

    pub fn manifest(&self) -> &ryeos_state::objects::SourceClosureManifest {
        &self.manifest
    }

    pub fn into_publication(mut self) -> Option<PendingCasPublication> {
        self.publication.take()
    }
}

pub fn admit_source_closure(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    project: Option<(&Path, &dyn AuthoritativeProjectContent, String)>,
    executor_policy: Option<&ExecutorSourcePolicyProjection>,
) -> anyhow::Result<Option<AdmittedSourceClosure>> {
    let mut publication = None;
    let mut admitted = admit_source_closure_in_publication(
        state,
        engine,
        kind,
        resolution,
        roots,
        project,
        executor_policy,
        &mut publication,
        None,
    )?;
    if let Some(admitted) = admitted.as_mut() {
        admitted.publication = publication;
    }
    Ok(admitted)
}

pub fn admit_source_closure_in_publication(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    project: Option<(&Path, &dyn AuthoritativeProjectContent, String)>,
    executor_policy: Option<&ExecutorSourcePolicyProjection>,
    publication: &mut Option<PendingCasPublication>,
    mut preview: Option<&mut Option<SourceClosureValidationPreview>>,
) -> anyhow::Result<Option<AdmittedSourceClosure>> {
    if resolution
        .composed
        .derived
        .contains_key(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
    {
        anyhow::bail!(
            "effective program attempted to pre-populate reserved source closure authority"
        );
    }
    let kind_schema = engine
        .kinds
        .get(kind)
        .ok_or_else(|| anyhow::anyhow!("source closure kind `{kind}` is not registered"))?;
    let contract = kind_schema
        .execution
        .as_ref()
        .and_then(|execution| execution.source_closure.as_ref());
    let Some(contract) = contract else {
        if resolution.composed.composed.get("source").is_some() {
            anyhow::bail!(
                "item declares `source` but its signed kind has no source closure authority"
            );
        }
        return Ok(None);
    };
    if matches!(
        &contract.location,
        ryeos_engine::kind_registry::SourceClosureLocationDecl::ItemNamespace
    ) && executor_policy.is_none()
    {
        return Ok(None);
    }
    let kind_evidence = engine.kinds.schema_evidence(kind).ok_or_else(|| {
        anyhow::anyhow!("source closure kind `{kind}` has no retained signed schema evidence")
    })?;
    let registered = roots.authoritative_root(
        &resolution.root.source_root,
        resolution.root.source_space,
        None,
    )?;
    let content_root = registered.content_root.as_deref().ok_or_else(|| {
        anyhow::anyhow!("source closure typed root has no registered content authority")
    })?;

    let (source, requests, root_entry, root_item_extension, worker_declaration): (
        Box<dyn AuthoritativeSourceContent + '_>,
        Vec<SourceRootRequest>,
        String,
        String,
        Option<ryeos_engine::source_closure::WorkerSourceDeclaration>,
    ) = match &contract.location {
        ryeos_engine::kind_registry::SourceClosureLocationDecl::ItemNamespace => {
            let policy = executor_policy.ok_or_else(|| {
                anyhow::anyhow!("direct source-owning item has no verified executor source policy")
            })?;
            if !matches!(
                policy.policy.location,
                ryeos_engine::source_closure::ExecutorSourceLocation::ItemNamespace
            ) {
                anyhow::bail!("executor source policy exceeds the signed kind location ceiling");
            }
            let source: Box<dyn AuthoritativeSourceContent> =
                match (resolution.root.source_space, project) {
                    (ItemSpace::Project, Some((project_root, content, identity))) => {
                        if project_root != content_root {
                            anyhow::bail!(
                                "project source authority root differs from typed resolution root"
                            );
                        }
                        Box::new(ProjectSourceContent { content, identity })
                    }
                    (ItemSpace::Project, None) => Box::new(DirectorySourceContent::new(
                        content_root,
                        format!(
                            "live:{}:{}",
                            root_identity_key(&resolution.root.source_root),
                            resolution.root.source_content_digest
                        ),
                        state.ignore_matcher.as_ref(),
                    )?),
                    (_, _) => Box::new(DirectorySourceContent::new(
                        content_root,
                        root_identity_key(&resolution.root.source_root),
                        state.ignore_matcher.as_ref(),
                    )?),
                };
            let (request, entry) = item_namespace_source_request(
                source.as_ref(),
                kind,
                kind_schema,
                &resolution.root.resolved_ref,
                &resolution.root.source_content_digest,
                contract.max_file_bytes,
            )?;
            let extension = Path::new(&entry)
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| format!(".{extension}"))
                .ok_or_else(|| anyhow::anyhow!("source owner has no kind-owned extension"))?;
            (source, vec![request], entry, extension, None)
        }
        ryeos_engine::kind_registry::SourceClosureLocationDecl::OwnerRelativeSource { .. } => {
            if executor_policy.is_some() {
                anyhow::bail!("self-launching source cannot borrow an executor source policy");
            }
            let declaration = if preview.is_some() {
                ryeos_engine::source_closure::WorkerSourceDeclaration::from_composed_for_static_preview(
                    &resolution.composed.composed,
                    Some(contract),
                )?
            } else {
                ryeos_engine::source_closure::WorkerSourceDeclaration::from_composed(
                    &resolution.composed.composed,
                    Some(contract),
                )?
            }
            .ok_or_else(|| anyhow::anyhow!("source declaration is absent"))?;
            let bare_id = resolution
                .root
                .resolved_ref
                .split_once(':')
                .map(|(_, id)| id)
                .ok_or_else(|| anyhow::anyhow!("source owner ref is not canonical"))?;
            let owner_namespace = bare_id
                .split('/')
                .next()
                .filter(|part| !part.is_empty())
                .ok_or_else(|| anyhow::anyhow!("source owner namespace is absent"))?;
            let prefix = Path::new(ryeos_engine::AI_DIR)
                .join(&kind_schema.directory)
                .join(owner_namespace)
                .join(&declaration.root);
            let source: Box<dyn AuthoritativeSourceContent> =
                match (resolution.root.source_space, project) {
                    (ItemSpace::Project, Some((project_root, content, identity))) => {
                        if project_root != content_root {
                            anyhow::bail!(
                                "project source authority root differs from typed resolution root"
                            );
                        }
                        Box::new(ProjectSourceContent { content, identity })
                    }
                    (ItemSpace::Project, None) => Box::new(DirectorySourceContent::new(
                        content_root,
                        format!(
                            "live:{}:{}",
                            root_identity_key(&resolution.root.source_root),
                            resolution.root.source_content_digest
                        ),
                        state.ignore_matcher.as_ref(),
                    )?),
                    (_, _) => Box::new(DirectorySourceContent::new(
                        content_root,
                        root_identity_key(&resolution.root.source_root),
                        state.ignore_matcher.as_ref(),
                    )?),
                };
            let descriptor_path = canonical_item_source_path(
                source.as_ref(),
                kind,
                kind_schema,
                &resolution.root.resolved_ref,
                &resolution.root.source_content_digest,
                contract.max_file_bytes,
            )?;
            let descriptor_extension = descriptor_path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| format!(".{extension}"))
                .ok_or_else(|| anyhow::anyhow!("source owner has no kind-owned extension"))?;
            let entry = declaration.entry.clone();
            (
                source,
                vec![SourceRootRequest {
                    id: "source".to_owned(),
                    selection: SourceRootSelection::Tree { prefix },
                }],
                entry,
                descriptor_extension,
                Some(declaration),
            )
        }
    };

    let candidate = ryeos_engine::source_closure::capture_source_candidate(
        source.as_ref(),
        &requests,
        contract,
    )?;
    require_manifest_entry(&candidate, &root_entry)?;
    if let Some(output) = preview.as_deref_mut()
        && let Some(declaration) = worker_declaration.as_ref()
    {
        let observed_digest = candidate.manifest.digest()?;
        let (ready_for_admission, status) = if !lillux::valid_hash(&declaration.digest) {
            (false, "pending")
        } else if declaration.digest == observed_digest {
            (true, "matched")
        } else {
            (false, "mismatched")
        };
        *output = Some(SourceClosureValidationPreview {
            owner_ref: resolution.root.resolved_ref.to_string(),
            expected_digest: Some(declaration.digest.clone()),
            observed_digest,
            binding_digest: None,
            file_count: candidate.manifest.totals.file_count,
            total_bytes: candidate.manifest.totals.total_bytes,
            ready_for_admission,
            status: status.to_owned(),
        });
        return Ok(None);
    }
    let testimony = match contract.testimony {
        ryeos_engine::kind_registry::SourceClosureTestimonyDecl::OwnerSignedFiles => {
            verify_owner_signed_files(
                &candidate,
                &root_entry,
                kind_schema,
                resolution,
                &engine.trust_store,
            )?
        }
        ryeos_engine::kind_registry::SourceClosureTestimonyDecl::OwnerSignedDigest => {
            let declaration = worker_declaration.as_ref().ok_or_else(|| {
                anyhow::anyhow!("signed-digest source has no atomic source declaration")
            })?;
            let observed = candidate.manifest.digest()?;
            if declaration.digest != observed {
                anyhow::bail!(
                    "signed source digest expected {}, observed {observed}",
                    declaration.digest
                );
            }
            ryeos_state::objects::SourceTestimonyProof::OwnerSignedDigest {
                expected_manifest_hash: declaration.digest.clone(),
            }
        }
    };

    let content_manifest_hash = candidate.manifest.digest()?;
    let owner = source_owner_identity(resolution)?;
    let normalized_declaration = serde_json::to_value(contract)?;
    let root_format = kind_schema
        .spec_for(&root_item_extension)
        .ok_or_else(|| anyhow::anyhow!("source owner extension is absent from its signed kind"))?;
    let execution_policy = if let Some(policy) = executor_policy {
        ryeos_state::objects::SourceExecutionPolicyIdentity::Executor {
            declarer_ref: policy.declarer.canonical_ref.clone(),
            signer_fingerprint: policy
                .declarer
                .signer_fingerprint
                .clone()
                .ok_or_else(|| anyhow::anyhow!("source policy declarer has no signer"))?,
            source_content_digest: policy.declarer.content_hash.clone(),
            raw_content_digest: policy.declarer.raw_content_digest.clone(),
            policy_digest: policy.policy.digest()?,
            chain_digest: policy.chain_digest.clone(),
        }
    } else {
        ryeos_state::objects::SourceExecutionPolicyIdentity::Worker {
            source_declaration_digest: worker_declaration
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("worker source declaration is absent"))?
                .identity_digest()?,
        }
    };
    let logical_binding = if let Some(policy) = executor_policy {
        ryeos_state::objects::SourceLogicalBinding::Tool {
            loader_roots: policy.policy.load_roots.clone(),
            root_entry: root_entry.clone(),
        }
    } else {
        ryeos_state::objects::SourceLogicalBinding::Worker {
            root: worker_declaration
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("worker source declaration is absent"))?
                .root
                .clone(),
            entry: root_entry.clone(),
        }
    };
    let binding = ryeos_state::objects::EffectiveSourceBinding {
        schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
        kind: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_KIND.to_owned(),
        owner,
        kind_ceiling: ryeos_state::objects::SignedKindSourceCeiling {
            schema_ref: kind_evidence.canonical_ref.clone(),
            source_content_digest: kind_evidence.source_content_digest.clone(),
            raw_content_digest: kind_evidence.raw_content_digest.clone(),
            signer_fingerprint: kind_evidence.signer_fingerprint.clone(),
            signature_header: kind_evidence.signature_header.clone(),
            schema_body: kind_evidence.body.clone(),
            schema_document: kind_evidence.document.clone(),
            normalized_declaration,
            root_kind_format: serde_json::json!({
                "extension": root_format.ext,
                "parser": root_format.parser,
                "signature": root_format.signature,
            }),
            root_signature_envelope: serde_json::to_value(&root_format.signature)?,
        },
        content_manifest_hash: content_manifest_hash.clone(),
        testimony,
        execution_policy,
        logical_binding,
    };
    binding.validate()?;
    binding.validate_content_manifest(&candidate.manifest)?;

    if let Some(preview) = preview {
        let expected_digest = match &binding.testimony {
            ryeos_state::objects::SourceTestimonyProof::OwnerSignedDigest {
                expected_manifest_hash,
            } => Some(expected_manifest_hash.clone()),
            ryeos_state::objects::SourceTestimonyProof::OwnerSignedFiles { .. } => None,
        };
        *preview = Some(SourceClosureValidationPreview {
            owner_ref: binding.owner.canonical_ref.clone(),
            expected_digest,
            observed_digest: content_manifest_hash,
            binding_digest: Some(binding.digest()?),
            file_count: candidate.manifest.totals.file_count,
            total_bytes: candidate.manifest.totals.total_bytes,
            ready_for_admission: true,
            status: "matched".to_owned(),
        });
        return Ok(None);
    }

    if publication.is_none() {
        let authority = state
            .state_store
            .with_state_db(|db| db.pinned_authority())?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        let staged = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(&guard, "launch-realization")?;
        drop(guard);
        *publication = Some(PendingCasPublication::new(authority, staged));
    }
    let authority = publication
        .as_ref()
        .expect("source admission initialized its publication")
        .authority()
        .try_clone()?;
    let proof_authority = authority.try_clone()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire CAS write permit: {error}"))?;
    let cas = authority.cas_store()?;
    let staged = publication
        .as_mut()
        .expect("source admission initialized its publication")
        .staged_roots_mut();
    for blob in &candidate.blobs {
        let outcome = cas.put_blob(&blob.bytes)?;
        if outcome.hash != blob.blob_hash {
            anyhow::bail!("stored source blob contradicts its captured identity");
        }
        staged.protect_blob_hash_admitted(&guard, &outcome.hash)?;
    }
    let stored_manifest_hash =
        staged.store_object_admitted(&guard, &cas, &candidate.manifest.to_value()?)?;
    if stored_manifest_hash != content_manifest_hash {
        anyhow::bail!("stored source manifest contradicts its captured identity");
    }
    let binding_hash = staged.store_object_admitted(&guard, &cas, &binding.to_value()?)?;
    let projection = ryeos_state::objects::EffectiveSourceClosureProjection {
        schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
        binding_hash,
        content_manifest_hash,
        owner_key: binding.owner_key()?,
        file_count: candidate.manifest.totals.file_count,
        total_bytes: candidate.manifest.totals.total_bytes,
    };
    resolution.composed.derived.insert(
        ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
        projection.to_value()?,
    );
    let store = RetainedSourceClosureStore {
        authority: proof_authority,
    };
    let proof = ryeos_engine::source_closure::prove_source_closure(projection, &store)?;
    drop(_permit);
    drop(guard);
    Ok(Some(AdmittedSourceClosure {
        proof,
        store,
        publication: None,
        binding,
        manifest: candidate.manifest,
    }))
}

#[allow(clippy::too_many_arguments)]
pub fn preview_source_closure(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    project: Option<(&Path, &dyn AuthoritativeProjectContent, String)>,
    executor_policy: Option<&ExecutorSourcePolicyProjection>,
) -> anyhow::Result<Option<SourceClosureValidationPreview>> {
    let mut resolution = resolution.clone();
    let mut publication = None;
    let mut preview = None;
    let admitted = admit_source_closure_in_publication(
        state,
        engine,
        kind,
        &mut resolution,
        roots,
        project,
        executor_policy,
        &mut publication,
        Some(&mut preview),
    )?;
    if admitted.is_some() || publication.is_some() {
        anyhow::bail!("static source validation attempted to mint launch authority");
    }
    Ok(preview)
}

pub fn recover_source_closure(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<Option<AdmittedSourceClosure>> {
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
    else {
        return Ok(None);
    };
    let projection = ryeos_state::objects::EffectiveSourceClosureProjection::from_value(value)?;
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let cas = authority.cas_store()?;
    let binding_value = cas
        .get_object(&projection.binding_hash)?
        .ok_or_else(|| anyhow::anyhow!("retained source binding is absent"))?;
    let binding = ryeos_state::objects::EffectiveSourceBinding::from_value(&binding_value)?;
    let manifest_value = cas
        .get_object(&projection.content_manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("retained source manifest is absent"))?;
    let manifest = ryeos_state::objects::SourceClosureManifest::from_value(&manifest_value)?;
    if binding.content_manifest_hash != projection.content_manifest_hash
        || manifest.digest()? != projection.content_manifest_hash
        || binding.digest()? != projection.binding_hash
        || binding.owner_key()? != projection.owner_key
        || manifest.totals.file_count != projection.file_count
        || manifest.totals.total_bytes != projection.total_bytes
    {
        anyhow::bail!("retained source closure contradicts its effective projection");
    }
    binding.validate_content_manifest(&manifest)?;
    if source_owner_identity(resolution)? != binding.owner {
        anyhow::bail!("retained source owner contradicts its admitted resolution");
    }
    let schema =
        require_current_kind_testimony(&engine.node_trust_store, &engine.trust_store, &binding)?;
    require_current_source_policy(
        &engine.node_trust_store,
        &engine.trust_store,
        resolution,
        &binding,
        &schema,
    )?;
    let store = RetainedSourceClosureStore { authority };
    let proof = ryeos_engine::source_closure::prove_source_closure(projection, &store)?;
    Ok(Some(AdmittedSourceClosure {
        proof,
        store,
        publication: None,
        binding,
        manifest,
    }))
}

fn require_current_kind_testimony(
    node_trust: &ryeos_engine::trust::TrustStore,
    item_trust: &ryeos_engine::trust::TrustStore,
    binding: &ryeos_state::objects::EffectiveSourceBinding,
) -> anyhow::Result<ryeos_engine::kind_registry::KindSchema> {
    if item_trust.get(&binding.owner.signer_fingerprint).is_none()
        && node_trust.get(&binding.owner.signer_fingerprint).is_none()
    {
        anyhow::bail!("retained source testimony signer is no longer trusted");
    }
    let evidence = ryeos_engine::kind_registry::KindSchemaEvidence {
        canonical_ref: binding.kind_ceiling.schema_ref.clone(),
        source_content_digest: binding.kind_ceiling.source_content_digest.clone(),
        raw_content_digest: binding.kind_ceiling.raw_content_digest.clone(),
        signer_fingerprint: binding.kind_ceiling.signer_fingerprint.clone(),
        signature_header: binding.kind_ceiling.signature_header.clone(),
        body: binding.kind_ceiling.schema_body.clone(),
        document: binding.kind_ceiling.schema_document.clone(),
    };
    let schema =
        ryeos_engine::kind_registry::verify_retained_kind_schema_evidence(&evidence, node_trust)?;
    let declaration = schema
        .execution
        .as_ref()
        .and_then(|execution| execution.source_closure.as_ref())
        .ok_or_else(|| anyhow::anyhow!("retained kind no longer proves source authority"))?;
    if serde_json::to_value(declaration)? != binding.kind_ceiling.normalized_declaration {
        anyhow::bail!("retained source ceiling contradicts its signed kind declaration");
    }
    let extension = binding
        .kind_ceiling
        .root_kind_format
        .get("extension")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("retained source format has no extension"))?;
    let format = schema.spec_for(extension).ok_or_else(|| {
        anyhow::anyhow!("retained source format extension is absent from its kind")
    })?;
    let expected_format = serde_json::json!({
        "extension": format.ext,
        "parser": format.parser,
        "signature": format.signature,
    });
    if expected_format != binding.kind_ceiling.root_kind_format
        || serde_json::to_value(&format.signature)? != binding.kind_ceiling.root_signature_envelope
    {
        anyhow::bail!("retained source format contradicts its signed kind");
    }
    Ok(schema)
}

fn require_current_source_policy(
    node_trust: &ryeos_engine::trust::TrustStore,
    item_trust: &ryeos_engine::trust::TrustStore,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    binding: &ryeos_state::objects::EffectiveSourceBinding,
    schema: &ryeos_engine::kind_registry::KindSchema,
) -> anyhow::Result<()> {
    match &binding.execution_policy {
        ryeos_state::objects::SourceExecutionPolicyIdentity::Executor {
            signer_fingerprint,
            ..
        } => {
            if item_trust.get(signer_fingerprint).is_none()
                && node_trust.get(signer_fingerprint).is_none()
            {
                anyhow::bail!("retained source policy signer is no longer trusted");
            }
        }
        ryeos_state::objects::SourceExecutionPolicyIdentity::Worker {
            source_declaration_digest,
        } => {
            let contract = schema
                .execution
                .as_ref()
                .and_then(|execution| execution.source_closure.as_ref())
                .ok_or_else(|| anyhow::anyhow!("retained worker kind has no source contract"))?;
            let declaration = ryeos_engine::source_closure::WorkerSourceDeclaration::from_composed(
                &resolution.composed.composed,
                Some(contract),
            )?
            .ok_or_else(|| anyhow::anyhow!("retained worker source declaration is absent"))?;
            if declaration.identity_digest()? != *source_declaration_digest
                || declaration.digest != binding.content_manifest_hash
            {
                anyhow::bail!("retained worker source policy contradicts its signed declaration");
            }
        }
    }
    Ok(())
}

fn verify_owner_signed_files(
    candidate: &CapturedSourceCandidate,
    root_entry: &str,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> anyhow::Result<ryeos_state::objects::SourceTestimonyProof> {
    let owner = resolution
        .root
        .signer_fingerprint
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!("source-owning project item has no verified signer fingerprint")
        })?;
    let mut testimony = Vec::with_capacity(candidate.manifest.entries.len());
    let mut root_matched = false;
    for entry in &candidate.manifest.entries {
        let blob = candidate
            .blobs
            .iter()
            .find(|blob| blob.blob_hash == entry.blob_hash)
            .ok_or_else(|| anyhow::anyhow!("source testimony blob is absent"))?;
        let content = std::str::from_utf8(&blob.bytes)
            .map_err(|_| anyhow::anyhow!("signed source file is not UTF-8"))?;
        let extension = Path::new(&entry.path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!(".{extension}"))
            .ok_or_else(|| anyhow::anyhow!("source file has no kind-owned extension"))?;
        let format = kind_schema.spec_for(&extension).ok_or_else(|| {
            anyhow::anyhow!("source file extension `{extension}` is not owned by the root kind")
        })?;
        let header =
            ryeos_engine::item_resolution::parse_signature_header(content, &format.signature)
                .ok_or_else(|| anyhow::anyhow!("source file `{}` is unsigned", entry.path))?;
        if header.signer_fingerprint != owner {
            anyhow::bail!(
                "source file `{}` is signed by a foreign principal",
                entry.path
            );
        }
        let (trust, signer) = ryeos_engine::trust::verify_item_signature(
            content,
            &header,
            &format.signature,
            trust_store,
        )?;
        if trust != ryeos_engine::contracts::TrustClass::Trusted
            || signer.as_ref().map(|signer| signer.0.as_str()) != Some(owner)
        {
            anyhow::bail!(
                "source file `{}` has no current owner testimony",
                entry.path
            );
        }
        if entry.path == root_entry {
            root_matched = true;
            if entry.blob_hash != resolution.root.source_content_digest {
                anyhow::bail!("source root entry differs from the resolved executable bytes");
            }
        }
        testimony.push(serde_json::json!({
            "path": &entry.path,
            "blob_hash": &entry.blob_hash,
            "signer": owner,
            "content_hash": header.content_hash,
        }));
    }
    if !root_matched {
        anyhow::bail!("source closure does not contain its resolved root entry");
    }
    let entries_digest = lillux::sha256_hex(
        lillux::canonical_json(&serde_json::Value::Array(testimony))?.as_bytes(),
    );
    Ok(
        ryeos_state::objects::SourceTestimonyProof::OwnerSignedFiles {
            signer_fingerprint: owner.to_owned(),
            file_count: candidate.manifest.entries.len(),
            entries_digest,
        },
    )
}

/// Resolve the root executable from the canonical item coordinate and the
/// signed kind's extension order. Host paths are deliberately absent: the
/// typed content root has already selected the authority, while the kind
/// schema defines the only filenames that may represent this canonical ref.
fn item_namespace_source_request(
    source: &dyn AuthoritativeSourceContent,
    expected_kind: &str,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    canonical_ref: &str,
    expected_source_digest: &str,
    max_file_bytes: u64,
) -> anyhow::Result<(SourceRootRequest, String)> {
    let selected = canonical_item_source_path(
        source,
        expected_kind,
        kind_schema,
        canonical_ref,
        expected_source_digest,
        max_file_bytes,
    )?;
    let (_, bare_id) = canonical_ref
        .split_once(':')
        .expect("canonical item lookup validated the ref");
    let kind_root = Path::new(ryeos_engine::AI_DIR).join(&kind_schema.directory);
    let components = Path::new(bare_id).components().collect::<Vec<_>>();
    if components.len() == 1 {
        let entry = selected
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("source item path is not UTF-8"))?
            .to_owned();
        return Ok((
            SourceRootRequest {
                id: "source".to_owned(),
                selection: SourceRootSelection::File { path: selected },
            },
            entry,
        ));
    }

    let prefix = kind_root.join(components[0].as_os_str());
    let entry = selected
        .strip_prefix(&prefix)
        .map_err(|_| anyhow::anyhow!("source item escaped its canonical namespace"))?
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("source item path is not UTF-8"))?
        .to_owned();
    Ok((
        SourceRootRequest {
            id: "source".to_owned(),
            selection: SourceRootSelection::Tree { prefix },
        },
        entry,
    ))
}

fn canonical_item_source_path(
    source: &dyn AuthoritativeSourceContent,
    expected_kind: &str,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    canonical_ref: &str,
    expected_source_digest: &str,
    max_file_bytes: u64,
) -> anyhow::Result<PathBuf> {
    let (kind, bare_id) = canonical_ref
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("source owner ref is not canonical"))?;
    if kind != expected_kind {
        anyhow::bail!("source owner ref contradicts its signed kind");
    }
    ryeos_state::objects::validate_canonical_project_relative_path(bare_id)?;
    let kind_root = Path::new(ryeos_engine::AI_DIR).join(&kind_schema.directory);

    let mut selected = None;
    for format in &kind_schema.extensions {
        let relative = kind_root.join(format!("{bare_id}{}", format.ext));
        let Some(bytes) = source.read_file(&relative, max_file_bytes)? else {
            continue;
        };
        if lillux::sha256_hex(&bytes) != expected_source_digest {
            anyhow::bail!(
                "canonical source coordinate contradicts the resolved executable identity"
            );
        }
        selected = Some(relative);
        break;
    }
    let selected = selected.ok_or_else(|| {
        anyhow::anyhow!("canonical source coordinate is absent from its typed content root")
    })?;
    Ok(selected)
}

fn require_manifest_entry(candidate: &CapturedSourceCandidate, entry: &str) -> anyhow::Result<()> {
    if !candidate
        .manifest
        .entries
        .iter()
        .any(|file| file.root == "source" && file.path == entry)
    {
        anyhow::bail!("source closure entry `{entry}` is absent after capture policy filtering");
    }
    Ok(())
}

fn source_owner_identity(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<ryeos_state::objects::SourceOwnerIdentity> {
    let signer =
        resolution.root.signer_fingerprint.clone().ok_or_else(|| {
            anyhow::anyhow!("source-owning item has no verified signer fingerprint")
        })?;
    let (item_kind, logical_item_key) = resolution
        .root
        .resolved_ref
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("source owner ref is not canonical"))?;
    let (source_space, source_root) =
        match (resolution.root.source_space, &resolution.root.source_root) {
            (ItemSpace::Project, ItemSourceRoot::Project) => (
                ryeos_state::objects::SourceSpaceIdentity::Project,
                ryeos_state::objects::SourceRootIdentity::Project,
            ),
            (ItemSpace::Bundle, ItemSourceRoot::Bundle { name }) => (
                ryeos_state::objects::SourceSpaceIdentity::Bundle,
                ryeos_state::objects::SourceRootIdentity::Bundle { name: name.clone() },
            ),
            (ItemSpace::Node, ItemSourceRoot::Node) => (
                ryeos_state::objects::SourceSpaceIdentity::Node,
                ryeos_state::objects::SourceRootIdentity::Node,
            ),
            _ => anyhow::bail!("source owner has incoherent typed root authority"),
        };
    Ok(ryeos_state::objects::SourceOwnerIdentity {
        canonical_ref: resolution.root.resolved_ref.clone(),
        item_kind: item_kind.to_owned(),
        source_space,
        source_root,
        root_source_content_digest: resolution.root.source_content_digest.clone(),
        root_raw_content_digest: resolution.root.raw_content_digest.clone(),
        signer_fingerprint: signer,
        logical_item_key: logical_item_key.to_owned(),
    })
}

fn root_identity_key(root: &ItemSourceRoot) -> String {
    match root {
        ItemSourceRoot::Project => "project".to_owned(),
        ItemSourceRoot::Bundle { name } => format!("bundle:{name}"),
        ItemSourceRoot::Node => "node".to_owned(),
        ItemSourceRoot::Search { label } => format!("search:{label}"),
    }
}

struct ProjectSourceContent<'a> {
    content: &'a dyn AuthoritativeProjectContent,
    identity: String,
}

impl AuthoritativeSourceContent for ProjectSourceContent<'_> {
    fn authority_identity(&self) -> Result<String, ryeos_engine::error::EngineError> {
        Ok(self.identity.clone())
    }

    fn list_files(
        &self,
        prefix: &Path,
        max_entries: usize,
    ) -> Result<Vec<ProjectContentEntry>, ryeos_engine::error::EngineError> {
        self.content.list_files(prefix, true, max_entries)
    }

    fn read_file(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, ryeos_engine::error::EngineError> {
        self.content.read_file(path, max_bytes)
    }
}

struct DirectorySourceContent<'a> {
    root: lillux::PinnedDirectory,
    identity: String,
    configured_ignore: &'a ryeos_state::ignore::IgnoreMatcher,
}

impl<'a> DirectorySourceContent<'a> {
    fn new(
        root: &Path,
        identity: String,
        configured_ignore: &'a ryeos_state::ignore::IgnoreMatcher,
    ) -> anyhow::Result<Self> {
        let root = lillux::PinnedDirectory::open(root)?
            .ok_or_else(|| anyhow::anyhow!("source content root is unavailable"))?;
        Ok(Self {
            root,
            identity,
            configured_ignore,
        })
    }

    fn included(&self, relative: &Path) -> anyhow::Result<bool> {
        let relative = relative
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("source content path is not UTF-8"))?;
        Ok(
            !ryeos_state::project_sync::is_durable_content_capture_floor_excluded(relative)
                && !self.configured_ignore.is_ignored(relative),
        )
    }
}

impl AuthoritativeSourceContent for DirectorySourceContent<'_> {
    fn authority_identity(&self) -> Result<String, ryeos_engine::error::EngineError> {
        self.root.ensure_path_binding().map_err(engine_internal)?;
        Ok(self.identity.clone())
    }

    fn list_files(
        &self,
        prefix: &Path,
        max_entries: usize,
    ) -> Result<Vec<ProjectContentEntry>, ryeos_engine::error::EngineError> {
        let path = self.root.path().join(prefix);
        let mut entries = Vec::new();
        let present = lillux::visit_regular_files_no_follow_bounded(
            &path,
            lillux::DirectoryTraversalBudget::new(max_entries.saturating_mul(2), 64),
            |relative, _is_directory| {
                let full = prefix.join(relative);
                self.included(&full).map(|included| !included)
            },
            |relative, file| {
                if entries.len() >= max_entries {
                    anyhow::bail!("source content exceeds its signed file ceiling");
                }
                let full = prefix.join(relative);
                if !self.included(&full)? {
                    return Ok(());
                }
                let metadata = file.metadata()?;
                #[cfg(unix)]
                let normalized_mode = {
                    use std::os::unix::fs::PermissionsExt as _;
                    if metadata.permissions().mode() & 0o111 == 0 {
                        0o644
                    } else {
                        0o755
                    }
                };
                #[cfg(not(unix))]
                let normalized_mode = 0o644;
                let bytes = lillux::read_open_regular_file_bounded(
                    file,
                    ryeos_state::objects::MAX_SOURCE_FILE_BYTES,
                )?;
                entries.push(ProjectContentEntry {
                    relative_path: relative.to_path_buf(),
                    content_hash: lillux::sha256_hex(&bytes),
                    size: bytes.len() as u64,
                    normalized_mode,
                });
                Ok(())
            },
        )
        .map_err(engine_internal)?;
        if !present {
            return Err(ryeos_engine::error::EngineError::Internal(
                "source content root is absent".to_owned(),
            ));
        }
        self.root.ensure_path_binding().map_err(engine_internal)?;
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(entries)
    }

    fn read_file(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, ryeos_engine::error::EngineError> {
        if !self.included(path).map_err(engine_internal)? {
            return Ok(None);
        }
        let bytes =
            lillux::read_regular_file_bounded_no_follow(&self.root.path().join(path), max_bytes)
                .map_err(engine_internal)?;
        self.root.ensure_path_binding().map_err(engine_internal)?;
        Ok(Some(bytes))
    }
}

fn engine_internal(error: anyhow::Error) -> ryeos_engine::error::EngineError {
    ryeos_engine::error::EngineError::Internal(error.to_string())
}

struct RetainedSourceClosureStore {
    authority: ryeos_state::PinnedStateAuthority,
}

impl SourceClosureStore for RetainedSourceClosureStore {
    fn source_closure_available(
        &self,
        binding_hash: &str,
        manifest_hash: &str,
    ) -> anyhow::Result<bool> {
        let cas = self.authority.cas_store()?;
        let Some(binding_value) = cas.get_object(binding_hash)? else {
            return Ok(false);
        };
        let binding = ryeos_state::objects::EffectiveSourceBinding::from_value(&binding_value)?;
        if binding.digest()? != binding_hash || binding.content_manifest_hash != manifest_hash {
            return Ok(false);
        }
        let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
            &cas,
            [binding_hash.to_owned()],
            ryeos_state::object_closure::ObjectClosureLimits::default(),
        )?;
        Ok(closure.is_complete())
    }
}
