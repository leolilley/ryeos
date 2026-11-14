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
    compose_external_execution_binding, installed_binding_credential_generation,
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
    /// The operator's keep/reset choices for sections the replacement would
    /// otherwise refuse to decide. Absent when there are none, so a request
    /// without choices is byte-identical to one made before they existed.
    #[serde(
        default,
        skip_serializing_if = "ryeos_node_runtime::node_policy::generation::PolicyCarryChoices::is_empty"
    )]
    pub policy_carry: ryeos_node_runtime::node_policy::generation::PolicyCarryChoices,
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
    /// Measured guest-owner runtime of the provider snapshot; defaults to the
    /// runtime manifest when the qualified runtime is the snapshot's owner.
    #[serde(default)]
    pub guest_runtime_manifest_hash: Option<String>,
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
    pub delivery_timeout_seconds: Option<u32>,
    pub maximum_lifetime_seconds: Option<u32>,
}

/// Recompose installed bindings against the installed bundles. An empty
/// list recomposes every binding whose pins the installed bundles fail; the
/// optional `consumer_verifiers` input carries the running node's derive.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecomposeArguments {
    #[serde(default)]
    pub bindings: Vec<ryeos_app::node_config::recompose::BindingRef>,
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

/// The principal class a delegated grant writes. Only the remote classes:
/// a local-client grant carries no origin binding, so it is never written on
/// a remote signer's authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantClass {
    /// The node key of a remote RyeOS node, bound to its site.
    RemoteNode,
    /// An operator key forwarded by a remote RyeOS node of one origin site.
    RemotePrincipal,
}

/// Most scopes one grant operation may name.
pub const MAX_GRANT_SCOPES: usize = 64;

/// Reconcile one node-signed grant for a remote node or a remote operator,
/// with the same core `ryeos authorize-client` uses.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeClientArguments {
    /// The raw 32-byte Ed25519 public key, base64.
    pub public_key: String,
    pub scopes: Vec<String>,
    pub label: String,
    pub class: GrantClass,
    /// The site the grant is bound to: the remote node's own site, or the
    /// site whose node forwards the operator.
    pub origin_site_id: String,
    /// Union the scopes with the grant already on disk instead of replacing
    /// them.
    #[serde(default)]
    pub merge_scopes: bool,
    /// Allow changing an existing grant's class or origin site. The node is
    /// stopped while the operation applies, which that change requires.
    #[serde(default)]
    pub allow_semantic_conversion: bool,
    /// Requested grant lifetime in seconds. Absent: the class maximum of the
    /// node's signed authorization policy. Above that maximum: refused.
    #[serde(default)]
    pub lifetime_secs: Option<u64>,
}

impl AuthorizeClientArguments {
    pub fn validate(&self) -> Result<()> {
        self.verifying_key()?;
        ensure!(!self.scopes.is_empty(), "scopes must not be empty");
        ensure!(
            self.scopes.len() <= MAX_GRANT_SCOPES,
            "more than {MAX_GRANT_SCOPES} scopes"
        );
        ensure!(
            !self.label.trim().is_empty() && self.label.len() <= 128,
            "the label must be 1 to 128 bytes"
        );
        ensure!(
            !self.scopes.iter().any(|scope| scope.contains('*')),
            "a delegated grant names exact scopes; wildcards are refused"
        );
        // Offline-operation scopes are granted only by the node's local
        // operator: a delegated grant cannot widen who may sign them.
        ryeos_node_runtime::identity::reject_offline_scopes(&self.scopes)?;
        ryeos_node_runtime::identity::validate_canonical_site_id(&self.origin_site_id)
            .context("origin site id is not canonical")?;
        Ok(())
    }

    fn verifying_key(&self) -> Result<lillux::crypto::VerifyingKey> {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.public_key)
            .context("the public key is not base64")?;
        let bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .context("the public key is not 32 bytes (Ed25519)")?;
        lillux::crypto::VerifyingKey::from_bytes(&bytes).context("the public key is not Ed25519")
    }
}

