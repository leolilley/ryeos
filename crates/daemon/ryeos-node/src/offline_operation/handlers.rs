//! The handlers: the stopped-node cores, each taking the state lock the
//! application already holds.
//!
//! A handler reads only its arguments, its digest-verified inputs and the
//! node's own state. It never reads the process environment, the image or any
//! other path the request did not pin. Output artifacts that the node trusts
//! (capacity documents, init-completion reseals, composed bindings) are
//! signed, as they always were, by the keys on this node's volume: the local
//! operator key where an operator signs, the node key where the node signs.
//! A remote signer authorizes an operation; it never signs what the operation
//! produces.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use lillux::crypto::SigningKey;
use ryeos_app::execution_history_reset::{
    ExecutionHistoryResetOptions, ExecutionHistorySchemaCut, ExecutionHistoryScratchRepairOptions,
    repair_execution_history_scratch_locked, run_execution_history_reset_with_progress_locked,
};
use ryeos_app::node_config::sections::external_execution::compose::{
    ComposeLimits, ComposeRequest, ComposeTransportRequest, DEFAULT_CAPACITY_GROUP,
    canonical_backend_id, compose_external_execution_binding,
    installed_binding_credential_generation,
};
use ryeos_app::node_config::sections::external_execution::compose_qualification::{
    QualificationBindingRequest, compose_qualification_session_binding,
    installed_qualification_binding_report,
};
use ryeos_app::node_config::sections::runtime_snapshot_production::compose::{
    ProducerFactsSource, ProductionComposeRequest, ProductionLimits, StandaloneProducerFacts,
    compose_runtime_snapshot_production_binding, installed_production_credential_generation,
};
use ryeos_app::node_config::sections::runtime_snapshot_qualification::compose::{
    QualificationComposeRequest, QualificationLimits,
    compose_runtime_snapshot_qualification_binding,
};
use ryeos_engine::roots::RuntimeRoot;
use ryeos_node_runtime::config::{Config, ConfigSources};
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::contract::is_digest;
use super::host::TypedReport;
use super::table::{HandlerContext, Prepared, VerifiedInputs};

// ── Arguments ────────────────────────────────────────────────────────────

/// Arguments of an operation that takes none.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyArguments {}

/// Replace the signed policy generation from the image seed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyReplaceArguments {
    /// Digest of the image seed tree, as measured.
    pub seed_digest: String,
    pub node_profile: String,
    pub host_class: String,
    /// The substrate image this request is for, `sha256:<hex>`.
    pub substrate_image_digest: String,
    /// Publisher trust documents to pin, as paths inside the seed tree. The
    /// seed digest covers them.
    pub trust_documents: Vec<String>,
}

