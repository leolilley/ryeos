use super::*;

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn require_exact_object_keys(
    value: &Value,
    expected: &[&str],
    label: &str,
) -> std::result::Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    let mut actual = object.keys().map(String::as_str).collect::<Vec<_>>();
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    if actual != expected {
        return Err(format!(
            "{label} must contain exactly [{}], got [{}]",
            expected.join(", "),
            actual.join(", ")
        ));
    }
    Ok(())
}

fn validate_exact_resolution_wire(value: &Value) -> std::result::Result<(), String> {
    require_exact_object_keys(
        value,
        &[
            "root",
            "ancestors",
            "references_edges",
            "referenced_items",
            "step_outputs",
            "effective_trust_class",
            "composed",
        ],
        "sealed resolution",
    )?;
    let validate_ancestor = |ancestor: &Value, label: &str| -> std::result::Result<(), String> {
        require_exact_object_keys(
            ancestor,
            &[
                "requested_id",
                "resolved_ref",
                "source_path",
                "source_space",
                "trust_class",
                "signer_fingerprint",
                "alias_resolution",
                "added_by",
                "raw_content",
                "source_content_digest",
                "raw_content_digest",
            ],
            label,
        )?;
        if let Some(alias) = ancestor
            .get("alias_resolution")
            .filter(|value| !value.is_null())
        {
            require_exact_object_keys(alias, &["expansion", "depth"], "sealed resolution alias")?;
        }
        Ok(())
    };
    validate_ancestor(&value["root"], "sealed resolution root")?;
    for (field, label) in [
        ("ancestors", "sealed resolution ancestor"),
        ("referenced_items", "sealed resolution referenced item"),
    ] {
        let values = value[field]
            .as_array()
            .ok_or_else(|| format!("sealed resolution {field} must be an array"))?;
        for entry in values {
            validate_ancestor(entry, label)?;
        }
    }
    let edges = value["references_edges"]
        .as_array()
        .ok_or_else(|| "sealed resolution references_edges must be an array".to_string())?;
    for edge in edges {
        require_exact_object_keys(
            edge,
            &[
                "from_ref",
                "from_source_path",
                "to_ref",
                "to_source_path",
                "to_source_space",
                "trust_class",
                "added_by",
            ],
            "sealed resolution edge",
        )?;
    }
    require_exact_object_keys(
        &value["composed"],
        &["composed", "derived", "policy_facts"],
        "sealed resolution composed view",
    )?;
    Ok(())
}

fn deserialize_exact_resolution<'de, D>(
    deserializer: D,
) -> std::result::Result<ryeos_engine::resolution::ResolutionOutput, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    validate_exact_resolution_wire(&value).map_err(serde::de::Error::custom)?;
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

