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
                "source_root",
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

/// v11 additionally seals the optional ingress-authenticated handler context.
/// Its absence is meaningful: node-internal execution must not manufacture a
/// verified transport principal during recovery or callback dispatch.
/// v12 carries flat exact node-history policy provenance instead of the
/// predecessor tagged config wrapper. v13 binds remotely adopted execution
/// to the exact target-node operator grant generation.
pub(super) const SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION: u32 = 13;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedShadowedCandidate {
    label: String,
    space: ItemSpace,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    source_root: ItemSourceRoot,
    resolved_from: String,
    shadowed: Vec<SealedShadowedCandidate>,
    probed_absent: Vec<ryeos_engine::contracts::ProbedAbsence>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    materialized_project_root: Option<PathBuf>,
    subject_resolution_authority: ryeos_engine::contracts::SubjectResolutionAuthority,
    raw_content_digest: String,
    source_content_b64: String,
    content_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    signature_header: Option<SignatureHeader>,
    source_format: SealedSourceFormat,
    metadata: SealedItemMetadata,
}

/// Secret-free exact subject retained by an admitted launch. This is the
/// narrow read model used by definition inspection; it deliberately excludes
/// source paths, invocation parameters, capabilities, and execution closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedProgramSubject {
    pub canonical_ref: String,
    pub kind: String,
    pub source_content: String,
    pub source_content_digest: String,
    pub raw_content: String,
    pub raw_content_digest: String,
    pub source_extension: String,
    pub parser_ref: String,
}