/// Pin a remote node as a trust anchor from its descriptor (the `descriptor`
/// input), without contacting it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePinArguments {
    /// The name the remote is pinned under.
    pub name: String,
}

impl RemotePinArguments {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 64
                && self
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'),
            "remote name `{}` is not a plain name",
            self.name
        );
        Ok(())
    }
}

/// Install a bundle from an archive (the `archive` input), replacing the
/// installed bundle of that name when there is one.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleReplaceArguments {
    pub name: String,
    /// Keep the installed `.ai/bin`, `.ai/objects` and `.ai/refs` when the
    /// archive omits them.
    #[serde(default)]
    pub preserve_runtime_artifacts: bool,
}

impl BundleReplaceArguments {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 64
                && self.name.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'-'),
            "bundle name `{}` is not 1-64 lowercase letters, digits, `_` or `-`",
            self.name
        );
        Ok(())
    }
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

// ── Trust ────────────────────────────────────────────────────────────────

/// Role and scope a signer is pinned with. Absent lists mean the role's
/// default.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustPinArguments {
    pub role: String,
    #[serde(default)]
    pub spaces: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub projects: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustScopeArguments {
    pub fingerprint: String,
    pub spaces: Vec<String>,
    pub kinds: Vec<String>,
    #[serde(default)]
    pub projects: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustFingerprintArguments {
    pub fingerprint: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustSucceedArguments {
    pub old_fingerprint: String,
}

fn trust_scope(
    role: ryeos_engine::trust::SignerRole,
    spaces: &[String],
    kinds: &[String],
    projects: &[String],
) -> Result<ryeos_engine::trust::SignerScope> {
    use ryeos_engine::trust::{SignerRole, SignerScope, SignerSpace};
    let mut scope = match role {
        SignerRole::Publisher => SignerScope::publisher_default(),
        SignerRole::Operator => SignerScope::operator_default(),
        SignerRole::Authoring => SignerScope::authoring_default(),
        SignerRole::ProjectAuthor => SignerScope {
            spaces: vec![SignerSpace::Project],
            kinds: vec![ryeos_engine::trust::ALL_KINDS.to_string()],
            projects: Vec::new(),
        },
    };
    if !spaces.is_empty() {
        scope.spaces = spaces
            .iter()
            .map(|space| {
                SignerSpace::parse(space).with_context(|| {
                    format!("`{space}` is not a signer space (bundle, project, node)")
                })
            })
            .collect::<Result<_>>()?;
    }
    if !kinds.is_empty() {
        scope.kinds = kinds.to_vec();
    }
    scope.projects = projects.to_vec();
    scope
        .validate()
        .map_err(|reason| anyhow::anyhow!("trust_scope_invalid: {reason}"))?;
    Ok(scope)
}

fn validate_trust_scope_lists(
    spaces: &[String],
    kinds: &[String],
    projects: &[String],
) -> Result<()> {
    ensure!(
        spaces.len() <= 3 && kinds.len() <= 64 && projects.len() <= 256,
        "scope lists are too long"
    );
    Ok(())
}

impl TrustPinArguments {
    pub fn validate(&self) -> Result<()> {
        let role = ryeos_engine::trust::SignerRole::parse(&self.role)
            .with_context(|| format!("`{}` is not a signer role", self.role))?;
        validate_trust_scope_lists(&self.spaces, &self.kinds, &self.projects)?;
        trust_scope(role, &self.spaces, &self.kinds, &self.projects).map(|_| ())
    }
}

impl TrustScopeArguments {
    pub fn validate(&self) -> Result<()> {
        ensure!(is_digest(&self.fingerprint), "fingerprint is not a digest");
        validate_trust_scope_lists(&self.spaces, &self.kinds, &self.projects)?;
        let scope = ryeos_engine::trust::SignerScope {
            spaces: self
                .spaces
                .iter()
                .map(|s| {
                    ryeos_engine::trust::SignerSpace::parse(s)
                        .with_context(|| format!("`{s}` is not a signer space"))
                })
                .collect::<Result<_>>()?,
            kinds: self.kinds.clone(),
            projects: self.projects.clone(),
        };
        scope
            .validate()
            .map_err(|reason| anyhow::anyhow!("trust_scope_invalid: {reason}"))
    }
}

impl TrustFingerprintArguments {
    pub fn validate(&self) -> Result<()> {
        ensure!(is_digest(&self.fingerprint), "fingerprint is not a digest");
        Ok(())
    }
}

fn trust_actor<'a>(
    ctx: &'a HandlerContext<'_>,
    operator: &'a str,
) -> crate::trust_registry::Actor<'a> {
    crate::trust_registry::Actor {
        node_key: ctx.node.signing_key(),
        operator,
        request_digest: ctx.request_digest,
    }
}

