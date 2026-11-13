//! Binding inventory and explicit generation renewal of pinned-project
//! content bindings.
//!
//! A pinned-project binding authorizes one consumer at one exact project
//! generation and one exact effective definition. A new snapshot or a
//! re-signed consumer is a new consumer authority, so every earlier binding
//! stops authorizing it. The node never carries a binding forward on its own:
//! the inventory reports which bindings no longer match, and `rebind` is the
//! operator's explicit, signed act of authorizing the same retained manifest
//! for the new generation. It reuses the retained-binding stage and the
//! ordinary bind, so it adds no second binding authority.

use std::path::PathBuf;

use ryeos_node_runtime::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use ryeos_state::external_content::products::composition::ProductSelection;
use ryeos_state::objects::{
    ExternalContentBinding, ExternalContentBindingState, ExternalContentConsumerAuthority,
};

use super::*;

/// Typed refusal of a re-bind. The prior binding stays exactly as it was.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct RebindRefused {
    pub code: &'static str,
    pub message: String,
}

impl RebindRefused {
    /// The consumer at the new generation is not the same consumer: another
    /// publisher, no longer project-scoped, or it no longer declares the
    /// prior binding's manifest.
    pub const CONSUMER_CHANGED: &'static str = "rebind_consumer_changed";
    /// Installed-bundle bindings do not depend on a project generation.
    pub const NOT_GENERATION_SCOPED: &'static str = "rebind_not_generation_scoped";
    /// The named binding is released or no longer the current head of its
    /// subject.
    pub const PRIOR_NOT_CURRENT: &'static str = "rebind_prior_binding_not_current";

    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn handler_error(&self) -> crate::handler_error::HandlerError {
        crate::handler_error::HandlerError::Structured {
            code: self.code.to_owned(),
            status: 409,
            body: serde_json::json!({ "code": self.code, "error": self.message }),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingsRequest {
    pub consumer_ref: String,
    pub consumer_kind: BindConsumerKind,
    #[serde(default)]
    pub project_snapshot_hash: Option<String>,
    #[serde(default)]
    pub project_path: Option<PathBuf>,
    #[serde(default)]
    pub product_selections: Option<Vec<ProductSelection>>,
    #[serde(default)]
    pub product_owner_principal: Option<String>,
}

impl BindingsRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_canonical_consumer_ref(&self.consumer_ref)?;
        validate_consumer_fields(
            self.consumer_kind,
            self.project_snapshot_hash.as_deref(),
            self.project_path.as_deref(),
            self.product_selections.as_deref(),
            self.product_owner_principal.as_deref(),
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebindRequest {
    pub prior_binding_hash: String,
    pub project_snapshot_hash: String,
    #[serde(default)]
    pub project_path: Option<PathBuf>,
    #[serde(default)]
    pub product_selections: Option<Vec<ProductSelection>>,
    #[serde(default)]
    pub product_owner_principal: Option<String>,
    /// Ceiling for the re-staged manifest. Defaults to the node import
    /// policy's total-byte ceiling.
    #[serde(default)]
    pub maximum_bytes: Option<u64>,
}

impl RebindRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !is_canonical_hash(&self.prior_binding_hash) {
            bail!("rebind prior_binding_hash is not a canonical digest");
        }
        if self.maximum_bytes == Some(0) {
            bail!("rebind maximum_bytes must be positive");
        }
        validate_consumer_fields(
            BindConsumerKind::PinnedProject,
            Some(&self.project_snapshot_hash),
            self.project_path.as_deref(),
            self.product_selections.as_deref(),
            self.product_owner_principal.as_deref(),
        )
    }
}

/// Non-secret summary of one consumer authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConsumerSummary {
    pub kind: &'static str,
    pub publisher_fingerprint: String,
    pub project_snapshot_hash: Option<String>,
    pub effective_consumer_digest: Option<String>,
}

impl ConsumerSummary {
    fn of(consumer: &ExternalContentConsumerAuthority) -> Self {
        match consumer {
            ExternalContentConsumerAuthority::InstalledBundle {
                publisher_fingerprint,
                ..
            } => Self {
                kind: "installed_bundle",
                publisher_fingerprint: publisher_fingerprint.clone(),
                project_snapshot_hash: None,
                effective_consumer_digest: None,
            },
            ExternalContentConsumerAuthority::PinnedProject {
                publisher_fingerprint,
                project_snapshot_hash,
                effective_consumer_digest,
                ..
            } => Self {
                kind: "pinned_project",
                publisher_fingerprint: publisher_fingerprint.clone(),
                project_snapshot_hash: Some(project_snapshot_hash.clone()),
                effective_consumer_digest: Some(effective_consumer_digest.clone()),
            },
        }
    }
}

/// One retained-content declaration of the consumer at the requested
/// generation, as launch admission would see it.
#[derive(Debug, Clone, Serialize)]
pub struct DeclarationBindingStatus {
    pub id: String,
    pub manifest_hash: String,
    /// `ready`, `missing_binding` or `missing_manifest`.
    pub status: &'static str,
    pub binding_hash: Option<String>,
    /// Newest active binding of this consumer and manifest made for another
    /// generation; the argument `rebind` takes.
    pub prior_binding_hash: Option<String>,
}

/// One binding head of the consumer.
#[derive(Debug, Clone, Serialize)]
pub struct BindingInventoryEntry {
    pub binding_hash: String,
    pub binding_subject_id: String,
    pub binding_id: String,
    pub manifest_hash: String,
    pub manifest_kind: String,
    pub consumer: ConsumerSummary,
    pub authorized_by: String,
    pub recorded_at: String,
    /// `current`, `stale` or `released`.
    pub status: &'static str,
    /// Why a `stale` binding no longer authorizes the consumer.
    pub stale_reasons: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BindingsResponse {
    pub consumer_ref: String,
    pub current_consumer: ConsumerSummary,
    pub ready_for_admission: bool,
    pub declarations: Vec<DeclarationBindingStatus>,
    pub bindings: Vec<BindingInventoryEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RebindResponse {
    pub prior_binding_hash: String,
    pub project_snapshot_hash: String,
    pub binding_subject_id: String,
    pub binding_id: String,
    pub binding_hash: String,
    pub manifest_hash: String,
    pub consumer_ref: String,
    pub publisher_fingerprint: String,
    /// True when the consumer already held an active binding for this
    /// generation; nothing was staged or written.
    pub idempotent: bool,
}

/// The exact prior binding a re-bind renews, checked before any
/// preparation or staging.
#[derive(Debug, Clone)]
pub struct PriorBinding {
    binding_hash: String,
    binding: ExternalContentBinding,
}

impl PriorBinding {
    pub fn consumer_ref(&self) -> &str {
        self.binding.consumer.consumer_ref()
    }
}

/// List every binding head of one consumer and say, against the consumer
/// resolved at the requested generation, whether each still authorizes it.
///
/// `resolution` must be the consumer prepared by the API layer through the
/// same snapshot materializer and source admission that bind and launch use;
/// for a selected consumer it carries the same verified product selections.
/// This is read-only: no head, stage or CAS root is written.
pub fn inventory(
    state: &AppState,
    context: &HandlerContext,
    request: &BindingsRequest,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<BindingsResponse> {
    crate::operator_authority::require_local_configured_operator(state, context)?;
    request.validate()?;
    if resolution.root.resolved_ref != request.consumer_ref {
        bail!("prepared consumer does not match the requested consumer ref");
    }
    let subject = match request.consumer_kind {
        BindConsumerKind::InstalledBundle => {
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
        }
        BindConsumerKind::PinnedProject => {
            ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: request
                    .project_snapshot_hash
                    .clone()
                    .expect("validated pinned-project request"),
            }
        }
    };
    let current = crate::external_content_admission::consumer_authority(resolution, &subject)?;
    let declarations = retained_declarations(state, resolution)?;
    let node = state.identity.fingerprint().to_owned();

    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let mut bindings = Vec::new();
    for (binding_hash, binding) in consumer_binding_heads(state, &cas, &request.consumer_ref)? {
        let declared = declarations
            .iter()
            .any(|(_, digest)| digest == &binding.manifest_hash);
        let authorizer_current = binding.state != ExternalContentBindingState::Active
            || require_current_binding_authorizer(state, &binding).is_ok();
        let (status, stale_reasons) =
            binding_status(&binding, &current, declared, authorizer_current);
        bindings.push(BindingInventoryEntry {
            binding_hash,
            binding_subject_id: binding.binding_subject_id.clone(),
            binding_id: binding.binding_id.clone(),
            manifest_hash: binding.manifest_hash.clone(),
            manifest_kind: binding.manifest_kind.clone(),
            consumer: ConsumerSummary::of(&binding.consumer),
            authorized_by: binding.authorized_by.clone(),
            recorded_at: binding.recorded_at.clone(),
            status,
            stale_reasons,
        });
    }
    // Newest first, so the first stale match is the newest prior binding.
    bindings.sort_by(|left, right| {
        right
            .recorded_at
            .cmp(&left.recorded_at)
            .then_with(|| left.binding_hash.cmp(&right.binding_hash))
    });

    let mut ready_for_admission = true;
    let mut declaration_statuses = Vec::with_capacity(declarations.len());
    for (id, digest) in declarations {
        let (status, binding_hash) = if cas.get_object(&digest)?.is_none() {
            ("missing_manifest", None)
        } else {
            match active_binding_from_store(&state.state_store, &cas, &digest, &current, &node)? {
                Some((binding_hash, binding))
                    if require_current_binding_authorizer(state, &binding).is_ok() =>
                {
                    ("ready", Some(binding_hash))
                }
                _ => ("missing_binding", None),
            }
        };
        ready_for_admission &= status == "ready";
        let prior_binding_hash = if status == "missing_binding" {
            newest_prior_binding(&bindings, &digest)
        } else {
            None
        };
        declaration_statuses.push(DeclarationBindingStatus {
            id,
            manifest_hash: digest,
            status,
            binding_hash,
            prior_binding_hash,
        });
    }
    drop(guard);
    Ok(BindingsResponse {
        consumer_ref: request.consumer_ref.clone(),
        current_consumer: ConsumerSummary::of(&current),
        ready_for_admission,
        declarations: declaration_statuses,
        bindings,
    })
}

/// Check the named prior binding before anything is prepared or staged: it
/// must be the active current head of its subject, made by this operator for
/// a pinned-project consumer.
pub fn prior_binding(
    state: &AppState,
    context: &HandlerContext,
    request: &RebindRequest,
) -> anyhow::Result<PriorBinding> {
    let operator = crate::operator_authority::require_local_configured_operator(state, context)?;
    request.validate()?;
    let closure_policy = state.node_policy.require::<NodeObjectClosurePolicy>()?;
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let binding = ExternalContentBinding::from_value(
        &ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &cas,
            &request.prior_binding_hash,
            closure_policy.max_object_bytes,
        )?,
    )?;
    if !matches!(
        binding.consumer,
        ExternalContentConsumerAuthority::PinnedProject { .. }
    ) {
        return Err(RebindRefused::new(
            RebindRefused::NOT_GENERATION_SCOPED,
            format!(
                "binding {} authorizes an installed-bundle consumer, which does not depend on a project generation; nothing to renew",
                request.prior_binding_hash
            ),
        )
        .into());
    }
    let current = active_binding_from_store(
        &state.state_store,
        &cas,
        &binding.manifest_hash,
        &binding.consumer,
        state.identity.fingerprint(),
    )?;
    if current.as_ref().map(|(hash, _)| hash.as_str()) != Some(request.prior_binding_hash.as_str())
    {
        return Err(RebindRefused::new(
            RebindRefused::PRIOR_NOT_CURRENT,
            format!(
                "binding {} is released or no longer the current head of its subject; run `ryeos external-content bindings {} pinned_project <snapshot>` for the current prior binding",
                request.prior_binding_hash,
                binding.consumer.consumer_ref()
            ),
        )
        .into());
    }
    if binding.authorized_by != operator {
        bail!("rebind prior binding was authorized by another operator");
    }
    require_current_binding_authorizer(state, &binding)?;
    Ok(PriorBinding {
        binding_hash: request.prior_binding_hash.clone(),
        binding,
    })
}

/// Authorize the prior binding's exact retained manifest for the same
/// consumer at the requested generation.
///
/// Every check that can refuse runs before the manifest is staged: the
/// consumer at the new generation must have the same publisher, still be
/// project-scoped, and still declare the manifest. The stage and bind then
/// reuse the ordinary retained-binding import and bind.
pub async fn rebind(
    state: Arc<AppState>,
    context: HandlerContext,
    request: RebindRequest,
    prior: PriorBinding,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<RebindResponse> {
    crate::operator_authority::require_local_configured_operator(&state, &context)?;
    request.validate()?;
    let consumer_ref = prior.consumer_ref().to_owned();
    if resolution.root.resolved_ref != consumer_ref {
        bail!("prepared consumer does not match the prior binding's consumer");
    }
    let subject = ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration {
        snapshot_hash: request.project_snapshot_hash.clone(),
    };
    let current = crate::external_content_admission::consumer_authority(resolution, &subject)
        .map_err(|error| {
            RebindRefused::new(
                RebindRefused::CONSUMER_CHANGED,
                format!(
                    "{consumer_ref} has no pinned-project consumer authority at snapshot {}: {error:#}",
                    request.project_snapshot_hash
                ),
            )
        })?;
    check_same_consumer(&prior.binding, &current)?;
    let declared = retained_declarations(&state, resolution)?
        .iter()
        .any(|(_, digest)| digest == &prior.binding.manifest_hash);
    if !declared {
        return Err(RebindRefused::new(
            RebindRefused::CONSUMER_CHANGED,
            format!(
                "{consumer_ref} at snapshot {} no longer declares manifest {}; bind its new content instead",
                request.project_snapshot_hash, prior.binding.manifest_hash
            ),
        )
        .into());
    }
    let existing = {
        let authority = state.state_store.pinned_state_authority()?;
        let _guard = authority.acquire_shared_guard()?;
        let cas = authority.cas_store()?;
        active_binding_from_store(
            &state.state_store,
            &cas,
            &prior.binding.manifest_hash,
            &current,
            state.identity.fingerprint(),
        )?
    };
    if let Some((binding_hash, binding)) = existing
        && require_current_binding_authorizer(&state, &binding).is_ok()
    {
        return Ok(RebindResponse {
            prior_binding_hash: prior.binding_hash,
            project_snapshot_hash: request.project_snapshot_hash,
            binding_subject_id: binding.binding_subject_id,
            binding_id: binding.binding_id,
            binding_hash,
            manifest_hash: binding.manifest_hash,
            consumer_ref,
            publisher_fingerprint: current.publisher_fingerprint().to_owned(),
            idempotent: true,
        });
    }

    let maximum_bytes = match request.maximum_bytes {
        Some(maximum_bytes) => maximum_bytes,
        None => {
            state
                .node_policy
                .require::<ExternalContentImportPolicyRecord>()?
                .limits
                .max_total_bytes
        }
    };
    let import_state = Arc::clone(&state);
    let import_context = context.clone();
    let import_request = RetainedBindingImportRequest {
        binding_hash: prior.binding_hash.clone(),
        maximum_bytes,
        binding_owner_principal: None,
    };
    let staged = tokio::task::spawn_blocking(move || {
        retained_binding::import(import_state, import_context, import_request)
    })
    .await
    .context("rebind staging worker stopped")??;
    if staged.manifest_hash != prior.binding.manifest_hash {
        bail!("rebind staged a manifest other than the prior binding's");
    }
    let bind_request = BindRequest {
        staging_id: staged.staging_id,
        request_digest: staged.request_digest,
        manifest_hash: staged.manifest_hash,
        consumer_ref: consumer_ref.clone(),
        consumer_kind: BindConsumerKind::PinnedProject,
        project_snapshot_hash: Some(request.project_snapshot_hash.clone()),
        project_path: request.project_path,
        product_selections: request.product_selections,
        product_owner_principal: request.product_owner_principal,
    };
    let bound = if bind_request.product_selections.is_some() {
        bind_selected_literal_resolution(state, context, bind_request, resolution).await?
    } else {
        bind_pinned_project(state, context, bind_request, resolution).await?
    };
    Ok(RebindResponse {
        prior_binding_hash: prior.binding_hash,
        project_snapshot_hash: request.project_snapshot_hash,
        binding_subject_id: bound.binding_subject_id,
        binding_id: bound.binding_id,
        binding_hash: bound.binding_hash,
        manifest_hash: bound.manifest_hash,
        consumer_ref,
        publisher_fingerprint: bound.publisher_fingerprint,
        idempotent: bound.idempotent,
    })
}

/// The new generation must be the same consumer in everything except its
/// generation coordinates.
fn check_same_consumer(
    prior: &ExternalContentBinding,
    current: &ExternalContentConsumerAuthority,
) -> anyhow::Result<()> {
    if !matches!(
        current,
        ExternalContentConsumerAuthority::PinnedProject { .. }
    ) {
        return Err(RebindRefused::new(
            RebindRefused::CONSUMER_CHANGED,
            format!(
                "{} no longer depends on the pinned project; bind it as an installed-bundle consumer instead",
                current.consumer_ref()
            ),
        )
        .into());
    }
    if current.consumer_ref() != prior.consumer.consumer_ref() {
        return Err(RebindRefused::new(
            RebindRefused::CONSUMER_CHANGED,
            format!(
                "prepared consumer {} differs from the prior binding's consumer {}",
                current.consumer_ref(),
                prior.consumer.consumer_ref()
            ),
        )
        .into());
    }
    if current.publisher_fingerprint() != prior.consumer.publisher_fingerprint() {
        return Err(RebindRefused::new(
            RebindRefused::CONSUMER_CHANGED,
            format!(
                "{} is now signed by {}, not {}; a new publisher needs a fresh operator bind",
                current.consumer_ref(),
                current.publisher_fingerprint(),
                prior.consumer.publisher_fingerprint()
            ),
        )
        .into());
    }
    Ok(())
}

/// Status of one binding against the consumer at the requested generation.
fn binding_status(
    binding: &ExternalContentBinding,
    current: &ExternalContentConsumerAuthority,
    declared: bool,
    authorizer_current: bool,
) -> (&'static str, Vec<&'static str>) {
    if binding.state == ExternalContentBindingState::Released {
        return ("released", Vec::new());
    }
    let mut reasons = Vec::new();
    let prior = &binding.consumer;
    if prior.publisher_fingerprint() != current.publisher_fingerprint() {
        reasons.push("publisher_changed");
    }
    match (prior, current) {
        (
            ExternalContentConsumerAuthority::PinnedProject {
                project_snapshot_hash: prior_snapshot,
                effective_consumer_digest: prior_digest,
                source_closure: prior_source,
                ..
            },
            ExternalContentConsumerAuthority::PinnedProject {
                project_snapshot_hash: current_snapshot,
                effective_consumer_digest: current_digest,
                source_closure: current_source,
                ..
            },
        ) => {
            if prior_snapshot != current_snapshot {
                reasons.push("project_snapshot_changed");
            }
            if prior_digest != current_digest {
                reasons.push("consumer_definition_changed");
            }
            if prior_source != current_source {
                reasons.push("source_closure_changed");
            }
        }
        (
            ExternalContentConsumerAuthority::InstalledBundle { .. },
            ExternalContentConsumerAuthority::InstalledBundle { .. },
        ) => {}
        _ => reasons.push("consumer_kind_changed"),
    }
    if !declared {
        reasons.push("manifest_not_declared");
    }
    if !authorizer_current {
        reasons.push("authorizer_not_current");
    }
    if reasons.is_empty() {
        ("current", reasons)
    } else {
        ("stale", reasons)
    }
}

fn newest_prior_binding(bindings: &[BindingInventoryEntry], manifest_hash: &str) -> Option<String> {
    bindings
        .iter()
        .find(|entry| {
            entry.status == "stale"
                && entry.manifest_hash == manifest_hash
                && entry.consumer.kind == "pinned_project"
        })
        .map(|entry| entry.binding_hash.clone())
}

/// Locator-free pinned declarations: the only ones an operator binding
/// authorizes. Uses the same declaration set bind checks against.
fn retained_declarations(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<Vec<(String, String)>> {
    let canonical =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&resolution.root.resolved_ref)?;
    let contract = state
        .engine
        .kinds
        .get(&canonical.kind)
        .and_then(|kind| kind.external_content_contract())
        .ok_or_else(|| anyhow::anyhow!("consumer kind has no signed external-content contract"))?;
    let declarations = ryeos_engine::external_content::external_content_declarations_for_binding(
        resolution,
        Some(contract),
        ryeos_engine::external_content::declaring_authority(resolution)?,
    )?
    .ok_or_else(|| anyhow::anyhow!("consumer does not declare external content"))?;
    Ok(declarations
        .into_iter()
        .filter(|declaration| {
            declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
                && declaration.locator.is_none()
                && declaration.bundle_binary.is_none()
        })
        .filter_map(|declaration| declaration.digest.map(|digest| (declaration.id, digest)))
        .collect())
}

/// Every binding head whose consumer is `consumer_ref`. Heads are keyed by a
/// digest over manifest and consumer authority, so this is a scan of the
/// node's binding namespace; corrupt heads fail the read, as in scrub.
fn consumer_binding_heads(
    state: &AppState,
    cas: &lillux::CasStore,
    consumer_ref: &str,
) -> anyhow::Result<Vec<(String, ExternalContentBinding)>> {
    let node = state.identity.fingerprint();
    let heads = state
        .state_store
        .with_state_db(|db| db.list_generic_head_refs(BINDING_HEAD_NAMESPACE))?;
    let mut bindings = Vec::new();
    for head in heads {
        let value = cas.get_object(&head.target_hash)?.ok_or_else(|| {
            anyhow::anyhow!(
                "external-content binding head {} target is absent",
                head.name
            )
        })?;
        let binding = ExternalContentBinding::from_value(&value)
            .with_context(|| format!("external-content binding head {}", head.name))?;
        if binding.consumer.consumer_ref() != consumer_ref {
            continue;
        }
        if head.signer != node
            || binding.binding_subject_id != head.name
            || binding.target_node_fingerprint != node
        {
            bail!(
                "external-content binding head {} contradicts its retained binding",
                head.name
            );
        }
        bindings.push((head.target_hash, binding));
    }
    Ok(bindings)
}

fn validate_canonical_consumer_ref(consumer_ref: &str) -> anyhow::Result<()> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(consumer_ref)
        .map_err(|error| anyhow::anyhow!("invalid consumer ref: {error}"))?;
    if canonical.to_string() != consumer_ref || canonical.suffix.is_some() {
        bail!("external-content consumer ref must be canonical");
    }
    Ok(())
}

fn is_canonical_hash(value: &str) -> bool {
    lillux::valid_hash(value) && !value.bytes().any(|byte| byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn pinned(snapshot: char, digest: char, publisher: char) -> ExternalContentConsumerAuthority {
        ExternalContentConsumerAuthority::pinned_project(
            "tool:example/verify".to_owned(),
            hash(publisher),
            hash(snapshot),
            hash(digest),
            None,
        )
        .unwrap()
    }

    fn binding(consumer: ExternalContentConsumerAuthority) -> ExternalContentBinding {
        ExternalContentBinding::active(
            hash('e'),
            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            consumer,
            hash('f'),
            hash('9'),
            hash('8'),
        )
        .unwrap()
    }

    #[test]
    fn binding_made_at_an_earlier_snapshot_is_stale_naming_the_snapshot() {
        let prior = binding(pinned('1', 'a', 'c'));
        let (status, reasons) = binding_status(&prior, &pinned('2', 'a', 'c'), true, true);
        assert_eq!(status, "stale");
        assert_eq!(reasons, vec!["project_snapshot_changed"]);
    }

    #[test]
    fn binding_made_before_a_verifier_resign_is_stale_naming_the_definition() {
        let prior = binding(pinned('1', 'a', 'c'));
        let (status, reasons) = binding_status(&prior, &pinned('1', 'b', 'c'), true, true);
        assert_eq!(status, "stale");
        assert_eq!(reasons, vec!["consumer_definition_changed"]);
    }

    #[test]
    fn binding_for_the_exact_consumer_is_current() {
        let prior = binding(pinned('1', 'a', 'c'));
        let (status, reasons) = binding_status(&prior, &pinned('1', 'a', 'c'), true, true);
        assert_eq!(status, "current");
        assert!(reasons.is_empty());
        let (status, reasons) = binding_status(&prior, &pinned('1', 'a', 'c'), false, false);
        assert_eq!(status, "stale");
        assert_eq!(
            reasons,
            vec!["manifest_not_declared", "authorizer_not_current"]
        );
    }

    #[test]
    fn newest_stale_pinned_binding_is_named_as_the_prior_binding() {
        let entry =
            |hash_byte: char, status: &'static str, kind: &'static str| BindingInventoryEntry {
                binding_hash: hash(hash_byte),
                binding_subject_id: hash('0'),
                binding_id: hash('0'),
                manifest_hash: hash('e'),
                manifest_kind: String::new(),
                consumer: ConsumerSummary {
                    kind,
                    publisher_fingerprint: hash('c'),
                    project_snapshot_hash: None,
                    effective_consumer_digest: None,
                },
                authorized_by: hash('9'),
                recorded_at: String::new(),
                status,
                stale_reasons: Vec::new(),
            };
        // Already sorted newest first by the inventory.
        let bindings = vec![
            entry('3', "released", "pinned_project"),
            entry('4', "stale", "installed_bundle"),
            entry('5', "stale", "pinned_project"),
            entry('6', "stale", "pinned_project"),
        ];
        assert_eq!(newest_prior_binding(&bindings, &hash('e')), Some(hash('5')));
        assert_eq!(newest_prior_binding(&bindings, &hash('d')), None);
    }

    #[test]
    fn rebind_refuses_changed_publisher() {
        let prior = binding(pinned('1', 'a', 'c'));
        let error = check_same_consumer(&prior, &pinned('2', 'b', 'd')).unwrap_err();
        let refused = error.downcast_ref::<RebindRefused>().unwrap();
        assert_eq!(refused.code, RebindRefused::CONSUMER_CHANGED);
        assert!(matches!(
            crate::handler_error::extract_handler_error(&error),
            Some(crate::handler_error::HandlerError::Structured { status: 409, ref code, .. })
                if code == RebindRefused::CONSUMER_CHANGED
        ));
        check_same_consumer(&prior, &pinned('2', 'b', 'c')).unwrap();
    }

    #[test]
    fn rebind_refuses_a_consumer_that_left_the_project() {
        let prior = binding(pinned('1', 'a', 'c'));
        let installed = ExternalContentConsumerAuthority::installed_bundle(
            "tool:example/verify".to_owned(),
            hash('c'),
        )
        .unwrap();
        let error = check_same_consumer(&prior, &installed).unwrap_err();
        assert_eq!(
            error.downcast_ref::<RebindRefused>().unwrap().code,
            RebindRefused::CONSUMER_CHANGED
        );
    }

    #[test]
    fn rebind_request_is_closed_and_pinned() {
        let request = serde_json::json!({
            "prior_binding_hash": hash('a'),
            "project_snapshot_hash": hash('b'),
            "project_path": "/abs/project",
        });
        let parsed: RebindRequest = serde_json::from_value(request.clone()).unwrap();
        parsed.validate().unwrap();
        let mut unknown = request.clone();
        unknown["consumer_ref"] = serde_json::json!("tool:example/verify");
        assert!(serde_json::from_value::<RebindRequest>(unknown).is_err());
        for (field, value) in [
            ("prior_binding_hash", serde_json::json!("A".repeat(64))),
            ("project_snapshot_hash", serde_json::json!("b".repeat(63))),
            ("maximum_bytes", serde_json::json!(0)),
            (
                "product_owner_principal",
                serde_json::json!(format!("fp:{}", hash('c'))),
            ),
        ] {
            let mut invalid = request.clone();
            invalid[field] = value;
            let parsed: RebindRequest = serde_json::from_value(invalid).unwrap();
            assert!(parsed.validate().is_err(), "{field} must be refused");
        }
        let mut no_path = request;
        no_path.as_object_mut().unwrap().remove("project_path");
        let parsed: RebindRequest = serde_json::from_value(no_path).unwrap();
        assert!(parsed.validate().is_err());
    }

    #[test]
    fn bindings_request_names_the_generation_for_a_pinned_consumer() {
        let installed = serde_json::json!({
            "consumer_ref": "tool:example/verify",
            "consumer_kind": "installed_bundle",
        });
        serde_json::from_value::<BindingsRequest>(installed.clone())
            .unwrap()
            .validate()
            .unwrap();
        let mut pinned = installed;
        pinned["consumer_kind"] = serde_json::json!("pinned_project");
        assert!(
            serde_json::from_value::<BindingsRequest>(pinned.clone())
                .unwrap()
                .validate()
                .is_err()
        );
        pinned["project_snapshot_hash"] = serde_json::json!(hash('b'));
        pinned["project_path"] = serde_json::json!("/abs/project");
        serde_json::from_value::<BindingsRequest>(pinned.clone())
            .unwrap()
            .validate()
            .unwrap();
        pinned["consumer_ref"] = serde_json::json!("not a ref");
        assert!(
            serde_json::from_value::<BindingsRequest>(pinned)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