impl PolicyReplaceArguments {
    pub fn validate(&self) -> Result<()> {
        ensure!(is_digest(&self.seed_digest), "seed digest is not a digest");
        ryeos_node_runtime::node_policy::generation::validate_init_profile_name(&self.node_profile)
            .context("node profile name")?;
        ensure!(
            ryeos_engine::isolation::valid_host_class_name(&self.host_class),
            "host class `{}` is not a canonical name",
            self.host_class
        );
        ensure!(
            self.substrate_image_digest
                .strip_prefix("sha256:")
                .is_some_and(is_digest),
            "substrate image digest is not `sha256:<hex>`"
        );
        ensure!(
            self.trust_documents.len() <= 16,
            "more than 16 trust documents"
        );
        for document in &self.trust_documents {
            seed_relative(Path::new("/"), document)?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyApplyArguments {
    pub section: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeArguments {
    pub id: String,
    pub backend: String,
    /// `https` or `file-relay`.
    pub transport: String,
    pub origin: Option<String>,
    pub owner_workspace_id: Option<String>,
    pub private_mailbox_parent: Option<String>,
    pub account: String,
    pub capacity_group: Option<String>,
    pub guest_runtime_manifest_hash: String,
}

impl ComposeArguments {
    /// Transport flags that contradict each other are refused with the inputs
    /// they name.
    pub fn validate(&self, inputs: &BTreeMap<String, String>) -> Result<()> {
        match self.transport.as_str() {
            "https" => {
                ensure!(
                    self.owner_workspace_id.is_none() && self.private_mailbox_parent.is_none(),
                    "transport https takes an origin and TLS roots, not the file-relay arguments"
                );
                ensure!(self.origin.is_some(), "transport https requires an origin");
                ensure!(
                    inputs.contains_key("tls_roots"),
                    "transport https requires the `tls_roots` input"
                );
            }
            "file-relay" => {
                ensure!(
                    self.origin.is_none() && !inputs.contains_key("tls_roots"),
                    "transport file-relay carries no origin and no TLS roots"
                );
                ensure!(
                    self.owner_workspace_id.is_some() && self.private_mailbox_parent.is_some(),
                    "transport file-relay requires an owner workspace id and a private mailbox parent"
                );
            }
            other => bail!("transport must be `https` or `file-relay`, not `{other}`"),
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotComposeArguments {
    pub id: String,
    /// Installed external-execution binding to take the provider facts from;
    /// exclusive with `backend`, `account`, `owner_id` and the `tls_roots`
    /// input, which together state them outright.
    pub binding: Option<String>,
    pub backend: Option<String>,
    pub account: Option<String>,
    pub owner_id: Option<String>,
    pub sandbox_group_id: String,
    pub region: String,
    pub source_plan: String,
    pub contact_timeout_seconds: Option<u32>,
    pub maximum_bootstrap_lifetime_seconds: Option<u32>,
    pub maximum_upload_bytes: Option<u64>,
}

impl SnapshotComposeArguments {
    /// Exactly one source of provider facts: the binding alone, or every
    /// stated fact with the roots input.
    pub fn validate(&self, inputs: &BTreeMap<String, String>) -> Result<()> {
        ProviderFactsArguments {
            binding: &self.binding,
            backend: &self.backend,
            account: &self.account,
            owner_id: &self.owner_id,
        }
        .validate(inputs)
    }
}

/// The arguments that name where provider facts come from.
struct ProviderFactsArguments<'a> {
    binding: &'a Option<String>,
    backend: &'a Option<String>,
    account: &'a Option<String>,
    owner_id: &'a Option<String>,
}

impl ProviderFactsArguments<'_> {
    fn validate(&self, inputs: &BTreeMap<String, String>) -> Result<()> {
        let stated = [
            self.backend.is_some(),
            self.account.is_some(),
            self.owner_id.is_some(),
            inputs.contains_key("tls_roots"),
        ];
        if self.binding.is_some() {
            ensure!(
                !stated.contains(&true),
                "a binding supplies backend, account, owner and trust roots; \
                 do not state them beside it"
            );
        } else {
            ensure!(
                !stated.contains(&false),
                "without a binding the backend, account, owner id and `tls_roots` input must \
                 all be stated"
            );
        }
        Ok(())
    }

    /// The facts source the arguments select, reading the roots input when
    /// the facts are stated.
    fn source(&self, inputs: &VerifiedInputs) -> Result<ProducerFactsSource> {
        Ok(match self.binding {
            Some(id) => ProducerFactsSource::ExternalBinding { id: id.clone() },
            None => ProducerFactsSource::Standalone(StandaloneProducerFacts {
                backend: self.backend.clone().context("backend")?,
                account: self.account.clone().context("account")?,
                owner_id: self.owner_id.clone().context("owner id")?,
                roots_pem: utf8(inputs.require("tls_roots")?, "TLS roots")?.to_owned(),
            }),
        })
    }
}

/// Compose the qualification-only structured-session binding a consumer
/// qualification's Worker session is placed under.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationSessionComposeArguments {
    pub id: String,
    /// Installed external-execution binding to take the provider facts from;
    /// exclusive with `backend`, `account`, `owner_id` and the `tls_roots`
    /// input, which together state them outright.
    pub binding: Option<String>,
    pub backend: Option<String>,
    pub account: Option<String>,
    pub owner_id: Option<String>,
    pub policy_ref: String,
    pub provider_declaration_id: String,
    pub runtime_manifest_hash: String,
    pub runtime_member_sha256: String,
    pub snapshot_id: String,
    pub plan: String,
    pub region: String,
    pub private_mailbox_parent: String,
    pub capacity_group: Option<String>,
}

impl QualificationSessionComposeArguments {
    pub fn validate(&self, inputs: &BTreeMap<String, String>) -> Result<()> {
        ProviderFactsArguments {
            binding: &self.binding,
            backend: &self.backend,
            account: &self.account,
            owner_id: &self.owner_id,
        }
        .validate(inputs)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationComposeArguments {
    pub id: String,
    pub snapshot_binding: String,
    pub verifier: String,
    pub contact_timeout_seconds: Option<u32>,
    pub maximum_lifetime_seconds: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityArguments {
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHistoryResetArguments {
    pub discard_project_heads: bool,
    pub schema_cut: Option<SchemaCutArguments>,
    pub discard_unreadable_lifetime_fence_after_reboot: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaCutArguments {
    pub from: u32,
    pub to: u32,
}

// ── Shared pieces ────────────────────────────────────────────────────────

fn config(ctx: &HandlerContext<'_>) -> Result<Config> {
    let config = Config::load(&ConfigSources {
        app_root: Some(ctx.app_root.to_path_buf()),
        ..ConfigSources::default()
    })
    .context("load the node's bootstrap configuration")?;
    ensure!(
        config.app_root == ctx.app_root,
        "the node's configuration names another app root"
    );
    Ok(config)
}

/// The local operator key on this node's volume.
fn operator_key(ctx: &HandlerContext<'_>) -> Result<SigningKey> {
    let path = RuntimeRoot::new(ctx.app_root.to_path_buf()).operator_signing_key_path();
    lillux::crypto::load_signing_key(&path)
        .with_context(|| format!("load the local operator key {}", path.display()))
}

/// A path named inside the image seed: relative, without `.` or `..`.
fn seed_relative(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    ensure!(
        !relative.is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "`{relative}` is not a plain path inside the image seed"
    );
    Ok(root.join(path))
}

fn read_json<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    serde_json::from_slice(bytes).with_context(|| format!("parse {what}"))
}

fn utf8<'a>(bytes: &'a [u8], what: &str) -> Result<&'a str> {
    std::str::from_utf8(bytes).with_context(|| format!("{what} is not UTF-8"))
}

/// The credential generation a signed request names: derived from its digest,
/// so the same request always names the generation it installed and two
/// requests never name the same one.
pub fn derived_credential_generation(request_digest: &str) -> String {
    lillux::sha256_hex(
        format!("ryeos/offline-operation-credential-generation/v1:{request_digest}").as_bytes(),
    )
}

// ── Policy ───────────────────────────────────────────────────────────────

pub(super) fn reset_policy_generation(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: PolicyReplaceArguments = ctx.arguments()?;
    arguments.validate()?;
    let seed = ctx
        .host
        .image_seed()
        .context("this process has no image seed; policy replacement needs one")?;
    ensure!(
        seed.substrate_image_digest == arguments.substrate_image_digest,
        "the request is for substrate image {}, this process runs {}",
        arguments.substrate_image_digest,
        seed.substrate_image_digest
    );
    let measured = super::seed::seed_tree_digest(&seed.root)?;
    ensure!(
        measured == arguments.seed_digest,
        "the image seed measures {measured}, not the {} the request pinned",
        arguments.seed_digest
    );
    let expected = ctx
        .expected("policy_generation")
        .context("the request pins no policy generation")?;
    let trust_files = arguments
        .trust_documents
        .iter()
        .map(|document| seed_relative(&seed.root, document))
        .collect::<Result<Vec<_>>>()?;
    let report = crate::init::run_init_under_lock(
        &crate::InitOptions {
            app_root: ctx.app_root.to_path_buf(),
            bind: None,
            uds_path: None,
            source_dir: seed.root,
            trust_files,
            node_profile: Some(arguments.node_profile.clone()),
            host_class: Some(arguments.host_class.clone()),
            substrate_identity: None,
            replace_node_policy_generation: true,
            replace_substrate_identity: false,
            confirm_substrate_identity_replacement: false,
            skip_preflight: false,
        },
        ctx.state_lock,
        Some(expected),
    )
    .context("replace the node policy generation in a locked initialization")?;
    // The pinned generation was checked before anything changed.
    Ok(json!({
        "node_profile": arguments.node_profile,
        "host_class": arguments.host_class,
        "node_fingerprint": report.node_key_fingerprint,
        "bundles": report.bundles_installed,
        "policy_generation": expected,
    }))
}

/// The policy generation a member replacement would publish, and the trust
/// it was prepared under. Pure: nothing is written.
fn plan_policy_apply(
    app_root: &Path,
    section: &str,
    policy: &[u8],
) -> Result<(
    ryeos_engine::trust::TrustStore,
    ryeos_node_runtime::node_policy::generation::NodePolicyUpdate,
)> {
    let table = ryeos_node_runtime::node_policy::NodePolicyTable::new();
    table
        .get(section)
        .with_context(|| format!("unknown node-policy section `{section}`"))?;
    let body: Value =
        serde_yaml::from_str(utf8(policy, "policy input")?).context("parse policy input")?;
    ensure!(body.is_object(), "node policy input must be a mapping");
    for forbidden in ["category", "section"] {
        ensure!(
            body.get(forbidden).is_none(),
            "node policy input declares path-owned field `{forbidden}`"
        );
    }
    let root = RuntimeRoot::new(app_root.to_path_buf());
    let trust_store = ryeos_engine::trust::TrustStore::load(None, &root.config())
        .context("load trust store for current node policies")?;
    // The replacement generation must stay the isolation policy of the host
    // class the node's signed substrate identity binds.
    let substrate = crate::load_verified_substrate_identity(app_root)
        .context("verify the node's substrate identity for policy apply")?;
    let update = ryeos_node_runtime::node_policy::generation::prepare_policy_member_replacement(
        app_root,
        &trust_store,
        &table,
        substrate.bound_host_class()?,
        section,
        body,
        Path::new("offline-operation-input/policy"),
    )?;
    Ok((trust_store, update))
}

/// The digest of the policy generation that replacing `section` with
/// `policy` would publish, for the request builder to pin as the result.
pub fn prospective_policy_apply_digest(
    app_root: &Path,
    section: &str,
    policy: &[u8],
) -> Result<String> {
    let (_, update) = plan_policy_apply(app_root, section, policy)?;
    Ok(update.generation().digest().to_owned())
}

pub(super) fn policy_apply(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: PolicyApplyArguments = ctx.arguments()?;
    let expected = ctx
        .expected("policy_generation")
        .context("the request pins no policy generation")?;
    let (trust_store, update) = plan_policy_apply(
        ctx.app_root,
        &arguments.section,
        ctx.inputs.require("policy")?,
    )?;
    ensure!(
        update.generation().digest() == expected,
        "the policy generation this would publish has digest {}, not the {expected} the request pinned; nothing was changed",
        update.generation().digest()
    );
    ryeos_node_runtime::node_policy::generation::publish_policy_update(
        ctx.app_root,
        &update,
        ctx.node,
        &trust_store,
        ctx.state_lock,
    )
    .context("atomically publish signed node policy generation")?;
    crate::seal_init_completion_after_policy_update(
        ctx.app_root,
        update.generation().digest(),
        ctx.state_lock,
    )
    .context("commit updated node policy generation into initialized state")?;
    Ok(json!({
        "section": arguments.section,
        "signer_fingerprint": ctx.node.fingerprint(),
        "policy_generation": expected,
    }))
}

// ── Composition ──────────────────────────────────────────────────────────

pub(super) fn external_compose(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: ComposeArguments = ctx.arguments()?;
    arguments.validate(&ctx.request.inputs)?;
    let settings: Value = serde_yaml::from_str(utf8(ctx.inputs.require("settings")?, "settings")?)
        .context("parse settings")?;
    let settings_digest = lillux::sha256_hex(lillux::canonical_json(&settings)?.as_bytes());
    if let Some(expected) = ctx.expected("settings_digest") {
        ensure!(
            settings_digest == expected,
            "the settings digest is {settings_digest}, not the {expected} the request pinned; nothing was changed"
        );
    }
    let generation = derived_credential_generation(ctx.request_digest);
    if installed_binding_credential_generation(ctx.app_root, &arguments.id)?.as_deref()
        == Some(generation.as_str())
    {
        return Ok(json!({
            "id": arguments.id,
            "credential_generation": generation,
            "settings_digest": settings_digest,
            "already_installed": true,
        }));
    }
    let program: AdmittedExternalCandidateProgram =
        read_json(ctx.inputs.require("program")?, "program")?;
    let limits: ComposeLimits = match ctx.inputs.get("limits") {
        Some(bytes) => read_json(bytes, "limits")?,
        None => ComposeLimits::default(),
    };
    let optional = |slot: &str| -> Result<Option<Value>> {
        ctx.inputs
            .get(slot)
            .map(|bytes| read_json(bytes, slot))
            .transpose()
    };
    let transport = match arguments.transport.as_str() {
        "https" => ComposeTransportRequest::Https {
            origin: arguments.origin.clone().context("origin")?,
            roots_pem: utf8(ctx.inputs.require("tls_roots")?, "TLS roots")?.to_owned(),
        },
        _ => ComposeTransportRequest::FileRelay {
            owner_workspace_id: arguments.owner_workspace_id.clone().context("owner")?,
            private_mailbox_parent: arguments
                .private_mailbox_parent
                .clone()
                .context("mailbox parent")?,
        },
    };
    let report = compose_external_execution_binding(ComposeRequest {
        app_root: ctx.app_root,
        state_lock: ctx.state_lock,
        id: arguments.id.clone(),
        backend: canonical_backend_id(&arguments.backend).to_owned(),
        program,
        transport,
        account: arguments.account.clone(),
        capacity_group: arguments
            .capacity_group
            .clone()
            .unwrap_or_else(|| DEFAULT_CAPACITY_GROUP.to_owned()),
        credential_generation: generation,
        settings,
        guest_runtime_manifest_hash: arguments.guest_runtime_manifest_hash.clone(),
        runtime_qualification: optional("runtime_qualification")?,
        runtime_content_qualification: optional("runtime_content_qualification")?,
        limits,
    })?;
    Ok(serde_json::to_value(report)?)
}

pub(super) fn snapshot_compose(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: SnapshotComposeArguments = ctx.arguments()?;
    arguments.validate(&ctx.request.inputs)?;
    let facts_source = ProviderFactsArguments {
        binding: &arguments.binding,
        backend: &arguments.backend,
        account: &arguments.account,
        owner_id: &arguments.owner_id,
    }
    .source(ctx.inputs)?;
    let generation = derived_credential_generation(ctx.request_digest);
    if installed_production_credential_generation(ctx.app_root, &arguments.id)?.as_deref()
        == Some(generation.as_str())
    {
        return Ok(json!({
            "id": arguments.id,
            "credential_generation": generation,
            "already_installed": true,
        }));
    }
    let defaults = ProductionLimits::default();
    let report = compose_runtime_snapshot_production_binding(ProductionComposeRequest {
        app_root: ctx.app_root,
        state_lock: ctx.state_lock,
        id: arguments.id.clone(),
        facts_source,
        snapshot_spec: ctx.inputs.require("provider_spec")?.to_vec(),
        sandbox_group_id: arguments.sandbox_group_id.clone(),
        region: arguments.region.clone(),
        source_plan: arguments.source_plan.clone(),
        credential_generation: generation,
        limits: ProductionLimits {
            contact_timeout_seconds: arguments
                .contact_timeout_seconds
                .unwrap_or(defaults.contact_timeout_seconds),
            maximum_bootstrap_lifetime_seconds: arguments
                .maximum_bootstrap_lifetime_seconds
                .unwrap_or(defaults.maximum_bootstrap_lifetime_seconds),
            maximum_upload_bytes: arguments
                .maximum_upload_bytes
                .unwrap_or(defaults.maximum_upload_bytes),
        },
    })?;
    Ok(serde_json::to_value(report)?)
}

pub(super) fn qualification_session_compose(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: QualificationSessionComposeArguments = ctx.arguments()?;
    arguments.validate(&ctx.request.inputs)?;
    let generation = derived_credential_generation(ctx.request_digest);
    if installed_binding_credential_generation(ctx.app_root, &arguments.id)?.as_deref()
        == Some(generation.as_str())
    {
        return Ok(serde_json::to_value(
            installed_qualification_binding_report(ctx.app_root, &arguments.id)?,
        )?);
    }
    let facts_source = ProviderFactsArguments {
        binding: &arguments.binding,
        backend: &arguments.backend,
        account: &arguments.account,
        owner_id: &arguments.owner_id,
    }
    .source(ctx.inputs)?;
    let limits: ComposeLimits = match ctx.inputs.get("limits") {
        Some(bytes) => read_json(bytes, "limits")?,
        None => ComposeLimits::default(),
    };
    let report = compose_qualification_session_binding(QualificationBindingRequest {
        app_root: ctx.app_root,
        state_lock: ctx.state_lock,
        id: arguments.id.clone(),
        facts_source,
        policy_ref: arguments.policy_ref.clone(),
        provider_declaration_id: arguments.provider_declaration_id.clone(),
        runtime_manifest_hash: arguments.runtime_manifest_hash.clone(),
        runtime_member_sha256: arguments.runtime_member_sha256.clone(),
        snapshot_id: arguments.snapshot_id.clone(),
        plan: arguments.plan.clone(),
        region: arguments.region.clone(),
        private_mailbox_parent: arguments.private_mailbox_parent.clone(),
        capacity_group: arguments
            .capacity_group
            .clone()
            .unwrap_or_else(|| DEFAULT_CAPACITY_GROUP.to_owned()),
        credential_generation: generation,
        limits,
    })?;
    Ok(serde_json::to_value(report)?)
}

pub(super) fn snapshot_qualification_compose(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: QualificationComposeArguments = ctx.arguments()?;
    let defaults = QualificationLimits::default();
    let consumer_verifiers = ctx
        .inputs
        .get("consumer_verifiers")
        .map(|bytes| read_json::<Value>(bytes, "consumer verifiers"))
        .transpose()?;
    let report = compose_runtime_snapshot_qualification_binding(QualificationComposeRequest {
        app_root: ctx.app_root,
        state_lock: ctx.state_lock,
        id: arguments.id.clone(),
        snapshot_binding_id: arguments.snapshot_binding.clone(),
        provider_spec: ctx.inputs.require("provider_spec")?.to_vec(),
        verifier_artifact_hash: arguments.verifier.clone(),
        consumer_verifiers,
        limits: QualificationLimits {
            contact_timeout_seconds: arguments
                .contact_timeout_seconds
                .unwrap_or(defaults.contact_timeout_seconds),
            maximum_lifetime_seconds: arguments
                .maximum_lifetime_seconds
                .unwrap_or(defaults.maximum_lifetime_seconds),
        },
    })?;
    Ok(serde_json::to_value(report)?)
}

// ── Capacity and restart ─────────────────────────────────────────────────

pub(super) fn capacity_reissue(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: CapacityArguments = ctx.arguments()?;
    let operator_key = operator_key(ctx)?;
    let command = ctx.command_label();
    let report =
        crate::native_capacity::reissue_capacity(crate::native_capacity::ReissueRequest {
            app_root: ctx.app_root,
            state_lock: ctx.state_lock,
            operator_key: &operator_key,
            reason: &arguments.reason,
            command: &command,
        })?;
    Ok(serde_json::to_value(report)?)
}

pub(super) fn capacity_reanchor(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: CapacityArguments = ctx.arguments()?;
    let operator_key = operator_key(ctx)?;
    let command = ctx.command_label();
    let report =
        crate::native_capacity::reanchor_capacity(crate::native_capacity::ReanchorRequest {
            app_root: ctx.app_root,
            state_lock: ctx.state_lock,
            operator_key: &operator_key,
            reason: &arguments.reason,
            command: &command,
        })?;
    Ok(serde_json::to_value(report)?)
}

pub(super) fn allow_restart(ctx: &HandlerContext<'_>) -> Result<Value> {
    let _: EmptyArguments = ctx.arguments()?;
    let cleared = crate::lifecycle_marker::clear_restart_inhibition(
        ctx.app_root,
        ctx.state_lock,
        &ctx.command_label(),
    )?;
    Ok(json!({ "cleared": cleared }))
}

// ── Resets ───────────────────────────────────────────────────────────────

pub(super) fn reset_execution_history(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: ExecutionHistoryResetArguments = ctx.arguments()?;
    if let Some(cut) = &arguments.schema_cut {
        ensure!(
            cut.from != cut.to,
            "execution-history schema cut must name distinct epochs"
        );
    }
    let options = ExecutionHistoryResetOptions {
        app_root: Some(ctx.app_root.to_path_buf()),
        dry_run: false,
        discard_project_heads: arguments.discard_project_heads,
        schema_cut: arguments.schema_cut.map(|cut| ExecutionHistorySchemaCut {
            from: cut.from,
            to: cut.to,
        }),
        discard_unreadable_lifetime_fence_after_reboot: arguments
            .discard_unreadable_lifetime_fence_after_reboot,
    };
    // The installed composition decides whether the scheduler takes part, as
    // daemon startup decides it. An unselected scheduler is never opened.
    let participant = ctx.host.scheduler_history_participant(ctx.app_root)?;
    let report = run_execution_history_reset_with_progress_locked(
        &options,
        participant.as_deref(),
        &mut |event| ctx.host.observe_execution_history_reset(event),
        ctx.state_lock,
    )
    .context("offline execution-history reset failed")?;
    ctx.host
        .observe_report(TypedReport::ExecutionHistoryReset(&report));
    Ok(serde_json::to_value(report)?)
}

pub(super) fn reset_execution_history_scratch(ctx: &HandlerContext<'_>) -> Result<Value> {
    let _: EmptyArguments = ctx.arguments()?;
    let report = repair_execution_history_scratch_locked(
        &ExecutionHistoryScratchRepairOptions {
            app_root: Some(ctx.app_root.to_path_buf()),
            dry_run: false,
        },
        ctx.state_lock,
    )
    .context("execution-history scratch repair failed")?;
    ctx.host
        .observe_report(TypedReport::ExecutionHistoryScratch(&report));
    Ok(serde_json::to_value(report)?)
}

pub(super) fn reset_replay_indexes(ctx: &HandlerContext<'_>) -> Result<Value> {
    let _: EmptyArguments = ctx.arguments()?;
    let config = config(ctx)?;
    let activation = ryeos_app::offline_native::reset_replay_indexes(&config, ctx.state_lock)
        .context("activate current replay indexes through the offline native host")?;
    ctx.host
        .observe_report(TypedReport::ReplayIndexes(&activation.report));
    Ok(json!({
        "database": activation.database,
        "replay_indexes": activation.report,
        "native_connections": activation.facts.tickets_issued,
    }))
}

pub(super) fn reset_external_content_bindings(ctx: &HandlerContext<'_>) -> Result<Value> {
    let _: EmptyArguments = ctx.arguments()?;
    let config = config(ctx)?;
    let reset =
        ryeos_app::offline_native::reset_external_content_bindings(&config, ctx.state_lock, false)
            .context("reset external-content bindings through the offline native host")?;
    Ok(json!({
        "bindings": reset.bindings,
        "native_connections": reset.facts.tickets_issued,
    }))
}

// ── Authorization reset ──────────────────────────────────────────────────

/// Grants a receipt can list; a node with more refuses the reset rather than
/// write a receipt it cannot hold.
const MAX_DISCARDED_GRANTS: usize = 1024;
const MAX_GRANT_BYTES: u64 = 1024 * 1024;

pub(super) fn authorization_reset_prepare(ctx: &HandlerContext<'_>) -> Result<Prepared> {
    let _: EmptyArguments = ctx.arguments()?;
    let directory = super::envelope::authorized_keys_dir(ctx.app_root);
    let mut digests = Vec::new();
    if let Some(pinned) = lillux::PinnedDirectory::open(&directory)? {
        let mut entries = pinned.entries_no_follow()?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        ensure!(
            entries.len() <= MAX_DISCARDED_GRANTS,
            "the authorized-key namespace holds more than {MAX_DISCARDED_GRANTS} entries"
        );
        for entry in entries {
            let digest = match entry.entry_type {
                lillux::PinnedEntryType::Regular => {
                    let file = pinned
                        .open_regular(&entry.name, false)?
                        .context("grant vanished while the reset listed it")?;
                    lillux::sha256_hex(&lillux::read_open_regular_file_bounded(
                        file,
                        MAX_GRANT_BYTES,
                    )?)
                }
                _ => lillux::sha256_hex(
                    format!("non-regular:{}", entry.name.to_string_lossy()).as_bytes(),
                ),
            };
            digests.push(digest);
        }
    }
    // The operator must be loadable before the receipt promises the swap.
    operator_key(ctx)?;
    Ok(Prepared {
        discarded_grant_digests: digests,
    })
}

pub(super) fn authorization_reset_effect(ctx: &HandlerContext<'_>) -> Result<Value> {
    let node = ctx.node;
    let operator = NodeIdentity::load(
        &RuntimeRoot::new(ctx.app_root.to_path_buf()).operator_signing_key_path(),
    )
    .context("load operator identity for authorization reset")?;
    let authorized_keys_dir = super::envelope::authorized_keys_dir(ctx.app_root);
    let parent = authorized_keys_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("authorized-key namespace has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    if let Some(entry_type) = lillux::inspect_optional_entry_no_follow(&authorized_keys_dir)? {
        ensure!(
            entry_type == lillux::PinnedEntryType::Directory,
            "authorized-key namespace is not a regular directory: {}",
            authorized_keys_dir.display()
        );
    }
    // The staging name is this operation's own. A staging tree left by a run
    // that died is removed: the state lock is held, so nothing else is using it.
    let staging = parent.join(".authorized_keys.offline-reset");
    if lillux::inspect_optional_entry_no_follow(&staging)?.is_some() {
        lillux::remove_dir_all_durable(&staging)
            .context("remove authorization reset staging left by an interrupted run")?;
    }
    std::fs::create_dir(&staging)?;
    let operator_key = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        operator.verifying_key().as_bytes(),
    );
    ryeos_node_runtime::identity::write_authorized_key_toml(
        &staging,
        operator.fingerprint(),
        &operator_key,
        &["*".to_string()],
        "bootstrap-authorized-user",
        operator.fingerprint(),
        &lillux::time::iso8601_now(),
        node.signing_key(),
        ryeos_node_runtime::identity::WildcardPolicy::AllowBootstrap,
    )?;
    lillux::sync_tree_durable(&staging)
        .context("durably flush reset authorized-key namespace before publication")?;
    if lillux::inspect_optional_entry_no_follow(&authorized_keys_dir)?.is_some() {
        // Exchange keeps the live namespace present at every instant. The
        // exchanged staging path becomes the retired tree only after the new
        // namespace is durably published in the parent directory.
        lillux::atomic_exchange_paths(&authorized_keys_dir, &staging)
            .map_err(anyhow::Error::from)
            .context("atomically publish reset authorized-key namespace")?;
        lillux::remove_dir_all_durable(&staging)
            .context("durably remove retired authorized-key namespace")?;
    } else {
        lillux::rename_path_noreplace_durable(&staging, &authorized_keys_dir)
            .map_err(anyhow::Error::from)
            .context("durably publish initial authorized-key namespace")?;
    }
    Ok(json!({
        "operator_fingerprint": operator.fingerprint(),
        "authorized_keys_dir": authorized_keys_dir,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offline_operation::contract::{REQUEST_SCHEMA, RequestBody};
    use crate::offline_operation::host::BareHost;
    use crate::offline_operation::table::VerifiedInputs;
    use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};

    #[test]
    fn a_snapshot_compose_names_one_source_of_provider_facts() {
        let arguments = |binding: Option<&str>, backend: bool, account: bool, owner: bool| {
            SnapshotComposeArguments {
                id: "producer".into(),
                binding: binding.map(str::to_owned),
                backend: backend.then(|| "render".to_owned()),
                account: account.then(|| "acct".to_owned()),
                owner_id: owner.then(|| "usr-1".to_owned()),
                sandbox_group_id: "sbg-1".into(),
                region: "oregon".into(),
                source_plan: "standard".into(),
                contact_timeout_seconds: None,
                maximum_bootstrap_lifetime_seconds: None,
                maximum_upload_bytes: None,
            }
        };
        let none = BTreeMap::new();
        let roots = BTreeMap::from([("tls_roots".to_owned(), "f".repeat(64))]);
        assert!(
            arguments(Some("operator"), false, false, false)
                .validate(&none)
                .is_ok()
        );
        assert!(arguments(None, true, true, true).validate(&roots).is_ok());
        let mixed = [
            arguments(Some("operator"), true, false, false).validate(&none),
            arguments(Some("operator"), false, false, false).validate(&roots),
        ];
        for refused in mixed {
            assert!(format!("{:#}", refused.unwrap_err()).contains("do not state them beside it"));
        }
        let incomplete = [
            arguments(None, true, true, true).validate(&none),
            arguments(None, true, true, false).validate(&roots),
            arguments(None, false, false, false).validate(&none),
        ];
        for refused in incomplete {
            assert!(format!("{:#}", refused.unwrap_err()).contains("must all be stated"));
        }
    }

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("bundles").is_dir())
            .expect("workspace root with bundles/ directory")
            .to_path_buf()
    }

    #[test]
    fn a_policy_apply_rerun_after_a_crash_reproduces_the_pinned_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        crate::init::run_init(&crate::InitOptions {
            app_root: state.clone(),
            bind: None,
            uds_path: None,
            source_dir: workspace_root().join("bundles"),
            trust_files: vec![workspace_root().join(".dev-keys/PUBLISHER_DEV_TRUST.toml")],
            node_profile: Some("full".to_owned()),
            host_class: Some("local".to_owned()),
            substrate_identity: Some(
                crate::SubstrateIdentity::new(
                    format!("sha256:{}", "1".repeat(64)),
                    1,
                    Some("full".to_owned()),
                    Some("local".to_owned()),
                )
                .unwrap(),
            ),
            replace_node_policy_generation: false,
            replace_substrate_identity: false,
            confirm_substrate_identity_replacement: false,
            skip_preflight: true,
        })
        .expect("initial generation");
        let policy = b"schema: 2\npatterns:\n  - \"*.trace\"\n".to_vec();
        let pinned = prospective_policy_apply_digest(&state, "ingest_ignore", &policy).unwrap();
        let node =
            NodeIdentity::load(&RuntimeRoot::new(state.clone()).node_signing_key_path()).unwrap();
        let lock = StateLock::acquire(&default_lock_path(&state)).unwrap();
        let request = RequestBody {
            schema: REQUEST_SCHEMA.to_owned(),
            node_fingerprint: node.fingerprint().to_owned(),
            signer_fingerprint: "b".repeat(64),
            signer_grant_digest: None,
            signer_origin_site_id: None,
            operation: "node.policy-apply".to_owned(),
            arguments: json!({ "section": "ingest_ignore" }),
            inputs: BTreeMap::new(),
            expected_result: BTreeMap::from([("policy_generation".to_owned(), pinned.clone())]),
            precondition: BTreeMap::new(),
            issued_at: "2026-10-28T00:00:00Z".to_owned(),
            not_after: "2026-10-28T01:00:00Z".to_owned(),
        };
        let inputs = VerifiedInputs::new(BTreeMap::from([("policy".to_owned(), policy)]));
        let context = HandlerContext {
            app_root: &state,
            state_lock: &lock,
            request: &request,
            request_digest: &"c".repeat(64),
            inputs: &inputs,
            host: &BareHost,
            node: &node,
        };
        policy_apply(&context).expect("first application");
        let first = crate::verify_init_completion(&state).unwrap().unwrap();
        assert_eq!(first.policy_generation_digest, pinned);
        // The node died before the receipt: the very same request runs again.
        policy_apply(&context).expect("rerun after the crash");
        let second = crate::verify_init_completion(&state).unwrap().unwrap();
        assert_eq!(second.policy_generation_digest, pinned);
    }
}