fn run_trust_change(
    ctx: &HandlerContext<'_>,
    change: crate::trust_registry::Change,
) -> Result<Value> {
    let operator = operator_key(ctx)?;
    let operator_fp = ryeos_engine::trust::compute_fingerprint(&operator.verifying_key());
    let outcome = crate::trust_registry::apply_change(
        ctx.app_root,
        ctx.state_lock,
        &trust_actor(ctx, &operator_fp),
        &change,
    )?;
    Ok(json!({
        "records": outcome.records,
        "events_written": outcome.events_written,
        "registry_digest": outcome.registry_digest,
    }))
}

pub(super) fn trust_pin(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: TrustPinArguments = ctx.arguments()?;
    arguments.validate()?;
    let role = ryeos_engine::trust::SignerRole::parse(&arguments.role).context("signer role")?;
    let scope = trust_scope(
        role,
        &arguments.spaces,
        &arguments.kinds,
        &arguments.projects,
    )?;
    let doc = ryeos_engine::trust::PublisherTrustDoc::parse(utf8(
        ctx.inputs.require("signer")?,
        "signer trust document",
    )?)
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let key = doc
        .decode_verifying_key()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    run_trust_change(
        ctx,
        crate::trust_registry::Change::Pin {
            key,
            label: doc.owner,
            role,
            scope,
        },
    )
}

pub(super) fn trust_scope_change(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: TrustScopeArguments = ctx.arguments()?;
    arguments.validate()?;
    let scope = ryeos_engine::trust::SignerScope {
        spaces: arguments
            .spaces
            .iter()
            .map(|s| {
                ryeos_engine::trust::SignerSpace::parse(s)
                    .with_context(|| format!("`{s}` is not a signer space"))
            })
            .collect::<Result<_>>()?,
        kinds: arguments.kinds,
        projects: arguments.projects,
    };
    run_trust_change(
        ctx,
        crate::trust_registry::Change::Scope {
            fingerprint: arguments.fingerprint,
            scope,
        },
    )
}

pub(super) fn trust_retire(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: TrustFingerprintArguments = ctx.arguments()?;
    arguments.validate()?;
    run_trust_change(
        ctx,
        crate::trust_registry::Change::Retire {
            fingerprint: arguments.fingerprint,
        },
    )
}

pub(super) fn trust_revoke(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: TrustFingerprintArguments = ctx.arguments()?;
    arguments.validate()?;
    run_trust_change(
        ctx,
        crate::trust_registry::Change::Revoke {
            fingerprint: arguments.fingerprint,
        },
    )
}

pub(super) fn trust_succeed(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: TrustSucceedArguments = ctx.arguments()?;
    ensure!(
        is_digest(&arguments.old_fingerprint),
        "old fingerprint is not a digest"
    );
    let statement = utf8(ctx.inputs.require("succession")?, "succession statement")?.to_owned();
    run_trust_change(
        ctx,
        crate::trust_registry::Change::Succeed {
            old_fingerprint: arguments.old_fingerprint,
            statement,
        },
    )
}

// ── Key rotation ─────────────────────────────────────────────────────────