impl SealedResolvedItem {
    fn capture(resolved: &ResolvedItem, retained_source_bytes: Option<&[u8]>) -> Result<Self> {
        let source_bytes = match retained_source_bytes {
            Some(bytes) => bytes.to_vec(),
            None if resolved
                .subject_resolution_authority
                .operational_generation()
                .is_some() =>
            {
                bail!(
                    "content-addressed admitted item source was not retained before launch capsule capture: {}",
                    resolved.source_path.display()
                )
            }
            None => {
                ryeos_engine::item_resolution::read_item_source_no_follow(&resolved.source_path)
                    .with_context(|| {
                        format!(
                            "securely read admitted item source for launch capsule: {}",
                            resolved.source_path.display()
                        )
                    })?
                    .into_bytes()
            }
        };
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
            source_root: resolved.source_root.clone(),
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
            probed_absent: resolved.probed_absent.clone(),
            materialized_project_root: resolved.materialized_project_root.clone(),
            subject_resolution_authority: resolved.subject_resolution_authority.clone(),
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
        let pinned_capsule_root = lillux::PinnedDirectory::open_or_create(capsule_root)
            .with_context(|| {
                format!(
                    "create admitted launch capsule root {}",
                    capsule_root.display()
                )
            })?;
        pinned_capsule_root.set_mode(0o700)?;
        pinned_capsule_root.ensure_path_binding()?;
        let source_name = std::ffi::OsStr::new("subject.source");
        let mut materialized_source_file =
            match pinned_capsule_root.open_regular(source_name, false)? {
                Some(existing) => existing,
                None => match pinned_capsule_root.atomic_create_regular(
                    source_name,
                    &source_bytes,
                    0o600,
                )? {
                    Some(created) => created,
                    None => pinned_capsule_root
                        .open_regular(source_name, false)?
                        .ok_or_else(|| {
                            anyhow!("admitted launch capsule source disappeared during create race")
                        })?,
                },
            };
        let expected_source_len = u64::try_from(source_bytes.len())
            .context("admitted launch capsule source length exceeds u64")?;
        let (materialized_source_hash, materialized_source_metadata) =
            lillux::digest_open_regular_file_stable_exact(
                &mut materialized_source_file,
                expected_source_len,
            )?;
        if materialized_source_hash != self.content_hash
            || materialized_source_metadata.len() != expected_source_len
        {
            bail!(
                "admitted launch capsule materialization has conflicting content: {}",
                capsule_root.join(source_name).display()
            );
        }
        pinned_capsule_root.ensure_path_binding()?;
        let materialized_source = capsule_root.join(source_name);
        Ok(ResolvedItem {
            canonical_ref,
            kind: self.kind.clone(),
            source_path: materialized_source,
            source_space: self.source_space,
            source_root: self.source_root.clone(),
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
            probed_absent: self.probed_absent.clone(),
            // The capsule directory owns only the retained source bytes. It is
            // never project authority. Direct restore keeps the sealed
            // materialized root; reconstructed recovery rebinds this field to
            // the exact provenance workspace before dispatch.
            materialized_project_root: self.materialized_project_root.clone(),
            subject_resolution_authority: self.subject_resolution_authority.clone(),
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

    fn exact_subject(&self) -> Result<AdmittedProgramSubject> {
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
        let observed_source_hash = lillux::sha256_hex(&source_bytes);
        if observed_source_hash != self.content_hash {
            bail!(
                "admitted launch capsule source digest mismatch: sealed source {}, observed source {}",
                self.content_hash,
                observed_source_hash
            );
        }
        let source_content = String::from_utf8(source_bytes)
            .context("admitted launch capsule subject source is not UTF-8")?;
        let raw_content = lillux::signature::strip_signature_lines_with_envelope(
            &source_content,
            &self.source_format.signature_prefix,
            self.source_format.signature_suffix.as_deref(),
        );
        let observed_raw_digest = ryeos_engine::item_resolution::content_hash(&raw_content);
        if observed_raw_digest != self.raw_content_digest {
            bail!(
                "admitted launch capsule runtime-body digest mismatch: sealed {}, observed {}",
                self.raw_content_digest,
                observed_raw_digest
            );
        }
        Ok(AdmittedProgramSubject {
            canonical_ref: self.canonical_ref.clone(),
            kind: self.kind.clone(),
            source_content,
            source_content_digest: self.content_hash.clone(),
            raw_content,
            raw_content_digest: self.raw_content_digest.clone(),
            source_extension: self.source_format.extension.clone(),
            parser_ref: self.source_format.parser.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    fn restore(&self) -> Result<EffectivePrincipal> {
        match self {
            Self::Local {
                fingerprint,
                scopes,
            } => Ok(EffectivePrincipal::Local(Principal {
                fingerprint: fingerprint.clone(),
                scopes: scopes.clone(),
            })),
            Self::Delegated { .. } => bail!(
                "sealed request principal restore refused: delegated principals have no acceptance path and their origin signature is unverifiable"
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedRootExecutionRequest {
    schema_version: u32,
    kind: String,
    item_ref: String,
    executor_ref: String,
    executor_route: AdmittedExecutorRoute,
    runtime_ref: String,
    launch_mode: String,
    current_site_id: String,
    origin_site_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    handler_context: Option<crate::handler_context::HandlerContext>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    admitted_operator_authority: Option<crate::operator_authority::AdmittedOperatorAuthority>,
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
    resolved_ref_bindings: BTreeMap<String, Value>,
    verified_subject: SealedResolvedItem,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    verified_signer_fingerprint: Option<String>,
    verified_trust_class: TrustClass,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    verified_pinned_version: Option<PinnedVersion>,
    #[serde(deserialize_with = "deserialize_exact_resolution")]
    resolution_output: ryeos_engine::resolution::ResolutionOutput,
    effective_definition_digest: ryeos_engine::resolution::EffectiveDefinitionDigest,
    planning_principal: SealedPrincipal,
    project_context: ProjectContext,
    project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    project_binding_subject_authority: ryeos_engine::contracts::SubjectResolutionAuthority,
    resolution_subject_authority: ryeos_engine::contracts::SubjectResolutionAuthority,
    execution_hints: HashMap<String, Value>,
    validate_only: bool,
    resolved_history_policy: ResolvedThreadHistoryPolicy,
    resolved_result_policy: ryeos_engine::history_policy::ResolvedThreadResultPolicy,
    captured_history_policy: ryeos_state::objects::CapturedThreadHistoryPolicy,
}

impl SealedRootExecutionRequest {
    /// Decode and exhaustively validate the immutable invocation retained by
    /// an admitted launch capsule without materializing node-local source or
    /// consulting an engine. Cross-site placement uses this boundary so the
    /// capsule remains the sole portable launch authority.
    pub fn decode_from_admitted_capsule(
        capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    ) -> Result<Self> {
        capsule.validate()?;
        let sealed: Self = serde_json::from_value(capsule.sealed_invocation.clone())
            .context("decode admitted capsule sealed invocation")?;
        sealed.validate_executor_route_against_capsule(capsule)?;
        if sealed.admitted_program_value()? != capsule.exact_program
            || sealed.admitted_program_hash()? != capsule.exact_program_hash
            || sealed.project_authority() != &capsule.project_authority
            || sealed.runtime_ref() != capsule.runtime_ref
            || sealed.executor_ref() != capsule.executor_ref
        {
            bail!("admitted capsule invocation contradicts its rooted program authority");
        }
        Ok(sealed)
    }

    pub fn capture_finalized(
        request: &ResolvedExecutionRequest,
        runtime_ref: String,
        program: &ryeos_engine::effective_program::FinalizedEffectiveProgram,
        handler_context: Option<&crate::handler_context::HandlerContext>,
    ) -> Result<Self> {
        Self::capture_finalized_with_ref_bindings(
            request,
            runtime_ref,
            program,
            BTreeMap::new(),
            handler_context,
        )
    }

    /// Seal a managed launch together with the exact, engine-resolved binding
    /// identities used by its launch preparer. Values are deliberately opaque
    /// here: the state capsule owns their mechanical equality check while the
    /// executor remains the only component that constructs the records.
    pub fn capture_finalized_with_ref_bindings(
        request: &ResolvedExecutionRequest,
        runtime_ref: String,
        program: &ryeos_engine::effective_program::FinalizedEffectiveProgram,
        resolved_ref_bindings: BTreeMap<String, Value>,
        handler_context: Option<&crate::handler_context::HandlerContext>,
    ) -> Result<Self> {
        Self::capture_with_effective_parts(
            request,
            runtime_ref,
            program.resolution().clone(),
            program.effective_definition_digest().clone(),
            resolved_ref_bindings,
            handler_context,
        )
    }

    fn capture_with_effective_parts(
        request: &ResolvedExecutionRequest,
        runtime_ref: String,
        resolution_output: ryeos_engine::resolution::ResolutionOutput,
        effective_definition_digest: ryeos_engine::resolution::EffectiveDefinitionDigest,
        resolved_ref_bindings: BTreeMap<String, Value>,
        handler_context: Option<&crate::handler_context::HandlerContext>,
    ) -> Result<Self> {
        let admission = request.root_admission.as_ref().ok_or_else(|| {
            anyhow!("cannot seal a root execution request without root admission")
        })?;
        admission.validate()?;
        admission.ensure_matches_request(request)?;
        let (principal, scopes) = match &admission.plan_context.requested_by {
            EffectivePrincipal::Local(principal) => {
                (principal.fingerprint.as_str(), principal.scopes.as_slice())
            }
            EffectivePrincipal::Delegated(principal) => (
                principal.caller_fingerprint.as_str(),
                principal.delegated_scopes.as_slice(),
            ),
        };
        if let Some(context) = handler_context {
            context.validate_execution_authority(
                principal,
                scopes,
                &request.current_site_id,
                &request.origin_site_id,
            )?;
        }
        validate_launch_mode(&request.launch_mode)?;
        if runtime_ref.trim().is_empty() || runtime_ref.trim() != runtime_ref {
            bail!("sealed root execution runtime ref must be non-empty and trimmed");
        }
        CanonicalRef::parse(&runtime_ref)
            .map_err(|error| anyhow!("invalid sealed runtime ref `{runtime_ref}`: {error}"))?;
        let executor_route = match admission.selected_executor_route.as_ref() {
            Some(route @ AdmittedExecutorRoute::RootExecutorChain { .. })
            | Some(route @ AdmittedExecutorRoute::DirectNativeExecutor { .. }) => route.clone(),
            Some(
                route @ AdmittedExecutorRoute::ManagedRuntimeForKind {
                    runtime_ref: admitted_runtime,
                    ..
                },
            ) => {
                if admitted_runtime != &runtime_ref {
                    bail!(
                        "sealed runtime `{runtime_ref}` differs from admitted managed route `{admitted_runtime}`"
                    );
                }
                route.clone()
            }
            Some(
                route @ AdmittedExecutorRoute::RuntimeDescriptorExecutor {
                    runtime_ref: admitted_runtime,
                    ..
                },
            ) => {
                if admitted_runtime != &runtime_ref {
                    bail!(
                        "sealed runtime `{runtime_ref}` differs from admitted descriptor route `{admitted_runtime}`"
                    );
                }
                route.clone()
            }
            None => bail!("cannot seal a root request without a typed executor route"),
        };
        if executor_route.executor_ref() != request.executor_ref {
            bail!(
                "sealed executor route `{}` differs from request executor `{}`",
                executor_route.executor_ref(),
                request.executor_ref
            );
        }
        let verified = &admission.verified_subject;
        if resolution_output.root.raw_content_digest != verified.resolved.raw_content_digest {
            bail!(
                "sealed resolution root digest does not match admitted subject: resolution={}, admitted={}",
                resolution_output.root.raw_content_digest,
                verified.resolved.raw_content_digest
            );
        }
        let observed_effective_digest = resolution_output.effective_definition_digest()?;
        if observed_effective_digest != effective_definition_digest {
            bail!(
                "sealed effective definition digest does not match its resolution: expected={}, observed={}",
                effective_definition_digest,
                observed_effective_digest
            );
        }
        if verified.resolved.subject_resolution_authority
            != *admission.resolution_closure.subject_authority()
        {
            bail!("sealed resolved-item authority differs from its admitted resolution closure");
        }
        Ok(Self {
            schema_version: SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION,
            kind: request.kind.clone(),
            item_ref: request.item_ref.clone(),
            executor_ref: request.executor_ref.clone(),
            executor_route,
            runtime_ref,
            launch_mode: request.launch_mode.clone(),
            current_site_id: request.current_site_id.clone(),
            origin_site_id: request.origin_site_id.clone(),
            handler_context: handler_context.cloned(),
            admitted_operator_authority: None,
            target_site_id: request.target_site_id.clone(),
            requested_by: request.requested_by.clone(),
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
            parameters: request.parameters.clone(),
            ref_bindings: request.ref_bindings.clone(),
            resolved_ref_bindings,
            verified_subject: SealedResolvedItem::capture(
                &verified.resolved,
                admission
                    .resolution_closure
                    .verified_attestation()
                    .map(|attestation| attestation.source_bytes()),
            )?,
            verified_signer_fingerprint: verified.signer.as_ref().map(|value| value.0.clone()),
            verified_trust_class: verified.trust_class,
            verified_pinned_version: verified.pinned_version.clone(),
            resolution_output,
            effective_definition_digest,
            planning_principal: SealedPrincipal::from(&admission.plan_context.requested_by),
            project_context: admission.plan_context.project_context.clone(),
            project_authority: admission.project_authority().clone(),
            project_binding_subject_authority: admission
                .project_binding
                .subject_resolution_authority()
                .clone(),
            resolution_subject_authority: admission.resolution_closure.subject_authority().clone(),
            execution_hints: admission.plan_context.execution_hints.values.clone(),
            validate_only: admission.plan_context.validate_only,
            resolved_history_policy: admission.resolved_history_policy.clone(),
            resolved_result_policy: admission.resolved_result_policy.clone(),
            captured_history_policy: admission.captured_history_policy.clone(),
        })
    }

    pub fn runtime_ref(&self) -> &str {
        &self.runtime_ref
    }

    pub fn executor_ref(&self) -> &str {
        &self.executor_ref
    }

    /// Return the credential selector from a portable worker invocation.
    /// The complete sealed parameter object remains opaque to the handoff
    /// protocol. Only this generic selector is interpreted and rebound; every
    /// other worker-defined parameter is preserved byte-for-byte.
    pub fn worker_credential_profile_id(&self) -> Result<&str> {
        let parameters = self
            .parameters
            .as_object()
            .context("portable worker parameters must be an object")?;
        let profile = parameters
            .get("credential_profile_id")
            .and_then(Value::as_str)
            .context("portable worker credential_profile_id must be a string")?;
        if profile.is_empty() || profile.trim() != profile {
            bail!("portable worker credential_profile_id is not canonical");
        }
        Ok(profile)
    }

    /// Prove the capsule invocation is owned by the exact local principal and
    /// source-site coordinates named by a worker-placement protocol.
    pub fn validate_worker_handoff_source(
        &self,
        expected_owner: &str,
        expected_source_site: &str,
        expected_origin_site: &str,
    ) -> Result<&str> {
        let principal = self.planning_principal.restore()?;
        let EffectivePrincipal::Local(principal) = principal else {
            unreachable!("delegated principal restoration is refused")
        };
        if self.requested_by.as_deref() != Some(principal.fingerprint.as_str())
            || principal.fingerprint != expected_owner
            || self.current_site_id != expected_source_site
            || self.origin_site_id != expected_origin_site
        {
            bail!("portable worker source capsule contradicts owner or site authority");
        }
        self.worker_credential_profile_id()
    }

    pub fn item_ref(&self) -> &str {
        &self.item_ref
    }

    /// Verify and return only the exact admitted root subject. This is safe for
    /// definition inspection because no mutable resolution or operational
    /// launch metadata is consulted.
    pub fn admitted_program_subject(&self) -> Result<AdmittedProgramSubject> {
        if self.item_ref != self.verified_subject.canonical_ref {
            bail!("sealed request root identity differs from its admitted subject");
        }
        if self.resolution_output.root.resolved_ref != self.item_ref
            || self.resolution_output.root.raw_content_digest
                != self.verified_subject.raw_content_digest
        {
            bail!("sealed resolution root differs from its admitted subject");
        }
        self.verified_subject.exact_subject()
    }

    pub fn effective_definition_digest(
        &self,
    ) -> &ryeos_engine::resolution::EffectiveDefinitionDigest {
        &self.effective_definition_digest
    }

    /// Verify and expose the immutable effective resolution for sanitized
    /// definition projection. This never re-resolves current content.
    pub fn admitted_effective_resolution(
        &self,
    ) -> Result<&ryeos_engine::resolution::ResolutionOutput> {
        self.admitted_program_subject()?;
        let observed = self.resolution_output.effective_definition_digest()?;
        if observed != self.effective_definition_digest {
            bail!(
                "sealed effective definition digest mismatch: persisted={}, observed={}",
                self.effective_definition_digest,
                observed
            );
        }
        Ok(&self.resolution_output)
    }

    pub fn project_context(&self) -> &ProjectContext {
        &self.project_context
    }

    /// Stable portable program closure shared by continuation segments and
    /// independently admitted nodes. The state-object projector owns the
    /// exhaustive field classification so a new sealed field cannot silently
    /// enter or escape program identity. It also replaces live diagnostic
    /// paths with the retained resolution and admitted-subject projections.
    pub fn admitted_program_value(&self) -> Result<Value> {
        let invocation =
            serde_json::to_value(self).context("serialize admitted root invocation")?;
        ryeos_state::objects::admitted_launch_capsule::project_sealed_root_exact_program(
            &invocation,
        )
    }

    pub fn admitted_program_hash(&self) -> Result<String> {
        let canonical = lillux::canonical_json(&self.admitted_program_value()?)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    /// Prove that the operational resume ledger describes this exact sealed
    /// invocation. The capsule stores both representations deliberately: the
    /// sealed request is immutable execution authority, while `ResumeContext`
    /// is the daemon's reconstruction index. Neither may silently become a
    /// partial or independently editable copy of the other.
    pub(crate) fn validate_invocation_against_resume(
        &self,
        resume: &crate::launch_metadata::ResumeContext,
    ) -> Result<()> {
        if let Some(field) = self.invocation_resume_mismatch(resume) {
            bail!(
                "sealed invocation and resume authority disagree for {} ({field})",
                resume.item_ref,
            );
        }
        Ok(())
    }

    /// Name the first contradictory field without returning either side's
    /// value. Invocation data may contain project paths or private parameters;
    /// a refusal must identify the broken authority edge without disclosing
    /// those values into a runtime-visible diagnostic.
    fn invocation_resume_mismatch(
        &self,
        resume: &crate::launch_metadata::ResumeContext,
    ) -> Option<&'static str> {
        if self.kind != resume.kind {
            Some("kind")
        } else if self.item_ref != resume.item_ref {
            Some("item_ref")
        } else if self.ref_bindings != resume.ref_bindings {
            Some("ref_bindings")
        } else if self.launch_mode != resume.launch_mode {
            Some("launch_mode")
        } else if self.parameters != resume.parameters {
            Some("parameters")
        } else if self.current_site_id != resume.current_site_id {
            Some("current_site_id")
        } else if self.origin_site_id != resume.origin_site_id {
            Some("origin_site_id")
        } else if self.requested_by.as_deref() != Some(resume.principal_identifier()) {
            Some("requested_by")
        } else if self.planning_principal != SealedPrincipal::from(&resume.requested_by) {
            Some("planning_principal")
        } else if self.project_context != resume.project_context {
            Some("project_context")
        } else if self.project_authority != resume.project_authority {
            Some("project_authority")
        } else if resume.executor_ref.as_deref() != Some(self.executor_ref()) {
            Some("executor_ref")
        } else if resume.runtime_ref.as_deref() != Some(self.runtime_ref()) {
            Some("runtime_ref")
        } else if self.execution_hints != resume.execution_hints.values {
            Some("execution_hints")
        } else {
            None
        }
    }

    /// Rebind only the invocation envelope of an exact admitted program for a
    /// machine continuation segment. Program bytes, composed resolution,
    /// runtime identity, trust facts, and ingress authority remain
    /// byte-for-byte inherited.
    pub fn for_continuation_invocation(
        &self,
        resume: &crate::launch_metadata::ResumeContext,
    ) -> Result<Self> {
        if self.requested_by.as_deref() != Some(resume.principal_identifier())
            || self.planning_principal != SealedPrincipal::from(&resume.requested_by)
        {
            bail!(
                "machine continuation cannot replace the admitted execution principal for {}",
                resume.item_ref
            );
        }
        self.rebind_continuation_invocation(resume, self.handler_context.clone())
    }

    /// Rebind an operator continuation to the exact handler authority that
    /// authenticated the new stimulus. This is the sole ordinary continuation
    /// path allowed to replace the predecessor's execution principal.
    pub fn for_operator_continuation_invocation(
        &self,
        resume: &crate::launch_metadata::ResumeContext,
        handler_context: &crate::handler_context::HandlerContext,
    ) -> Result<Self> {
        self.rebind_continuation_invocation(resume, Some(handler_context.clone()))
    }

    fn rebind_continuation_invocation(
        &self,
        resume: &crate::launch_metadata::ResumeContext,
        handler_context: Option<crate::handler_context::HandlerContext>,
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
        successor.handler_context = handler_context;
        successor.project_context = resume.project_context.clone();
        successor.project_authority = resume.project_authority.clone();
        successor.project_binding_subject_authority = continued_binding_subject_authority(
            &self.project_binding_subject_authority,
            &resume.project_authority,
        )?;
        successor.execution_hints = resume.execution_hints.values.clone();
        successor.validate_handler_context()?;
        successor
            .validate_invocation_against_resume(resume)
            .context("continuation invocation rebind validation")?;
        Ok(successor)
    }

    pub fn handler_context(&self) -> Option<&crate::handler_context::HandlerContext> {
        self.handler_context.as_ref()
    }

    pub fn admitted_operator_authority(
        &self,
    ) -> Option<&crate::operator_authority::AdmittedOperatorAuthority> {
        self.admitted_operator_authority.as_ref()
    }

    /// Recheck a target-node grant before recovered private execution resumes.
    /// Absence denotes an ordinary locally admitted source execution; every
    /// remotely adopted capsule is required to carry this authority by the
    /// cross-site transition validator.
    pub fn validate_current_operator_authority(
        &self,
        state: &crate::state::AppState,
    ) -> Result<()> {
        let Some(expected) = self.admitted_operator_authority.as_ref() else {
            return Ok(());
        };
        let current = crate::operator_authority::retained_admitted_operator_authority(
            state,
            &expected.owner_principal,
            &expected.origin_site_id,
        )?;
        if &current != expected {
            bail!("remotely adopted execution operator grant was replaced");
        }
        Ok(())
    }

    fn validate_handler_context(&self) -> Result<()> {
        if let Some(authority) = self.admitted_operator_authority.as_ref() {
            authority.validate()?;
            if self.handler_context.as_ref() != Some(&authority.handler_context())
                || self.requested_by.as_deref() != Some(authority.owner_principal.as_str())
                || self.origin_site_id != authority.origin_site_id
                || self.planning_principal
                    != (SealedPrincipal::Local {
                        fingerprint: authority.owner_principal.clone(),
                        scopes: authority.scopes.clone(),
                    })
            {
                bail!("sealed target operator authority contradicts its execution principal");
            }
        }
        let Some(context) = self.handler_context.as_ref() else {
            return Ok(());
        };
        let (principal, scopes) = match &self.planning_principal {
            SealedPrincipal::Local {
                fingerprint,
                scopes,
            } => (fingerprint.as_str(), scopes.as_slice()),
            SealedPrincipal::Delegated {
                caller_fingerprint,
                delegated_scopes,
                ..
            } => (caller_fingerprint.as_str(), delegated_scopes.as_slice()),
        };
        context.validate_execution_authority(
            principal,
            scopes,
            &self.current_site_id,
            &self.origin_site_id,
        )
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
        let source_content = b"{}";
        let content_hash = lillux::sha256_hex(source_content);
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
                node_policy: NodeHistoryPolicyProvenance::test_policy(),
            },
        };
        let captured_history_policy = ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
            canonical_item_ref: canonical_item_ref.clone(),
            item_content_hash: content_hash.clone(),
            item_signer_fingerprint: None,
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Unsigned,
            kind_schema_content_hash: kind_schema_content_hash.clone(),
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(
                ),
            },
        };
        let resolved_result_policy = ryeos_engine::history_policy::ResolvedThreadResultPolicy {
            retention: ryeos_engine::history_policy::ThreadResultRetention::Full,
            canonical_item_ref: canonical_item_ref.clone(),
            item_content_hash: content_hash.clone(),
            item_signer_fingerprint: None,
            item_trust_class: TrustClass::Unsigned,
            kind_schema_content_hash: kind_schema_content_hash.clone(),
            source: ryeos_engine::history_policy::ResultPolicyProvenance::DefaultFull,
        };
        let mut fixture = Self {
            schema_version: SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION,
            kind: "graph_run".to_string(),
            item_ref: canonical_item_ref.clone(),
            executor_ref: "native:storage-fixture".to_string(),
            executor_route: AdmittedExecutorRoute::ManagedRuntimeForKind {
                runtime_ref: "runtime:storage-fixture".to_string(),
                runtime_content_hash: "33".repeat(32),
                runtime_signer_fingerprint: "44".repeat(32),
                serves_kind: "graph".to_string(),
                executor_ref: "native:storage-fixture".to_string(),
            },
            runtime_ref: "runtime:storage-fixture".to_string(),
            launch_mode: "detached".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            handler_context: None,
            admitted_operator_authority: None,
            target_site_id: None,
            requested_by: Some("session:test".to_string()),
            usage_subject: None,
            usage_subject_asserted_by: None,
            parameters: json!({}),
            ref_bindings: BTreeMap::new(),
            resolved_ref_bindings: BTreeMap::new(),
            verified_subject: SealedResolvedItem {
                canonical_ref: canonical_item_ref.clone(),
                kind: "graph".to_string(),
                source_path: PathBuf::from("/synthetic/storage-fixture.yaml"),
                source_space: ItemSpace::Project,
                source_root: ItemSourceRoot::Project,
                resolved_from: "storage_test_fixture".to_string(),
                shadowed: Vec::new(),
                probed_absent: Vec::new(),
                materialized_project_root: None,
                subject_resolution_authority:
                    ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
                raw_content_digest: content_hash.clone(),
                source_content_b64: base64::engine::general_purpose::STANDARD
                    .encode(source_content),
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
                    source_root: ItemSourceRoot::Project,
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
            effective_definition_digest:
                ryeos_engine::resolution::EffectiveDefinitionDigest::parse("0".repeat(64))
                    .expect("valid synthetic digest"),
            planning_principal: SealedPrincipal::Local {
                fingerprint: "session:test".to_string(),
                scopes: Vec::new(),
            },
            project_context: ProjectContext::None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            project_binding_subject_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
            resolution_subject_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
            execution_hints: HashMap::new(),
            validate_only: false,
            resolved_history_policy,
            resolved_result_policy,
            captured_history_policy,
        };
        fixture.effective_definition_digest = fixture
            .resolution_output
            .effective_definition_digest()
            .expect("synthetic resolution has a canonical effective digest");
        fixture
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
        fixture.project_binding_subject_authority =
            storage_fixture_subject_authority_from_project_authority(&project_authority);
        fixture.resolution_subject_authority = fixture.project_binding_subject_authority.clone();
        fixture.verified_subject.subject_resolution_authority =
            fixture.resolution_subject_authority.clone();
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
        self.validate_handler_context()
            .context("validate sealed root handler authority")?;
        let requested_by = self.planning_principal.restore()?;
        let observed_effective_digest = self.resolution_output.effective_definition_digest()?;
        if observed_effective_digest != self.effective_definition_digest {
            bail!(
                "sealed effective definition digest mismatch: persisted={}, observed={}",
                self.effective_definition_digest,
                observed_effective_digest
            );
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
            requested_by,
            project_context: self.project_context.clone(),
            subject_resolution_authority: self.project_binding_subject_authority.clone(),
            current_site_id: self.current_site_id.clone(),
            origin_site_id: self.origin_site_id.clone(),
            execution_hints: ExecutionHints {
                values: self.execution_hints.clone(),
            },
            validate_only: false,
        };
        let project_binding = AdmittedProjectBinding::restore(
            engine,
            &plan_context,
            self.project_authority.clone(),
            self.project_binding_subject_authority.clone(),
        )?;
        let admission = RootExecutionAdmission {
            verified_subject,
            resolution_closure: std::sync::Arc::new(
                crate::resolution_cache::ResolvedClosure::restored(
                    self.resolution_output.clone(),
                    self.resolution_subject_authority.clone(),
                    match (&self.resolution_subject_authority, &self.project_context) {
                        (ryeos_engine::contracts::SubjectResolutionAuthority::Projectless, _) => {
                            None
                        }
                        (_, ProjectContext::LocalPath { path }) => Some(path.clone()),
                        (
                            _,
                            ProjectContext::None
                            | ProjectContext::SnapshotHash { .. }
                            | ProjectContext::ProjectRef { .. },
                        ) => None,
                    },
                )?,
            ),
            plan_context: plan_context.clone(),
            thread_profile: self.kind.clone(),
            usage_subject: self.usage_subject.clone(),
            usage_subject_asserted_by: self.usage_subject_asserted_by.clone(),
            ref_bindings: self.ref_bindings.clone(),
            resolved_history_policy: self.resolved_history_policy.clone(),
            resolved_result_policy: self.resolved_result_policy.clone(),
            captured_history_policy: self.captured_history_policy.clone(),
            project_binding,
            admitted_request_snapshot: None,
            selected_executor_route: None,
        };
        if self.executor_route.executor_ref() != self.executor_ref {
            bail!(
                "sealed root executor `{}` differs from its admitted executor route `{}`",
                self.executor_ref,
                self.executor_route.executor_ref()
            );
        }
        if self
            .executor_route
            .runtime_ref()
            .is_some_and(|runtime_ref| runtime_ref != self.runtime_ref)
        {
            bail!(
                "sealed root runtime `{}` differs from its admitted executor route",
                self.runtime_ref
            );
        }
        let mut admission = admission;
        admission.selected_executor_route = Some(self.executor_route.clone());
        // The sealed request already crossed fresh admission. Recovery proves
        // its internal closure and applies explicit current trust/isolation
        // narrowing; it never re-enters today's kind/runtime registries.
        admission.validate()?;
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
        rebound_plan_context.subject_resolution_authority =
            provenance.subject_resolution_authority();
        let rebound_binding =
            AdmittedProjectBinding::from_provenance(engine, &rebound_plan_context, provenance)?;
        let rebound_materialized_project_root = match provenance {
            crate::execution_provenance::ExecutionProvenance::Projectless { .. } => None,
            crate::execution_provenance::ExecutionProvenance::RootLiveProject { .. }
            | crate::execution_provenance::ExecutionProvenance::ChildLiveProject { .. }
            | crate::execution_provenance::ExecutionProvenance::RootPinnedGeneration { .. }
            | crate::execution_provenance::ExecutionProvenance::ChildPinnedGeneration { .. } => {
                Some(provenance.effective_path().to_path_buf())
            }
        };
        request.resolved_item.materialized_project_root = rebound_materialized_project_root.clone();
        {
            let admission = request
                .root_admission
                .as_mut()
                .ok_or_else(|| anyhow!("restored sealed root has no admission"))?;
            admission.plan_context = rebound_plan_context.clone();
            admission.project_binding = rebound_binding;
            admission
                .verified_subject
                .resolved
                .materialized_project_root = rebound_materialized_project_root;
            // Complete request snapshots include parser/kind registry
            // semantics and belong only to fresh admission. Recovered paths
            // use the sealed execution closure plus a trust-only current
            // policy view derived from this exact project materialization.
            admission.admitted_request_snapshot = None;
            admission.validate()?;
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
        let sealed = Self::decode_from_admitted_capsule(capsule)?;
        sealed.restore_for_reconstructed_provenance(engine, capsule_root, provenance)
    }

    /// Rebind an admitted worker invocation directly from capsule authority
    /// onto one target placement. Source operational `RuntimeLaunchMetadata`
    /// and its path-bearing `ResumeContext` never cross the site boundary.
    pub fn for_remote_worker_adoption_from_capsule(
        capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
        rebind: &crate::worker_handoff::RemoteResumeContextRebind,
    ) -> Result<(crate::launch_metadata::ResumeContext, Self)> {
        capsule.validate_durable_handoff_eligibility()?;
        let source = Self::decode_from_admitted_capsule(capsule)?;
        let requested_by = source.planning_principal.restore()?;
        let owner = match &requested_by {
            EffectivePrincipal::Local(principal) => principal.fingerprint.as_str(),
            EffectivePrincipal::Delegated(_) => unreachable!("delegated restore is refused"),
        };
        if source.requested_by.as_deref() != Some(owner)
            || source.current_site_id != rebind.source_site_id
            || source.origin_site_id.is_empty()
            || rebind.source_site_id == rebind.target_site_id
            || rebind.credential_reservation.owner_principal != owner
        {
            bail!("remote worker source capsule contradicts placement coordinates");
        }
        rebind.credential_reservation.validate()?;
        rebind.target_operator_authority.validate()?;
        if rebind.target_operator_authority.owner_principal != owner
            || rebind.target_operator_authority.origin_site_id != source.origin_site_id
        {
            bail!("target operator grant differs from the source execution owner or origin");
        }
        rebind
            .target_operator_authority
            .require_covers(&capsule.effective_caps)?;
        if let Some(parent_caps) = capsule.parent_delegation_caps.as_ref() {
            rebind
                .target_operator_authority
                .require_covers(parent_caps)?;
        }
        rebind.validate_target_project_authority()?;
        if source.worker_credential_profile_id()? != rebind.source_credential_profile_id {
            bail!("source credential profile differs from the capsule invocation");
        }
        let parameters = crate::worker_handoff::rebind_credential_profile_parameter(
            &source.parameters,
            &rebind.source_credential_profile_id,
            &rebind.credential_reservation.profile_id,
        )?;
        let requested_by = EffectivePrincipal::Local(Principal {
            fingerprint: rebind.target_operator_authority.owner_principal.clone(),
            scopes: rebind.target_operator_authority.scopes.clone(),
        });
        let target_resume = crate::launch_metadata::ResumeContext {
            kind: source.kind.clone(),
            item_ref: source.item_ref.clone(),
            ref_bindings: source.ref_bindings.clone(),
            launch_mode: source.launch_mode.clone(),
            parameters,
            project_context: rebind.target_project_context.clone(),
            project_authority: rebind.target_project_authority.clone(),
            lifecycle_authority: capsule.lifecycle_authority,
            stable_project_identity: rebind.target_stable_project_identity.clone(),
            local_overlay_root: rebind.target_local_overlay_root.clone(),
            original_snapshot_hash: rebind.target_original_snapshot_hash.clone(),
            original_pushed_head_ref: rebind.target_original_pushed_head_ref.clone(),
            state_root: rebind.target_state_root.clone(),
            current_site_id: rebind.target_site_id.clone(),
            origin_site_id: source.origin_site_id.clone(),
            requested_by,
            execution_hints: ExecutionHints {
                values: source.execution_hints.clone(),
            },
            effective_caps: capsule.effective_caps.clone(),
            parent_delegation_caps: capsule.parent_delegation_caps.clone(),
            executor_ref: Some(capsule.executor_ref.clone()),
            runtime_ref: Some(capsule.runtime_ref.clone()),
        };
        target_resume.authoritative_project_identity()?;

        let mut target = source.clone();
        target.current_site_id = rebind.target_site_id.clone();
        target.target_site_id = Some(rebind.target_site_id.clone());
        target.handler_context = Some(rebind.target_operator_authority.handler_context());
        target.admitted_operator_authority = Some(rebind.target_operator_authority.clone());
        target.planning_principal = SealedPrincipal::from(&target_resume.requested_by);
        target.parameters = target_resume.parameters.clone();
        target.project_context = target_resume.project_context.clone();
        target.project_authority = target_resume.project_authority.clone();
        target.project_binding_subject_authority = continued_binding_subject_authority(
            &source.project_binding_subject_authority,
            &target_resume.project_authority,
        )?;
        target
            .validate_invocation_against_resume(&target_resume)
            .context("target invocation ledger validation after remote adoption rebind")?;
        if target.admitted_program_value()? != capsule.exact_program
            || target.admitted_program_hash()? != capsule.exact_program_hash
        {
            bail!("remote worker invocation rebind changed immutable admitted program identity");
        }
        Ok((target_resume, target))
    }

    pub(crate) fn handoff_principal_identifier(&self) -> Result<&str> {
        let principal = match &self.planning_principal {
            SealedPrincipal::Local { fingerprint, .. } => fingerprint.as_str(),
            SealedPrincipal::Delegated { .. } => {
                bail!("remote worker handoff refuses a delegated source principal")
            }
        };
        if self.requested_by.as_deref() != Some(principal) {
            bail!("remote worker source capsule has contradictory principal authority");
        }
        Ok(principal)
    }

    pub(crate) fn handoff_current_site_id(&self) -> &str {
        &self.current_site_id
    }

    pub(crate) fn handoff_origin_site_id(&self) -> &str {
        &self.origin_site_id
    }

    fn validate_executor_route_against_capsule(
        &self,
        capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    ) -> Result<()> {
        if self.executor_route.executor_ref() != capsule.executor_ref {
            bail!("sealed executor route contradicts admitted capsule executor identity");
        }
        match (&self.executor_route, &capsule.artifact_identity) {
            (
                AdmittedExecutorRoute::ManagedRuntimeForKind {
                    runtime_ref,
                    runtime_content_hash,
                    runtime_signer_fingerprint,
                    executor_ref,
                    ..
                },
                ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
                    runtime_ref: artifact_runtime_ref,
                    runtime_content_hash: artifact_runtime_content_hash,
                    runtime_signer_fingerprint: artifact_runtime_signer,
                    executor_ref: artifact_executor_ref,
                    ..
                },
            ) => {
                if runtime_ref != artifact_runtime_ref
                    || runtime_content_hash != artifact_runtime_content_hash
                    || runtime_signer_fingerprint != artifact_runtime_signer
                    || executor_ref != artifact_executor_ref
                {
                    bail!(
                        "sealed managed executor route contradicts admitted capsule artifact identity"
                    );
                }
            }
            (
                AdmittedExecutorRoute::ManagedRuntimeForKind { .. },
                ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor { .. },
            ) => {
                bail!("sealed managed executor route has a direct-item capsule artifact identity");
            }
            (
                AdmittedExecutorRoute::RuntimeDescriptorExecutor {
                    runtime_ref,
                    runtime_content_hash,
                    runtime_signer_fingerprint,
                    ..
                },
                ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                    runtime_identity,
                    ..
                },
            ) => {
                if runtime_ref != &runtime_identity.runtime_ref
                    || runtime_content_hash != &runtime_identity.runtime_content_hash
                    || runtime_signer_fingerprint != &runtime_identity.runtime_signer_fingerprint
                {
                    bail!(
                        "sealed runtime-descriptor route contradicts admitted direct runtime identity"
                    );
                }
            }
            (
                AdmittedExecutorRoute::RuntimeDescriptorExecutor { .. },
                ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime { .. },
            ) => {
                bail!(
                    "sealed runtime-descriptor executor route has a managed capsule artifact identity"
                );
            }
            (
                AdmittedExecutorRoute::RootExecutorChain { .. }
                | AdmittedExecutorRoute::DirectNativeExecutor { .. },
                ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                    runtime_identity,
                    ..
                },
            ) => {
                if self.runtime_ref != runtime_identity.runtime_ref {
                    bail!(
                        "sealed direct executor route contradicts admitted direct runtime identity"
                    );
                }
            }
            (
                AdmittedExecutorRoute::RootExecutorChain { .. }
                | AdmittedExecutorRoute::DirectNativeExecutor { .. },
                ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime { .. },
            ) => {
                bail!("sealed direct executor route has a managed capsule artifact identity");
            }
        }
        Ok(())
    }

    pub fn project_authority(&self) -> &ryeos_state::objects::ExecutionProjectAuthority {
        &self.project_authority
    }
}

fn continued_binding_subject_authority(
    previous: &ryeos_engine::contracts::SubjectResolutionAuthority,
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> Result<ryeos_engine::contracts::SubjectResolutionAuthority> {
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use ryeos_state::objects::{ExecutionProjectAuthority, PinnedProjectRealization};

    match project_authority {
        ExecutionProjectAuthority::Projectless { .. } => {
            if previous != &SubjectResolutionAuthority::Projectless {
                bail!("projectless continuation must inherit projectless subject authority");
            }
            Ok(SubjectResolutionAuthority::Projectless)
        }
        ExecutionProjectAuthority::LiveProject { .. } => {
            if previous != &SubjectResolutionAuthority::LiveFs {
                bail!("live continuation must inherit live subject authority");
            }
            Ok(SubjectResolutionAuthority::LiveFs)
        }
        ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash,
            realization: PinnedProjectRealization::ReadOnly,
            ..
        } => {
            let expected = SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: snapshot_hash.clone(),
            };
            if previous != &expected {
                bail!("read-only pinned continuation must inherit the exact pinned generation");
            }
            Ok(expected)
        }
        ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash: authority_base,
            snapshot_hash,
            realization: PinnedProjectRealization::Cow { .. },
            ..
        } => {
            let base_snapshot_hash = match previous {
                SubjectResolutionAuthority::CowWorkspace {
                    base_snapshot_hash, ..
                } if base_snapshot_hash == authority_base => base_snapshot_hash.clone(),
                SubjectResolutionAuthority::CowWorkspace { .. } => {
                    bail!("pinned COW continuation changed its original base generation")
                }
                SubjectResolutionAuthority::Projectless
                | SubjectResolutionAuthority::LiveFs
                | SubjectResolutionAuthority::PinnedGeneration { .. } => {
                    bail!("pinned COW continuation must inherit an existing COW subject authority")
                }
            };
            Ok(SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash,
                current_operational_generation: snapshot_hash.clone(),
            })
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
fn storage_fixture_subject_authority_from_project_authority(
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> ryeos_engine::contracts::SubjectResolutionAuthority {
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use ryeos_state::objects::{ExecutionProjectAuthority, PinnedProjectRealization};

    match project_authority {
        ExecutionProjectAuthority::Projectless { .. } => SubjectResolutionAuthority::Projectless,
        ExecutionProjectAuthority::LiveProject { .. } => SubjectResolutionAuthority::LiveFs,
        ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash,
            realization: PinnedProjectRealization::ReadOnly,
            ..
        } => SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: snapshot_hash.clone(),
        },
        ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            snapshot_hash,
            realization: PinnedProjectRealization::Cow { .. },
            ..
        } => SubjectResolutionAuthority::CowWorkspace {
            base_snapshot_hash: base_snapshot_hash.clone(),
            current_operational_generation: snapshot_hash.clone(),
        },
    }
}

#[cfg(test)]
mod authority_tests {
    use super::*;

    fn empty_engine() -> Arc<Engine> {
        Arc::new(Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
            Vec::new(),
        ))
    }

    #[test]
    fn sealed_request_refuses_delegated_principal_recovery() {
        let mut value =
            serde_json::to_value(SealedRootExecutionRequest::storage_test_fixture()).unwrap();
        value["planning_principal"] = json!({
            "type": "delegated",
            "protocol_version": "1",
            "delegation_id": "delegation:test",
            "caller_fingerprint": "caller:test",
            "origin_site_id": "site:origin",
            "audience_site_id": "site:target",
            "delegated_scopes": ["threads.execute"],
            "budget_lease_id": null,
            "request_hash": "request:test",
            "idempotency_key": "idempotency:test",
            "issued_at": "2026-08-10T00:00:00Z",
            "expires_at": "2026-08-10T00:05:00Z",
            "non_redelegable": true,
            "origin_signature": "unverifiable-signature"
        });
        let sealed: SealedRootExecutionRequest = serde_json::from_value(value).unwrap();
        let capsule_root = tempfile::tempdir().unwrap();

        let error = sealed
            .restore(&empty_engine(), capsule_root.path())
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "sealed request principal restore refused: delegated principals have no acceptance path and their origin signature is unverifiable"
        );
    }

    #[test]
    fn sealed_request_local_principal_round_trips_unchanged() {
        let fixture = SealedRootExecutionRequest::storage_test_fixture();
        let value = serde_json::to_value(&fixture).unwrap();
        let sealed: SealedRootExecutionRequest = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(
            serde_json::to_value(&sealed.planning_principal).unwrap(),
            value["planning_principal"]
        );
        assert_eq!(
            sealed.planning_principal.restore().unwrap(),
            EffectivePrincipal::Local(Principal {
                fingerprint: "session:test".to_string(),
                scopes: Vec::new(),
            })
        );
    }

    #[test]
    fn sealed_remote_operator_handler_authority_round_trips_exactly() {
        let mut fixture = SealedRootExecutionRequest::storage_test_fixture();
        fixture.current_site_id = "site:target".to_string();
        fixture.origin_site_id = "site:source".to_string();
        fixture.handler_context = Some(crate::handler_context::HandlerContext::new_with_authority(
            "session:test".to_string(),
            Vec::new(),
            true,
            Some(crate::identity::AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:source".to_string()),
        ));
        fixture.validate_handler_context().unwrap();

        let round_trip: SealedRootExecutionRequest =
            serde_json::from_value(serde_json::to_value(&fixture).unwrap()).unwrap();
        round_trip.validate_handler_context().unwrap();
        assert_eq!(round_trip.handler_context, fixture.handler_context);
    }

    fn cow_authority(base: &str, current: &str) -> ryeos_state::objects::ExecutionProjectAuthority {
        let authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "test-project".to_string(),
            None,
            base.to_string(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        authority
            .transition_operational_generation(
                ryeos_state::objects::OperationalProjectAuthorityTransition::AdvancePinnedCowContinuation {
                    result_snapshot_hash: current,
                },
            )
            .unwrap()
    }

    fn continuation_resume(
        project_path: &str,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> crate::launch_metadata::ResumeContext {
        crate::launch_metadata::ResumeContext {
            kind: "graph_run".to_string(),
            item_ref: "graph:test/storage-fixture".to_string(),
            ref_bindings: BTreeMap::new(),
            launch_mode: "detached".to_string(),
            parameters: json!({"continuation": true}),
            project_context: ProjectContext::LocalPath {
                path: PathBuf::from(project_path),
            },
            project_authority,
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity: None,
            local_overlay_root: None,
            original_snapshot_hash: None,
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "session:test".to_string(),
                scopes: Vec::new(),
            }),
            execution_hints: ExecutionHints::default(),
            effective_caps: Vec::new(),
            parent_delegation_caps: None,
            executor_ref: Some("native:storage-fixture".to_string()),
            runtime_ref: Some("runtime:storage-fixture".to_string()),
        }
    }

    #[test]
    fn operator_continuation_rebinds_exact_authenticated_principal() {
        let fixture = SealedRootExecutionRequest::storage_test_fixture();
        let mut resume = continuation_resume(
            "/unused",
            ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        );
        resume.project_context = ProjectContext::None;
        resume.requested_by = EffectivePrincipal::Local(Principal {
            fingerprint: "session:operator".to_string(),
            scopes: vec!["threads.execute".to_string()],
        });
        let handler = crate::handler_context::HandlerContext::new_with_authority(
            "session:operator".to_string(),
            vec!["threads.execute".to_string()],
            true,
            Some(crate::identity::AuthorizedKeyPrincipalClass::LocalClient),
            None,
        );

        let rebound = fixture
            .for_operator_continuation_invocation(&resume, &handler)
            .unwrap();
        assert_eq!(rebound.handler_context(), Some(&handler));
        assert_eq!(rebound.requested_by.as_deref(), Some("session:operator"));
        assert_eq!(
            rebound.planning_principal,
            SealedPrincipal::Local {
                fingerprint: "session:operator".to_string(),
                scopes: vec!["threads.execute".to_string()],
            }
        );
    }

    #[test]
    fn machine_continuation_cannot_replace_execution_principal() {
        let fixture = SealedRootExecutionRequest::storage_test_fixture();
        let mut resume = continuation_resume(
            "/unused",
            ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        );
        resume.project_context = ProjectContext::None;
        resume.requested_by = EffectivePrincipal::Local(Principal {
            fingerprint: "session:other".to_string(),
            scopes: Vec::new(),
        });

        assert!(
            fixture
                .for_continuation_invocation(&resume)
                .unwrap_err()
                .to_string()
                .contains("cannot replace the admitted execution principal")
        );
    }

    #[test]
    fn worker_handoff_source_requires_exact_selector_principal_and_sites() {
        let mut fixture = SealedRootExecutionRequest::storage_test_fixture();
        fixture.current_site_id = "site:source".to_string();
        fixture.origin_site_id = "site:origin".to_string();
        fixture.requested_by = Some("session:test".to_string());
        fixture.planning_principal = SealedPrincipal::Local {
            fingerprint: "session:test".to_string(),
            scopes: vec!["project.read".to_string()],
        };
        fixture.parameters = json!({
            "credential_profile_id":"source-profile",
            "worker_defined":{"mode":"review","limit":7}
        });
        assert_eq!(
            fixture
                .validate_worker_handoff_source("session:test", "site:source", "site:origin")
                .unwrap(),
            "source-profile"
        );

        for parameters in [
            json!({}),
            json!({"credential_profile_id": 7}),
            json!({"credential_profile_id": ""}),
            json!({"credential_profile_id": " padded "}),
        ] {
            let mut invalid = fixture.clone();
            invalid.parameters = parameters;
            assert!(
                invalid
                    .validate_worker_handoff_source("session:test", "site:source", "site:origin")
                    .is_err()
            );
        }
        for (owner, source_site, origin_site) in [
            ("session:other", "site:source", "site:origin"),
            ("session:test", "site:other", "site:origin"),
            ("session:test", "site:source", "site:other"),
        ] {
            assert!(
                fixture
                    .validate_worker_handoff_source(owner, source_site, origin_site)
                    .is_err()
            );
        }

        let mut delegated = fixture;
        delegated.planning_principal = SealedPrincipal::Delegated {
            protocol_version: "v1".to_string(),
            delegation_id: "delegation:test".to_string(),
            caller_fingerprint: "a".repeat(64),
            origin_site_id: "site:origin".to_string(),
            audience_site_id: "site:source".to_string(),
            delegated_scopes: vec!["project.read".to_string()],
            budget_lease_id: None,
            request_hash: "b".repeat(64),
            idempotency_key: "handoff:test".to_string(),
            issued_at: "2026-01-01T00:00:00Z".to_string(),
            expires_at: "2026-01-01T01:00:00Z".to_string(),
            non_redelegable: true,
            origin_signature: "signature".to_string(),
        };
        assert!(
            delegated
                .validate_worker_handoff_source("session:test", "site:source", "site:origin")
                .unwrap_err()
                .to_string()
                .contains("delegated principals have no acceptance path")
        );
    }

    #[test]
    fn remote_worker_adoption_rebinds_exact_target_operator_authority() {
        use crate::worker_handoff::{CredentialGenerationReservation, RemoteResumeContextRebind};
        use ryeos_state::objects::{EnvironmentAuthority, PinnedProjectRealization};

        let base = "a".repeat(64);
        let target_generation = "b".repeat(64);
        let source_path = PathBuf::from("/source/project");
        let target_path = PathBuf::from("/target/project");
        let source_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "project:test".to_string(),
            Some(source_path.clone()),
            base.clone(),
            PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        let owner = format!("fp:{}", "e".repeat(64));
        let mut fixture = SealedRootExecutionRequest::storage_test_fixture_with_project_identity(
            ProjectContext::SnapshotHash { hash: base.clone() },
            source_authority.clone(),
        );
        fixture.current_site_id = "site:source".to_string();
        fixture.origin_site_id = "site:source".to_string();
        fixture.requested_by = Some(owner.clone());
        fixture.planning_principal = SealedPrincipal::Local {
            fingerprint: owner.clone(),
            scopes: vec!["*".to_string()],
        };
        fixture.parameters = json!({
            "credential_profile_id":"source-profile",
            "worker_defined":{"mode":"review","limit":7}
        });
        fixture.handler_context = Some(crate::handler_context::HandlerContext::new_with_authority(
            owner.clone(),
            vec!["*".to_string()],
            true,
            Some(crate::identity::AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:source".to_string()),
        ));
        fixture.validate_handler_context().unwrap();

        let mut source_resume = continuation_resume("/source/project", source_authority.clone());
        source_resume.project_context = ProjectContext::SnapshotHash { hash: base.clone() };
        source_resume.stable_project_identity = Some(
            crate::launch_metadata::StableProjectIdentity::from_path(&source_path, "site:source")
                .unwrap(),
        );
        source_resume.original_snapshot_hash = Some(base.clone());
        source_resume.current_site_id = "site:source".to_string();
        source_resume.origin_site_id = "site:source".to_string();
        source_resume.requested_by = EffectivePrincipal::Local(Principal {
            fingerprint: owner.clone(),
            scopes: vec!["*".to_string()],
        });
        source_resume.parameters = fixture.parameters.clone();
        source_resume.effective_caps = vec!["ryeos.read.project.live".to_string()];
        source_resume.parent_delegation_caps = Some(vec![
            "ryeos.read.project.live".to_string(),
            "ryeos.write.project.live".to_string(),
        ]);
        let target_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "project:test".to_string(),
            Some(target_path.clone()),
            base.clone(),
            PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap()
        .transition_operational_generation(
            ryeos_state::objects::OperationalProjectAuthorityTransition::AdvancePinnedCowContinuation {
                result_snapshot_hash: &target_generation,
            },
        )
        .unwrap();
        let rebind = RemoteResumeContextRebind {
            source_site_id: "site:source".to_string(),
            target_site_id: "site:target".to_string(),
            target_project_context: ProjectContext::LocalPath {
                path: target_path.clone(),
            },
            target_project_authority: target_authority,
            target_stable_project_identity: Some(
                crate::launch_metadata::StableProjectIdentity::from_path(
                    &target_path,
                    "site:target",
                )
                .unwrap(),
            ),
            target_local_overlay_root: None,
            target_original_snapshot_hash: Some(target_generation.clone()),
            target_original_pushed_head_ref: None,
            target_state_root: None,
            source_credential_profile_id: "source-profile".to_string(),
            credential_reservation: CredentialGenerationReservation {
                profile_id: "target-profile".to_string(),
                owner_principal: owner.clone(),
                generation: 1,
                reservation_id: "reservation:test".to_string(),
                upstream_session_id: "upstream:test".to_string(),
                subject_contract_digest: "c".repeat(64),
                subject_digest: "d".repeat(64),
            },
            target_operator_authority: crate::operator_authority::AdmittedOperatorAuthority {
                owner_principal: owner,
                origin_site_id: "site:source".to_string(),
                principal_class: crate::identity::AuthorizedKeyPrincipalClass::RemoteOperator,
                grant_digest: "f".repeat(64),
                scopes: vec!["*".to_string()],
            },
        };
        let runtime_key = lillux::crypto::SigningKey::from_bytes(&[5; 32]);
        let runtime_descriptor_document =
            lillux::signature::sign_content("runtime fixture\n", &runtime_key, "#", None);
        let runtime_header = lillux::signature::parse_signature_line(
            runtime_descriptor_document.lines().next().unwrap(),
            "#",
            None,
        )
        .unwrap();
        let protocol_key = lillux::crypto::SigningKey::from_bytes(&[6; 32]);
        let protocol_descriptor_document =
            lillux::signature::sign_content("protocol fixture\n", &protocol_key, "#", None);
        let protocol_header = lillux::signature::parse_signature_line(
            protocol_descriptor_document.lines().next().unwrap(),
            "#",
            None,
        )
        .unwrap();
        let mut capsule_fixture = fixture;
        capsule_fixture.executor_route = AdmittedExecutorRoute::ManagedRuntimeForKind {
            runtime_ref: "runtime:storage-fixture".to_string(),
            runtime_content_hash: runtime_header.content_hash.clone(),
            runtime_signer_fingerprint: runtime_header.signer_fingerprint.clone(),
            serves_kind: "graph".to_string(),
            executor_ref: "native:storage-fixture".to_string(),
        };
        let source_capsule = crate::launch_metadata::RuntimeLaunchMetadata {
            launch_driver: Some(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime),
            resume_context: Some(source_resume),
            sealed_root_request: Some(capsule_fixture),
            admitted_project_authority: Some(source_authority),
            admitted_artifact_identity: Some(
                ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
                    runtime_ref: "runtime:storage-fixture".to_string(),
                    runtime_content_hash: runtime_header.content_hash.clone(),
                    runtime_signer_fingerprint: runtime_header.signer_fingerprint.clone(),
                    protocol_ref: "protocol:test/fixture".to_string(),
                    protocol_content_hash: protocol_header.content_hash.clone(),
                    protocol_signer_fingerprint: protocol_header.signer_fingerprint.clone(),
                    executor_ref: "native:storage-fixture".to_string(),
                    executor_content_hash: "7".repeat(64),
                    executor_bundle_manifest_hash: "8".repeat(64),
                    executor_bundle_signer_fingerprint: "9".repeat(64),
                },
            ),
            admitted_launch_capsule_schema: Some(
                ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
            ),
            execution_realization_hash: Some("a".repeat(64)),
            admitted_execution_closure: Some(
                ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
                    prepared_runtime_launch: json!({
                        "binding_records": {},
                        "required_secrets": [],
                        "admitted_sessions": {}
                    }),
                    runtime_descriptor_document,
                    protocol_descriptor_document,
                    executor_blob_hash: "7".repeat(64),
                },
            ),
            ..crate::launch_metadata::RuntimeLaunchMetadata::default()
        }
        .admitted_launch_capsule()
        .unwrap()
        .expect("sealed source launch must produce a capsule");
        let (capsule_resume, capsule_target) =
            SealedRootExecutionRequest::for_remote_worker_adoption_from_capsule(
                &source_capsule,
                &rebind,
            )
            .unwrap();
        assert_eq!(
            capsule_resume.parent_delegation_caps,
            Some(vec![
                "ryeos.read.project.live".to_string(),
                "ryeos.write.project.live".to_string()
            ])
        );
        assert_eq!(
            capsule_resume.effective_caps,
            vec!["ryeos.read.project.live"]
        );
        assert_eq!(
            capsule_resume.parameters,
            json!({
                "credential_profile_id":"target-profile",
                "worker_defined":{"mode":"review","limit":7}
            })
        );
        assert_eq!(
            capsule_target
                .handler_context()
                .and_then(|context| context.authorized_key_class),
            Some(crate::identity::AuthorizedKeyPrincipalClass::RemoteOperator)
        );
        assert_eq!(capsule_target.parameters, capsule_resume.parameters);
    }

    #[test]
    fn continuation_subject_authority_preserves_exact_family_and_cow_base() {
        use ryeos_engine::contracts::SubjectResolutionAuthority;

        let base = "a".repeat(64);
        let current = "b".repeat(64);
        let authority = cow_authority(&base, &current);
        let inherited = continued_binding_subject_authority(
            &SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash: base.clone(),
                current_operational_generation: base.clone(),
            },
            &authority,
        )
        .unwrap();
        assert_eq!(
            inherited,
            SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash: base.clone(),
                current_operational_generation: current,
            }
        );
        assert!(
            continued_binding_subject_authority(
                &SubjectResolutionAuthority::PinnedGeneration {
                    snapshot_hash: base.clone(),
                },
                &authority,
            )
            .is_err()
        );
        assert!(
            continued_binding_subject_authority(
                &SubjectResolutionAuthority::CowWorkspace {
                    base_snapshot_hash: "c".repeat(64),
                    current_operational_generation: base,
                },
                &authority,
            )
            .is_err()
        );
    }

    #[test]
    fn sealed_source_capsule_never_becomes_materialized_project_root() {
        let projectless = SealedRootExecutionRequest::storage_test_fixture();
        let capsule = tempfile::tempdir().unwrap();
        let restored = projectless
            .verified_subject
            .restore(capsule.path())
            .unwrap();
        assert_eq!(restored.source_path, capsule.path().join("subject.source"));
        assert_eq!(restored.materialized_project_root, None);

        let mut project = SealedRootExecutionRequest::storage_test_fixture();
        project.verified_subject.materialized_project_root =
            Some(PathBuf::from("/admitted/project"));
        let capsule = tempfile::tempdir().unwrap();
        let restored = project.verified_subject.restore(capsule.path()).unwrap();
        assert_eq!(restored.source_path, capsule.path().join("subject.source"));
        assert_eq!(
            restored.materialized_project_root,
            Some(PathBuf::from("/admitted/project"))
        );
    }

    #[test]
    fn admitted_program_subject_rechecks_source_and_runtime_body_hashes() {
        let request = SealedRootExecutionRequest::storage_test_fixture();
        let subject = request.admitted_program_subject().unwrap();
        assert_eq!(subject.canonical_ref, "graph:test/storage-fixture");
        assert_eq!(subject.source_content, "{}");
        assert_eq!(subject.raw_content, "{}");
        assert_eq!(subject.source_content_digest, lillux::sha256_hex(b"{}"));
        assert_eq!(subject.raw_content_digest, lillux::sha256_hex(b"{}"));

        let mut source_tampered = request.clone();
        source_tampered.verified_subject.source_content_b64 =
            base64::engine::general_purpose::STANDARD.encode(b"changed");
        assert!(source_tampered.admitted_program_subject().is_err());

        let mut body_tampered = request;
        body_tampered.verified_subject.raw_content_digest = "f".repeat(64);
        body_tampered.resolution_output.root.raw_content_digest = "f".repeat(64);
        assert!(body_tampered.admitted_program_subject().is_err());
    }

    #[test]
    fn storage_fixture_retains_one_self_consistent_effective_program_identity() {
        let request = SealedRootExecutionRequest::storage_test_fixture();
        let observed = request
            .resolution_output
            .effective_definition_digest()
            .unwrap();
        assert_eq!(request.effective_definition_digest(), &observed);
        assert_eq!(
            request
                .admitted_effective_resolution()
                .unwrap()
                .effective_definition_digest()
                .unwrap(),
            observed
        );

        let exact_program = request.admitted_program_value().unwrap();
        assert_eq!(
            exact_program["effective_definition_digest"],
            serde_json::to_value(&observed).unwrap()
        );
        assert_eq!(
            request.admitted_program_hash().unwrap(),
            lillux::sha256_hex(lillux::canonical_json(&exact_program).unwrap().as_bytes())
        );

        let round_trip: SealedRootExecutionRequest =
            serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
        assert_eq!(
            round_trip.effective_definition_digest(),
            request.effective_definition_digest()
        );
        assert_eq!(round_trip.admitted_program_value().unwrap(), exact_program);
    }

    #[test]
    fn sealed_cow_continuation_survives_restart_and_advances_operational_generation() {
        use ryeos_engine::contracts::SubjectResolutionAuthority;

        let base = "a".repeat(64);
        let generation_b = "b".repeat(64);
        let generation_c = "c".repeat(64);
        let mut initial = SealedRootExecutionRequest::storage_test_fixture_with_project_identity(
            ProjectContext::LocalPath {
                path: PathBuf::from("/tmp/cow-a"),
            },
            cow_authority(&base, &base),
        );
        initial.parameters = json!({"continuation": true});
        let generation_b_request = initial
            .for_continuation_invocation(&continuation_resume(
                "/tmp/cow-b",
                cow_authority(&base, &generation_b),
            ))
            .unwrap();
        let restarted: SealedRootExecutionRequest =
            serde_json::from_value(serde_json::to_value(generation_b_request).unwrap()).unwrap();
        let generation_c_request = restarted
            .for_continuation_invocation(&continuation_resume(
                "/tmp/cow-c",
                cow_authority(&base, &generation_c),
            ))
            .unwrap();

        assert_eq!(
            generation_c_request.project_binding_subject_authority,
            SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash: base.clone(),
                current_operational_generation: generation_c,
            }
        );
        assert_eq!(
            generation_c_request.resolution_subject_authority,
            SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash: base.clone(),
                current_operational_generation: base.clone(),
            },
            "the admitted program closure remains rooted at its original generation"
        );
        assert_eq!(
            generation_c_request
                .project_authority
                .subject_base_snapshot_hash(),
            Some(base.as_str())
        );
    }
}