pub(super) const SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedShadowedCandidate {
    label: String,
    space: ItemSpace,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedItemMetadata {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    executor_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    version: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    description: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    category: Option<String>,
    required_secrets: Vec<String>,
    extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedSourceFormat {
    extension: String,
    parser: String,
    signature_prefix: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    signature_suffix: Option<String>,
    signature_after_shebang: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedResolvedItem {
    canonical_ref: String,
    kind: String,
    source_path: PathBuf,
    source_space: ItemSpace,
    resolved_from: String,
    shadowed: Vec<SealedShadowedCandidate>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    materialized_project_root: Option<PathBuf>,
    raw_content_digest: String,
    source_content_b64: String,
    content_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    signature_header: Option<SignatureHeader>,
    source_format: SealedSourceFormat,
    metadata: SealedItemMetadata,
}

impl SealedResolvedItem {
    fn capture(resolved: &ResolvedItem) -> Result<Self> {
        let source_bytes = std::fs::read(&resolved.source_path).with_context(|| {
            format!(
                "read admitted item source for launch capsule: {}",
                resolved.source_path.display()
            )
        })?;
        let source_digest = lillux::sha256_hex(&source_bytes);
        if source_digest != resolved.content_hash {
            bail!(
                "admitted item source changed before launch capsule capture: {} (resolved source {}, observed source {}; resolved runtime body {})",
                resolved.source_path.display(),
                resolved.content_hash,
                source_digest,
                resolved.raw_content_digest
            );
        }
        Ok(Self {
            canonical_ref: resolved.canonical_ref.to_string(),
            kind: resolved.kind.clone(),
            source_path: resolved.source_path.clone(),
            source_space: resolved.source_space,
            resolved_from: resolved.resolved_from.clone(),
            shadowed: resolved
                .shadowed
                .iter()
                .map(|candidate| SealedShadowedCandidate {
                    label: candidate.label.clone(),
                    space: candidate.space,
                    path: candidate.path.clone(),
                })
                .collect(),
            materialized_project_root: resolved.materialized_project_root.clone(),
            raw_content_digest: resolved.raw_content_digest.clone(),
            source_content_b64: base64::engine::general_purpose::STANDARD.encode(source_bytes),
            content_hash: resolved.content_hash.clone(),
            signature_header: resolved.signature_header.clone(),
            source_format: SealedSourceFormat {
                extension: resolved.source_format.extension.clone(),
                parser: resolved.source_format.parser.clone(),
                signature_prefix: resolved.source_format.signature.prefix.clone(),
                signature_suffix: resolved.source_format.signature.suffix.clone(),
                signature_after_shebang: resolved.source_format.signature.after_shebang,
            },
            metadata: SealedItemMetadata {
                executor_id: resolved.metadata.executor_id.clone(),
                version: resolved.metadata.version.clone(),
                description: resolved.metadata.description.clone(),
                category: resolved.metadata.category.clone(),
                required_secrets: resolved.metadata.required_secrets.clone(),
                extra: resolved.metadata.extra.clone(),
            },
        })
    }

    fn restore(&self, capsule_root: &Path) -> Result<ResolvedItem> {
        let canonical_ref = CanonicalRef::parse(&self.canonical_ref)
            .map_err(|error| anyhow!("invalid sealed canonical ref: {error}"))?;
        if canonical_ref.kind != self.kind {
            bail!(
                "sealed resolved kind `{}` does not match canonical ref kind `{}`",
                self.kind,
                canonical_ref.kind
            );
        }
        let source_bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.source_content_b64)
            .context("decode admitted launch capsule source bytes")?;
        let source_digest = lillux::sha256_hex(&source_bytes);
        if source_digest != self.content_hash {
            bail!(
                "admitted launch capsule source digest mismatch: sealed source {}, observed source {}; sealed runtime body {}",
                self.content_hash,
                source_digest,
                self.raw_content_digest
            );
        }
        std::fs::create_dir_all(capsule_root).with_context(|| {
            format!(
                "create admitted launch capsule root {}",
                capsule_root.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(capsule_root, std::fs::Permissions::from_mode(0o700))?;
        }
        let materialized_source = capsule_root.join("subject.source");
        if materialized_source.exists() {
            let existing = std::fs::read(&materialized_source)?;
            if lillux::sha256_hex(&existing) != self.content_hash {
                bail!(
                    "admitted launch capsule materialization has conflicting content: {}",
                    materialized_source.display()
                );
            }
        } else {
            lillux::atomic_write(&materialized_source, &source_bytes).with_context(|| {
                format!(
                    "materialize admitted launch capsule source {}",
                    materialized_source.display()
                )
            })?;
        }
        Ok(ResolvedItem {
            canonical_ref,
            kind: self.kind.clone(),
            source_path: materialized_source,
            source_space: self.source_space,
            resolved_from: self.resolved_from.clone(),
            shadowed: self
                .shadowed
                .iter()
                .map(|candidate| ShadowedCandidate {
                    label: candidate.label.clone(),
                    space: candidate.space,
                    path: candidate.path.clone(),
                })
                .collect(),
            materialized_project_root: Some(capsule_root.to_path_buf()),
            raw_content_digest: self.raw_content_digest.clone(),
            content_hash: self.content_hash.clone(),
            signature_header: self.signature_header.clone(),
            source_format: ResolvedSourceFormat {
                extension: self.source_format.extension.clone(),
                parser: self.source_format.parser.clone(),
                signature: SignatureEnvelope {
                    prefix: self.source_format.signature_prefix.clone(),
                    suffix: self.source_format.signature_suffix.clone(),
                    after_shebang: self.source_format.signature_after_shebang,
                },
            },
            metadata: ItemMetadata {
                executor_id: self.metadata.executor_id.clone(),
                version: self.metadata.version.clone(),
                description: self.metadata.description.clone(),
                category: self.metadata.category.clone(),
                required_secrets: self.metadata.required_secrets.clone(),
                extra: self.metadata.extra.clone(),
            },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
// This exact durable wire shape keeps delegated-principal fields flattened.
// Adding indirection would change the sealed request representation.
#[allow(clippy::large_enum_variant)]
enum SealedPrincipal {
    Local {
        fingerprint: String,
        scopes: Vec<String>,
    },
    Delegated {
        protocol_version: String,
        delegation_id: String,
        caller_fingerprint: String,
        origin_site_id: String,
        audience_site_id: String,
        delegated_scopes: Vec<String>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        budget_lease_id: Option<String>,
        request_hash: String,
        idempotency_key: String,
        issued_at: String,
        expires_at: String,
        non_redelegable: bool,
        origin_signature: String,
    },
}

impl From<&EffectivePrincipal> for SealedPrincipal {
    fn from(principal: &EffectivePrincipal) -> Self {
        match principal {
            EffectivePrincipal::Local(principal) => Self::Local {
                fingerprint: principal.fingerprint.clone(),
                scopes: principal.scopes.clone(),
            },
            EffectivePrincipal::Delegated(principal) => Self::Delegated {
                protocol_version: principal.protocol_version.clone(),
                delegation_id: principal.delegation_id.clone(),
                caller_fingerprint: principal.caller_fingerprint.clone(),
                origin_site_id: principal.origin_site_id.clone(),
                audience_site_id: principal.audience_site_id.clone(),
                delegated_scopes: principal.delegated_scopes.clone(),
                budget_lease_id: principal.budget_lease_id.clone(),
                request_hash: principal.request_hash.clone(),
                idempotency_key: principal.idempotency_key.clone(),
                issued_at: principal.issued_at.clone(),
                expires_at: principal.expires_at.clone(),
                non_redelegable: principal.non_redelegable,
                origin_signature: principal.origin_signature.clone(),
            },
        }
    }
}

impl SealedPrincipal {
    fn restore(&self) -> EffectivePrincipal {
        match self {
            Self::Local {
                fingerprint,
                scopes,
            } => EffectivePrincipal::Local(Principal {
                fingerprint: fingerprint.clone(),
                scopes: scopes.clone(),
            }),
            Self::Delegated {
                protocol_version,
                delegation_id,
                caller_fingerprint,
                origin_site_id,
                audience_site_id,
                delegated_scopes,
                budget_lease_id,
                request_hash,
                idempotency_key,
                issued_at,
                expires_at,
                non_redelegable,
                origin_signature,
            } => EffectivePrincipal::Delegated(Box::new(DelegatedPrincipal {
                protocol_version: protocol_version.clone(),
                delegation_id: delegation_id.clone(),
                caller_fingerprint: caller_fingerprint.clone(),
                origin_site_id: origin_site_id.clone(),
                audience_site_id: audience_site_id.clone(),
                delegated_scopes: delegated_scopes.clone(),
                budget_lease_id: budget_lease_id.clone(),
                request_hash: request_hash.clone(),
                idempotency_key: idempotency_key.clone(),
                issued_at: issued_at.clone(),
                expires_at: expires_at.clone(),
                non_redelegable: *non_redelegable,
                origin_signature: origin_signature.clone(),
            })),
        }
    }
}

/// Exact, current-format durable authority for a root admitted before its
/// first launch. This is persisted only for the created-root crash window and
/// reconstructs the complete request without consulting mutable item source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedRootExecutionRequest {
    schema_version: u32,
    kind: String,
    item_ref: String,
    executor_ref: String,
    runtime_ref: String,
    launch_mode: String,
    current_site_id: String,
    origin_site_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    target_site_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    requested_by: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    usage_subject: Option<UsageSubject>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    usage_subject_asserted_by: Option<String>,
    parameters: Value,
    ref_bindings: BTreeMap<String, String>,
    verified_subject: SealedResolvedItem,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    verified_signer_fingerprint: Option<String>,
    verified_trust_class: TrustClass,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    verified_pinned_version: Option<PinnedVersion>,
    #[serde(deserialize_with = "deserialize_exact_resolution")]
    resolution_output: ryeos_engine::resolution::ResolutionOutput,
    planning_principal: SealedPrincipal,
    project_context: ProjectContext,
    project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    execution_hints: HashMap<String, Value>,
    validate_only: bool,
    resolved_history_policy: ResolvedThreadHistoryPolicy,
    captured_history_policy: ryeos_state::objects::CapturedThreadHistoryPolicy,
}

impl SealedRootExecutionRequest {
    pub fn capture(request: &ResolvedExecutionRequest, runtime_ref: String) -> Result<Self> {
        let resolution_output = request
            .root_admission
            .as_ref()
            .ok_or_else(|| anyhow!("cannot seal a root execution request without root admission"))?
            .resolution_output()
            .clone();
        Self::capture_with_resolution(request, runtime_ref, resolution_output)
    }

    pub fn capture_with_resolution(
        request: &ResolvedExecutionRequest,
        runtime_ref: String,
        resolution_output: ryeos_engine::resolution::ResolutionOutput,
    ) -> Result<Self> {
        let admission = request.root_admission.as_ref().ok_or_else(|| {
            anyhow!("cannot seal a root execution request without root admission")
        })?;
        admission.validate()?;
        admission.ensure_matches_request(request)?;
        validate_launch_mode(&request.launch_mode)?;
        if runtime_ref.trim().is_empty() || runtime_ref.trim() != runtime_ref {
            bail!("sealed root execution runtime ref must be non-empty and trimmed");
        }
        CanonicalRef::parse(&runtime_ref)
            .map_err(|error| anyhow!("invalid sealed runtime ref `{runtime_ref}`: {error}"))?;
        let verified = &admission.verified_subject;
        if resolution_output.root.raw_content_digest != verified.resolved.raw_content_digest {
            bail!(
                "sealed resolution root digest does not match admitted subject: resolution={}, admitted={}",
                resolution_output.root.raw_content_digest,
                verified.resolved.raw_content_digest
            );
        }
        Ok(Self {
            schema_version: SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION,
            kind: request.kind.clone(),
            item_ref: request.item_ref.clone(),
            executor_ref: request.executor_ref.clone(),
            runtime_ref,
            launch_mode: request.launch_mode.clone(),
            current_site_id: request.current_site_id.clone(),
            origin_site_id: request.origin_site_id.clone(),
            target_site_id: request.target_site_id.clone(),
            requested_by: request.requested_by.clone(),
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
            parameters: request.parameters.clone(),
            ref_bindings: request.ref_bindings.clone(),
            verified_subject: SealedResolvedItem::capture(&verified.resolved)?,
            verified_signer_fingerprint: verified.signer.as_ref().map(|value| value.0.clone()),
            verified_trust_class: verified.trust_class,
            verified_pinned_version: verified.pinned_version.clone(),
            resolution_output,
            planning_principal: SealedPrincipal::from(&admission.plan_context.requested_by),
            project_context: admission.plan_context.project_context.clone(),
            project_authority: admission.project_authority().clone(),
            execution_hints: admission.plan_context.execution_hints.values.clone(),
            validate_only: admission.plan_context.validate_only,
            resolved_history_policy: admission.resolved_history_policy.clone(),
            captured_history_policy: admission.captured_history_policy.clone(),
        })
    }

    pub fn runtime_ref(&self) -> &str {
        &self.runtime_ref
    }

    pub fn executor_ref(&self) -> &str {
        &self.executor_ref
    }

    pub fn item_ref(&self) -> &str {
        &self.item_ref
    }

    pub fn project_context(&self) -> &ProjectContext {
        &self.project_context
    }

    /// Stable program closure shared by continuation segments. Invocation
    /// stimulus, principal envelope, and project realization are authorized
    /// separately and may change at an explicit continuation boundary; item
    /// bytes, resolution, bindings, runtime identity, and launch semantics may
    /// not.
    pub fn admitted_program_value(&self) -> Result<Value> {
        let mut value = serde_json::to_value(self).context("serialize admitted program")?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| anyhow!("sealed execution request is not an object"))?;
        for invocation_field in [
            "parameters",
            "requested_by",
            "planning_principal",
            "project_context",
            "project_authority",
            "usage_subject",
            "usage_subject_asserted_by",
        ] {
            object
                .remove(invocation_field)
                .ok_or_else(|| anyhow!("sealed execution request is missing {invocation_field}"))?;
        }
        Ok(value)
    }

    pub fn admitted_program_hash(&self) -> Result<String> {
        let canonical = lillux::canonical_json(&self.admitted_program_value()?)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    /// Rebind only the invocation envelope of an exact admitted program for a
    /// continuation segment. Program bytes, composed resolution, runtime
    /// identity, and trust facts remain byte-for-byte inherited.
    pub fn for_continuation_invocation(
        &self,
        resume: &crate::launch_metadata::ResumeContext,
    ) -> Result<Self> {
        if self.kind != resume.kind
            || self.item_ref != resume.item_ref
            || self.ref_bindings != resume.ref_bindings
            || self.launch_mode != resume.launch_mode
            || self.current_site_id != resume.current_site_id
            || self.origin_site_id != resume.origin_site_id
            || resume.executor_ref.as_deref() != Some(self.executor_ref())
            || resume.runtime_ref.as_deref() != Some(self.runtime_ref())
            || self.execution_hints != resume.execution_hints.values
        {
            bail!(
                "continuation invocation does not match admitted program identity for {}",
                resume.item_ref
            );
        }
        let mut successor = self.clone();
        successor.parameters = resume.parameters.clone();
        successor.requested_by = Some(resume.principal_identifier().to_string());
        successor.planning_principal = SealedPrincipal::from(&resume.requested_by);
        successor.project_context = resume.project_context.clone();
        successor.project_authority = resume.project_authority.clone();
        successor.execution_hints = resume.execution_hints.values.clone();
        Ok(successor)
    }

    /// Exact captured policy carried by the synthetic storage fixture.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn captured_history_policy(&self) -> &ryeos_state::objects::CapturedThreadHistoryPolicy {
        &self.captured_history_policy
    }

    /// Structurally complete current-format value for storage-boundary tests.
    /// It is deliberately unavailable in production builds and is never valid
    /// launch authority because its synthetic subject is unsigned.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn storage_test_fixture() -> Self {
        let content_hash = "11".repeat(32);
        let kind_schema_content_hash = "22".repeat(32);
        let canonical_item_ref = "graph:test/storage-fixture".to_string();
        let resolved_history_policy = ResolvedThreadHistoryPolicy {
            retention: ryeos_engine::history_policy::ThreadHistoryRetention::Durable,
            canonical_item_ref: canonical_item_ref.clone(),
            item_content_hash: content_hash.clone(),
            item_signer_fingerprint: None,
            item_trust_class: TrustClass::Unsigned,
            kind_schema_content_hash: kind_schema_content_hash.clone(),
            source: PolicyProvenance::NodeDefault {
                node_policy: NodeHistoryPolicyProvenance::MissingConfig,
            },
        };
        let captured_history_policy = ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
            canonical_item_ref: canonical_item_ref.clone(),
            item_content_hash: content_hash.clone(),
            item_signer_fingerprint: None,
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Unsigned,
            kind_schema_content_hash,
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy:
                    ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
            },
        };
        Self {
            schema_version: SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION,
            kind: "graph_run".to_string(),
            item_ref: canonical_item_ref.clone(),
            executor_ref: "native:storage-fixture".to_string(),
            runtime_ref: "runtime:storage-fixture".to_string(),
            launch_mode: "detached".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            target_site_id: None,
            requested_by: Some("session:test".to_string()),
            usage_subject: None,
            usage_subject_asserted_by: None,
            parameters: json!({}),
            ref_bindings: BTreeMap::new(),
            verified_subject: SealedResolvedItem {
                canonical_ref: canonical_item_ref.clone(),
                kind: "graph".to_string(),
                source_path: PathBuf::from("/synthetic/storage-fixture.yaml"),
                source_space: ItemSpace::Project,
                resolved_from: "storage_test_fixture".to_string(),
                shadowed: Vec::new(),
                materialized_project_root: None,
                raw_content_digest: content_hash.clone(),
                source_content_b64: base64::engine::general_purpose::STANDARD.encode(b"{}"),
                content_hash: content_hash.clone(),
                signature_header: None,
                source_format: SealedSourceFormat {
                    extension: "yaml".to_string(),
                    parser: "yaml-header-document".to_string(),
                    signature_prefix: "# ".to_string(),
                    signature_suffix: None,
                    signature_after_shebang: false,
                },
                metadata: SealedItemMetadata {
                    executor_id: None,
                    version: None,
                    description: None,
                    category: None,
                    required_secrets: Vec::new(),
                    extra: HashMap::new(),
                },
            },
            verified_signer_fingerprint: None,
            verified_trust_class: TrustClass::Unsigned,
            verified_pinned_version: None,
            resolution_output: ryeos_engine::resolution::ResolutionOutput {
                root: ryeos_engine::resolution::ResolvedAncestor {
                    requested_id: canonical_item_ref.clone(),
                    resolved_ref: canonical_item_ref,
                    source_path: PathBuf::from("/synthetic/storage-fixture.yaml"),
                    source_space: ItemSpace::Project,
                    trust_class: ResolutionTrustClass::Unsigned,
                    signer_fingerprint: None,
                    alias_resolution: None,
                    added_by: ryeos_engine::resolution::ResolutionStepName::PipelineInit,
                    raw_content: "{}".to_string(),
                    source_content_digest: content_hash.clone(),
                    raw_content_digest: content_hash,
                },
                ancestors: Vec::new(),
                references_edges: Vec::new(),
                referenced_items: Vec::new(),
                step_outputs: HashMap::new(),
                effective_trust_class: ResolutionTrustClass::Unsigned,
                composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
            },
            planning_principal: SealedPrincipal::Local {
                fingerprint: "session:test".to_string(),
                scopes: Vec::new(),
            },
            project_context: ProjectContext::None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            execution_hints: HashMap::new(),
            validate_only: false,
            resolved_history_policy,
            captured_history_policy,
        }
    }

    /// Current-shape synthetic authority with a caller-selected project pair.
    /// Test-only because the sealed subject remains deliberately unsigned and
    /// is never valid launch authority.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn storage_test_fixture_with_project_identity(
        project_context: ProjectContext,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> Self {
        let mut fixture = Self::storage_test_fixture();
        fixture.project_context = project_context;
        fixture.project_authority = project_authority;
        fixture
    }

    pub fn restore(
        &self,
        engine: &Arc<Engine>,
        capsule_root: &Path,
    ) -> Result<ResolvedExecutionRequest> {
        if self.schema_version != SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION {
            bail!(
                "sealed root execution request schema mismatch: persisted={}, expected={}",
                self.schema_version,
                SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION
            );
        }
        if self.validate_only {
            bail!("persisted root execution request cannot be validate-only");
        }
        self.project_authority
            .validate()
            .context("validate sealed root project authority")?;
        validate_launch_mode(&self.launch_mode)?;
        if self.runtime_ref.trim().is_empty() || self.runtime_ref.trim() != self.runtime_ref {
            bail!("sealed root execution runtime ref must be non-empty and trimmed");
        }
        CanonicalRef::parse(&self.runtime_ref).map_err(|error| {
            anyhow!("invalid sealed runtime ref `{}`: {error}", self.runtime_ref)
        })?;
        let resolved_item = self.verified_subject.restore(capsule_root)?;
        let verified_subject = VerifiedItem {
            resolved: resolved_item.clone(),
            signer: self
                .verified_signer_fingerprint
                .clone()
                .map(SignerFingerprint),
            trust_class: self.verified_trust_class,
            pinned_version: self.verified_pinned_version.clone(),
        };
        let plan_context = PlanContext {
            requested_by: self.planning_principal.restore(),
            project_context: self.project_context.clone(),
            current_site_id: self.current_site_id.clone(),
            origin_site_id: self.origin_site_id.clone(),
            execution_hints: ExecutionHints {
                values: self.execution_hints.clone(),
            },
            validate_only: false,
        };
        let project_binding =
            AdmittedProjectBinding::restore(engine, &plan_context, self.project_authority.clone())?;
        let admission = RootExecutionAdmission {
            verified_subject,
            resolution_output: self.resolution_output.clone(),
            plan_context: plan_context.clone(),
            thread_profile: self.kind.clone(),
            usage_subject: self.usage_subject.clone(),
            usage_subject_asserted_by: self.usage_subject_asserted_by.clone(),
            ref_bindings: self.ref_bindings.clone(),
            resolved_history_policy: self.resolved_history_policy.clone(),
            captured_history_policy: self.captured_history_policy.clone(),
            project_binding,
        };
        admission.validate_for_persistence()?;
        let request = ResolvedExecutionRequest {
            kind: self.kind.clone(),
            item_ref: self.item_ref.clone(),
            executor_ref: self.executor_ref.clone(),
            launch_mode: self.launch_mode.clone(),
            current_site_id: self.current_site_id.clone(),
            origin_site_id: self.origin_site_id.clone(),
            target_site_id: self.target_site_id.clone(),
            requested_by: self.requested_by.clone(),
            usage_subject: self.usage_subject.clone(),
            usage_subject_asserted_by: self.usage_subject_asserted_by.clone(),
            parameters: self.parameters.clone(),
            ref_bindings: self.ref_bindings.clone(),
            root_raw_content_digest: resolved_item.raw_content_digest.clone(),
            resolved_item,
            plan_context,
            root_admission: Some(admission.clone()),
        };
        admission.ensure_matches_request(&request)?;
        Ok(request)
    }

    /// Restore a sealed program after recovery has reconstructed its exact
    /// current execution workspace. The persisted invocation is validated
    /// first, then only its operational project materialization is rebound
    /// from the newly proven provenance. No caller-supplied path participates.
    pub fn restore_for_reconstructed_provenance(
        &self,
        engine: &Arc<Engine>,
        capsule_root: &Path,
        provenance: &crate::execution_provenance::ExecutionProvenance,
    ) -> Result<ResolvedExecutionRequest> {
        if !Arc::ptr_eq(engine, provenance.request_engine()) {
            bail!("reconstructed provenance engine differs from sealed request engine");
        }
        if &self.project_authority != provenance.project_authority() {
            bail!("reconstructed provenance authority differs from sealed request authority");
        }

        // Validate the exact persisted invocation before rebinding its
        // disposable operational workspace.
        let mut request = self.restore(engine, capsule_root)?;
        let rebound_project_context = match provenance {
            crate::execution_provenance::ExecutionProvenance::Projectless { .. } => {
                ProjectContext::None
            }
            crate::execution_provenance::ExecutionProvenance::RootLiveProject { .. }
            | crate::execution_provenance::ExecutionProvenance::ChildLiveProject { .. }
            | crate::execution_provenance::ExecutionProvenance::RootPinnedGeneration { .. }
            | crate::execution_provenance::ExecutionProvenance::ChildPinnedGeneration { .. } => {
                ProjectContext::LocalPath {
                    path: provenance.effective_path().to_path_buf(),
                }
            }
        };
        let mut rebound_plan_context = request.plan_context.clone();
        rebound_plan_context.project_context = rebound_project_context;
        let rebound_binding =
            AdmittedProjectBinding::from_provenance(engine, &rebound_plan_context, provenance)?;
        {
            let admission = request
                .root_admission
                .as_mut()
                .ok_or_else(|| anyhow!("restored sealed root has no admission"))?;
            admission.plan_context = rebound_plan_context.clone();
            admission.project_binding = rebound_binding;
            admission.validate_for_persistence()?;
        }
        request.plan_context = rebound_plan_context;
        let admission = request
            .root_admission
            .as_ref()
            .ok_or_else(|| anyhow!("restored sealed root lost its admission"))?;
        admission.ensure_matches_request(&request)?;
        admission.ensure_matches_provenance(provenance)?;
        Ok(request)
    }

    /// Restore the exact invocation rooted by an admitted launch capsule.
    ///
    /// This is the recovery-side inverse of
    /// [`crate::launch_metadata::RuntimeLaunchMetadata::admitted_launch_capsule`].
    /// It decodes no operational metadata and performs no item lookup: the
    /// signed subject and full resolution closure come only from the capsule's
    /// `sealed_invocation`.
    pub fn restore_from_admitted_capsule(
        capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
        engine: &Arc<Engine>,
        capsule_root: &Path,
        provenance: &crate::execution_provenance::ExecutionProvenance,
    ) -> Result<ResolvedExecutionRequest> {
        capsule.validate()?;
        let sealed: Self = serde_json::from_value(capsule.sealed_invocation.clone())
            .context("decode admitted capsule sealed invocation")?;
        if sealed.admitted_program_value()? != capsule.exact_program
            || sealed.admitted_program_hash()? != capsule.exact_program_hash
            || sealed.project_authority() != &capsule.project_authority
            || sealed.runtime_ref() != capsule.runtime_ref
            || sealed.executor_ref() != capsule.executor_ref
        {
            bail!("admitted capsule invocation contradicts its rooted program authority");
        }
        sealed.restore_for_reconstructed_provenance(engine, capsule_root, provenance)
    }

    pub fn project_authority(&self) -> &ryeos_state::objects::ExecutionProjectAuthority {
        &self.project_authority
    }
}