fn rotate_key(ctx: &HandlerContext<'_>, key: crate::key_rotation::RotatedKey) -> Result<Value> {
    let _: EmptyArguments = ctx.arguments()?;
    let statement = utf8(ctx.inputs.require("succession")?, "succession statement")?;
    crate::key_rotation::rotate(
        key,
        crate::key_rotation::RotationRequest {
            app_root: ctx.app_root,
            state_lock: ctx.state_lock,
            node_key: ctx.node.signing_key(),
            request_digest: ctx.request_digest,
            signer_fingerprint: &ctx.request.signer_fingerprint,
            statement,
        },
    )
}

pub(super) fn operator_rotate(ctx: &HandlerContext<'_>) -> Result<Value> {
    rotate_key(ctx, crate::key_rotation::RotatedKey::Operator)
}

pub(super) fn authoring_rotate(ctx: &HandlerContext<'_>) -> Result<Value> {
    rotate_key(ctx, crate::key_rotation::RotatedKey::Authoring)
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
            policy_carry: arguments.policy_carry.clone(),
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
        "capacity_amendment": report.capacity_amendment,
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
    let trust_store = ryeos_engine::trust::TrustStore::load_node_registry(app_root)
        .context("load trust store for current node policies")?;
    if section == ryeos_node_runtime::node_policy::sections::host_access::SECTION_NAME {
        check_host_access_signers(app_root, &trust_store, &body)?;
    }
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

/// A host-access grant hands its item operator-equivalent authority, so its
/// signer must be pinned in the node trust store and must not be the node's
/// own key: runtime-authored items are node-signed and never gain host access.
fn check_host_access_signers(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    body: &Value,
) -> Result<()> {
    let policy: ryeos_node_runtime::node_policy::sections::host_access::NodeHostAccessPolicy =
        serde_json::from_value(body.clone()).context("parse node host_access policy input")?;
    let node_key = ryeos_node_runtime::identity::node_identity_fingerprint(app_root)?;
    for grant in &policy.grants {
        ensure!(
            grant.signer != node_key,
            "{}: host_access grant `{}` names this node's own key {} as signer; runtime-authored items never gain host access",
            ryeos_node_runtime::node_policy::sections::host_access::HostAccessRefusal::SIGNER_REFUSED,
            grant.id,
            grant.signer
        );
        ensure!(
            trust_store.is_trusted(&grant.signer),
            "{}: host_access grant `{}` names signer {}, which is not pinned in this node's trust store",
            ryeos_node_runtime::node_policy::sections::host_access::HostAccessRefusal::SIGNER_REFUSED,
            grant.id,
            grant.signer
        );
    }
    Ok(())
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
    // The capacity issuance pins the `native_capacity` policy it was granted
    // under, so that section cannot change by publication alone: the
    // operator amends the issuance to the new policy first, under the same
    // lock, so the republished generation and the account agree when the
    // completion is resealed below. A rerun converges on the same amendment.
    let capacity_amendment = if arguments.section
        == ryeos_node_runtime::node_policy::sections::native_capacity::SECTION_NAME
    {
        let body = update
            .generation()
            .policies()
            .get(&arguments.section)
            .context("the prepared policy generation lacks its native_capacity member")?;
        let policy: ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy =
            serde_json::from_value(body.clone())
                .context("parse the prepared native_capacity policy")?;
        let operator_key = operator_key(ctx)?;
        Some(crate::native_capacity::amend_capacity_policy(
            crate::native_capacity::CapacityPolicyAmendmentRequest {
                app_root: ctx.app_root,
                state_lock: ctx.state_lock,
                operator_key: &operator_key,
                policy: &policy,
                reason: &format!("node policy generation {expected}"),
                command: &ctx.command_label(),
            },
        )?)
    } else {
        None
    };
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
        "capacity_amendment": capacity_amendment,
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
        backend: arguments.backend.clone(),
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
        guest_runtime_manifest_hash: arguments
            .guest_runtime_manifest_hash
            .clone()
            .unwrap_or_else(|| arguments.runtime_manifest_hash.clone()),
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
            delivery_timeout_seconds: arguments
                .delivery_timeout_seconds
                .unwrap_or(defaults.delivery_timeout_seconds),
            maximum_lifetime_seconds: arguments
                .maximum_lifetime_seconds
                .unwrap_or(defaults.maximum_lifetime_seconds),
        },
    })?;
    Ok(serde_json::to_value(report)?)
}

pub(super) fn recompose(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: RecomposeArguments = ctx.arguments()?;
    let consumer_verifiers = ctx
        .inputs
        .get("consumer_verifiers")
        .map(|bytes| read_json(bytes, "consumer verifiers"))
        .transpose()?;
    let report = ryeos_app::node_config::recompose::recompose_bindings(
        ryeos_app::node_config::recompose::RecomposeRequest {
            app_root: ctx.app_root,
            state_lock: ctx.state_lock,
            bindings: arguments.bindings,
            consumer_verifiers,
            dry_run: false,
        },
    )?;
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

pub(super) fn purge_quarantined_large_objects(ctx: &HandlerContext<'_>) -> Result<Value> {
    let _: EmptyArguments = ctx.arguments()?;
    let config = config(ctx)?;
    let purged = ryeos_app::operator_external_content::purge_quarantined_large_objects(
        &config,
        ctx.state_lock,
        false,
    )
    .context("purge the large-object quarantine")?;
    Ok(serde_json::to_value(purged)?)
}

// ── Grants, remotes and bundles ──────────────────────────────────────────

pub(super) fn authorize_client(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: AuthorizeClientArguments = ctx.arguments()?;
    arguments.validate()?;
    let public_key = arguments.verifying_key()?;
    let fingerprint = lillux::crypto::fingerprint(&public_key);
    // The configured operator's grant never lapses and is managed by init and
    // the authorization reset; a delegated grant must not rewrite it.
    let operator = NodeIdentity::load(
        &RuntimeRoot::new(ctx.app_root.to_path_buf()).operator_signing_key_path(),
    )
    .context("load operator identity to guard the operator grant")?;
    ensure!(
        fingerprint != operator.fingerprint(),
        "grant_is_operator: the configured operator's grant is managed by `ryeos init` and \
         `ryeos node reset authorization`; it cannot be written through node.authorize-client"
    );
    let class = match arguments.class {
        GrantClass::RemoteNode => {
            ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::RemoteNode
        }
        GrantClass::RemotePrincipal => {
            ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::RemotePrincipal
        }
    };
    // The grant's lifetime is bounded by the node's signed policy; the
    // request's signer and digest are the evidence of who asked.
    let policy =
        ryeos_node_runtime::node_policy::sections::authorization::NodeAuthorizationPolicy::load_offline(
            ctx.app_root,
            ctx.node,
        )?;
    let terms = ryeos_node_runtime::identity::GrantTerms {
        expires_at: policy
            .expiry_for(
                class,
                arguments.lifetime_secs,
                lillux::time::unix_secs_now(),
            )
            .map_err(|error| anyhow::anyhow!("{}: {error}", error.code()))?,
        authorized_by: ryeos_node_runtime::identity::AuthorizedBy::principal_request(
            &ctx.request.signer_fingerprint,
            Some(ctx.request_digest.to_owned()),
        ),
    };
    let subject = match arguments.class {
        GrantClass::RemoteNode => ryeos_node_runtime::identity::AuthorizedKeySubject::RemoteNode {
            origin_site_id: &arguments.origin_site_id,
        },
        GrantClass::RemotePrincipal => {
            ryeos_node_runtime::identity::AuthorizedKeySubject::RemotePrincipal {
                origin_site_id: &arguments.origin_site_id,
            }
        }
    };
    // The canonical reconciliation, under the state lock the application
    // holds: the node is stopped, so a class or origin change is allowed
    // when the request asks for it.
    let (path, dropped_scopes, transition) =
        ryeos_node_runtime::identity::reconcile_authorized_key_toml_scopes_for_subject(
            &super::envelope::authorized_keys_dir(ctx.app_root),
            &fingerprint,
            &arguments.public_key,
            &arguments.scopes,
            &arguments.label,
            &ctx.command_label(),
            &lillux::time::iso8601_now(),
            &terms,
            ctx.node,
            ryeos_node_runtime::identity::WildcardPolicy::Reject,
            arguments.merge_scopes,
            subject,
            arguments.allow_semantic_conversion,
        )
        .context("reconcile the authorized-key grant")?;
    Ok(json!({
        "fingerprint": fingerprint,
        "path": path,
        "principal_class": transition.principal_class.as_str(),
        "origin_site_id": transition.origin_site_id,
        "previous_principal_class": transition
            .previous_principal_class
            .map(|class| class.as_str().to_owned()),
        "previous_origin_site_id": transition.previous_origin_site_id,
        "dropped_scopes": dropped_scopes,
        "merged": arguments.merge_scopes,
        "expires_at": terms.expires_at.to_grant_text(),
    }))
}

pub(super) fn remote_pin(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: RemotePinArguments = ctx.arguments()?;
    arguments.validate()?;
    ctx.host.pin_remote(super::host::RemotePin {
        app_root: ctx.app_root,
        state_lock: ctx.state_lock,
        node: ctx.node,
        name: &arguments.name,
        descriptor: ctx.inputs.require("descriptor")?,
    })
}

pub(super) fn bundle_replace(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: BundleReplaceArguments = ctx.arguments()?;
    arguments.validate()?;
    ctx.host.replace_bundle(super::host::BundleReplace {
        app_root: ctx.app_root,
        state_lock: ctx.state_lock,
        node: ctx.node,
        name: &arguments.name,
        archive: ctx.inputs.require("archive")?,
        preserve_runtime_artifacts: arguments.preserve_runtime_artifacts,
    })
}

// ── Bundle state homes ───────────────────────────────────────────────────

/// Export one namespace, or every namespace, of a bundle's state home as a
/// signed archive written to a path the operator names.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleStateExportArguments {
    pub bundle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Absolute path of the archive to create; it must not exist.
    pub output: String,
}

impl BundleStateExportArguments {
    pub fn validate(&self) -> Result<()> {
        validate_state_target(&self.bundle_id, self.namespace.as_deref())?;
        ensure!(
            Path::new(&self.output).is_absolute()
                && Path::new(&self.output).file_name().is_some()
                && !self.output.contains('\0'),
            "the export output must be an absolute path naming a new file"
        );
        Ok(())
    }
}

/// Install a signed archive's namespaces into the node's state homes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleStateImportArguments {
    /// Namespaces of the archive the operator lets replace an existing,
    /// different namespace. Any other existing namespace refuses the import.
    #[serde(default)]
    pub replace_namespaces: Vec<String>,
}

impl BundleStateImportArguments {
    pub fn validate(&self) -> Result<()> {
        for namespace in &self.replace_namespaces {
            ryeos_bundle::runtime_authority::validate_state_home_segment(
                "replace namespace",
                namespace,
            )
            .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
}

/// Delete one namespace, or a bundle's whole state home.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleStateRemoveArguments {
    pub bundle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

impl BundleStateRemoveArguments {
    pub fn validate(&self) -> Result<()> {
        validate_state_target(&self.bundle_id, self.namespace.as_deref())
    }
}

fn validate_state_target(bundle_id: &str, namespace: Option<&str>) -> Result<()> {
    ryeos_state::objects::validate_bundle_identifier("bundle_id", bundle_id)
        .map_err(|error| anyhow::anyhow!("{error:#}"))?;
    if let Some(namespace) = namespace {
        ryeos_bundle::runtime_authority::validate_state_home_segment("namespace", namespace)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

pub(super) fn bundle_state_export(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: BundleStateExportArguments = ctx.arguments()?;
    arguments.validate()?;
    let output = Path::new(&arguments.output);
    let parent = output.parent().context("the export output has no parent directory")?;
    let name = output.file_name().context("the export output names no file")?;
    let directory = lillux::PinnedDirectory::open(parent)?
        .with_context(|| format!("the export output directory {} does not exist", parent.display()))?;
    if directory.entry_no_follow(name)?.is_some() {
        // A node that died after writing the archive and before the receipt
        // finds its own archive here: the same bundle, signed by this node.
        let existing = lillux::read_regular_file_bounded_no_follow(
            output,
            ryeos_app::bundle_state_transfer::MAX_ARCHIVE_BYTES,
        )
        .with_context(|| {
            format!("{} already exists and is not a readable archive", output.display())
        })?;
        let parsed = ryeos_app::bundle_state_transfer::parse_archive(&existing)?;
        parsed.manifest.verify(ctx.node.verifying_key())?;
        ensure!(
            parsed.manifest.bundle_id == arguments.bundle_id,
            "{} already exists and holds another bundle's state; export to a new path",
            output.display()
        );
        return Ok(json!({
            "bundle_id": parsed.manifest.bundle_id,
            "namespaces": parsed.manifest.namespaces.iter().map(|ns| ns.name.clone()).collect::<Vec<_>>(),
            "files": parsed.files.len(),
            "archive_sha256": lillux::sha256_hex(&existing),
            "output": arguments.output,
            "already_exported": true,
        }));
    }
    let (archive, summary) = ryeos_app::bundle_state_transfer::export_archive(
        ctx.app_root,
        ctx.node,
        &arguments.bundle_id,
        arguments.namespace.as_deref(),
    )?;
    let mut file = directory.open_regular_create(name, true, true, 0o600).with_context(|| {
        format!(
            "create the archive {}; it must not already exist",
            output.display()
        )
    })?;
    std::io::Write::write_all(&mut file, &archive)?;
    file.sync_all()?;
    Ok(json!({
        "bundle_id": summary.bundle_id,
        "namespaces": summary.namespaces,
        "files": summary.files,
        "bytes": summary.bytes,
        "archive_sha256": summary.archive_sha256,
        "output": arguments.output,
    }))
}

pub(super) fn bundle_state_import(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: BundleStateImportArguments = ctx.arguments()?;
    arguments.validate()?;
    let archive = ctx.inputs.require("archive")?;
    let trust_store = ryeos_engine::trust::TrustStore::load_node_registry(ctx.app_root)
        .context("load the node trust registry")?;
    let own_fingerprint = ctx.node.principal_id();
    let own_key = *ctx.node.verifying_key();
    let signer_key = |signer: &str| -> Option<lillux::crypto::VerifyingKey> {
        if signer == own_fingerprint {
            return Some(own_key);
        }
        trust_store
            .get(signer.strip_prefix("fp:").unwrap_or(signer))
            .map(|trusted| trusted.verifying_key)
    };
    let bootstrap = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root: ctx.app_root,
        trust_store: &trust_store,
    };
    let installed = bootstrap
        .load_bundle_section()
        .context("load the node's bundle registrations")?;
    let namespace_backed = |bundle_id: &str, namespace: &str| -> std::result::Result<(), String> {
        let record = installed
            .iter()
            .find(|record| record.name == bundle_id)
            .ok_or_else(|| format!("bundle `{bundle_id}` is not installed on this node"))?;
        let manifest = ryeos_bundle::manifest::load_verified_manifest(
            &record.path.join(ryeos_engine::AI_DIR),
            bundle_id,
            &trust_store,
        )
        .map_err(|error| format!("its manifest does not verify: {error:#}"))?
        .manifest;
        let decl = manifest
            .runtime_authority
            .state_home
            .as_ref()
            .ok_or_else(|| "its signed manifest declares no `runtime_authority.state_home`".to_owned())?;
        if decl.namespaces.backs(namespace) {
            Ok(())
        } else {
            Err(format!(
                "its signed manifest does not declare state namespace `{namespace}`"
            ))
        }
    };
    let summary = ryeos_app::bundle_state_transfer::import_archive(
        ctx.app_root,
        ctx.node,
        archive,
        &ryeos_app::bundle_state_transfer::ImportPolicy {
            signer_key: &signer_key,
            namespace_backed: &namespace_backed,
            replace: &arguments.replace_namespaces,
        },
    )?;
    Ok(json!({
        "bundle_id": summary.bundle_id,
        "created": summary.created,
        "replaced": summary.replaced,
        "unchanged": summary.unchanged,
        "files": summary.files,
    }))
}

/// Receipt first: the receipt lists one digest, the sha256 of the canonical
/// listing of every file the removal will delete, so the ledger names exactly
/// what was discarded before anything is.
pub(super) fn bundle_state_remove_prepare(ctx: &HandlerContext<'_>) -> Result<Prepared> {
    let arguments: BundleStateRemoveArguments = ctx.arguments()?;
    arguments.validate()?;
    let digest = ryeos_app::bundle_state_transfer::listing_digest(
        ctx.app_root,
        ctx.node,
        &arguments.bundle_id,
        arguments.namespace.as_deref(),
    )?;
    Ok(Prepared {
        discarded_grant_digests: vec![digest],
    })
}

pub(super) fn bundle_state_remove_effect(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: BundleStateRemoveArguments = ctx.arguments()?;
    arguments.validate()?;
    let summary = ryeos_app::bundle_state_transfer::remove_state(
        ctx.app_root,
        ctx.node,
        &arguments.bundle_id,
        arguments.namespace.as_deref(),
    )?;
    Ok(json!({
        "bundle_id": summary.bundle_id,
        "namespaces": summary.namespaces,
        "bundle_home_removed": summary.bundle_home_removed,
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
        "operator",
        operator.fingerprint(),
        &lillux::time::iso8601_now(),
        // The operator asked for this reset through the signed offline request
        // whose digest the receipt records; the grant never lapses.
        &ryeos_node_runtime::identity::GrantTerms::operator(
            ryeos_node_runtime::identity::AuthorizedBy::principal_request(
                &ctx.request.signer_fingerprint,
                Some(ctx.request_digest.to_owned()),
            ),
        ),
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
                backend: backend.then(|| "test-backend".to_owned()),
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
    fn recompose_operation_is_idempotent_on_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().to_path_buf();
        let node =
            NodeIdentity::create(&RuntimeRoot::new(state.clone()).node_signing_key_path()).unwrap();
        let lock = StateLock::acquire(&default_lock_path(&state)).unwrap();
        let request = |bindings: serde_json::Value| RequestBody {
            schema: REQUEST_SCHEMA.to_owned(),
            node_fingerprint: node.fingerprint().to_owned(),
            signer_fingerprint: "b".repeat(64),
            signer_grant_digest: None,
            signer_origin_site_id: None,
            operation: "node.external-execution.recompose".to_owned(),
            arguments: json!({ "bindings": bindings }),
            inputs: BTreeMap::new(),
            expected_result: BTreeMap::new(),
            precondition: BTreeMap::new(),
            issued_at: "2026-11-13T00:00:00Z".to_owned(),
            not_after: "2026-11-13T01:00:00Z".to_owned(),
        };
        let inputs = VerifiedInputs::new(BTreeMap::new());
        let run = |request: &RequestBody| {
            recompose(&HandlerContext {
                app_root: &state,
                state_lock: &lock,
                request,
                request_digest: &"c".repeat(64),
                inputs: &inputs,
                host: &BareHost,
                node: &node,
            })
        };
        // A node whose bindings already match its bundles: the effect and its
        // replay both publish nothing and report the same.
        let all = request(json!([]));
        let first = run(&all).expect("first application");
        let replay = run(&all).expect("replay after the crash");
        assert_eq!(first, replay);
        assert_eq!(first["recomposed"], json!([]));
        // A named binding that is not installed is refused, typed, every time.
        let absent = request(json!(["runtime_snapshot_production/render-snapshot"]));
        for _ in 0..2 {
            let error = run(&absent).unwrap_err();
            let refusal = error
                .downcast_ref::<ryeos_app::node_config::recompose::RecomposeRefusal>()
                .unwrap_or_else(|| panic!("typed refusal: {error:#}"));
            assert_eq!(
                refusal.code,
                ryeos_app::node_config::recompose::RECOMPOSE_BINDING_NOT_INSTALLED
            );
        }
        assert!(
            serde_json::from_value::<RecomposeArguments>(json!({"bindings": ["bundles/core"]}))
                .is_err()
        );
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
            policy_carry: Default::default(),
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
