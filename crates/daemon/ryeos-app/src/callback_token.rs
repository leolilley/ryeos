use lillux::time::{Duration, MonotonicDeadline};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::execution_provenance::ExecutionProvenance;

/// Hook identity admitted at the same launch boundary that mints callback
/// authority. Runtime callback input may select one of these identities; it
/// cannot author a new provenance label for durable hook evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDispatchAuthorization {
    pub owner_kind: String,
    pub hook_id: String,
    pub event: String,
    pub layer: ryeos_engine::hooks::HookLayer,
    pub result_mode: ryeos_engine::hooks::HookResultMode,
    pub context_contract: ryeos_engine::hooks::HookContextContract,
    /// Exact source-owned authority for this hook. Callback child dispatches
    /// are bounded by this set rather than the launching root's capabilities.
    pub dispatch_caps: Vec<String>,
}

/// Default TTL for callback tokens when no explicit duration is requested.
const DEFAULT_CALLBACK_TTL_SECS: u64 = 300;

/// Runtime-method ceiling carried by one live callback bearer.
///
/// Ordinary managed runtimes retain the pre-existing complete callback
/// protocol. A hosted workload client receives an exact finite surface. This
/// remains part of the existing callback capability so method authority,
/// project authority, principal authority, expiry, and revocation cannot
/// diverge across separate bearer stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackRuntimeMethodSurface {
    exact: Option<Vec<String>>,
}

impl CallbackRuntimeMethodSurface {
    pub fn complete_runtime_protocol() -> Self {
        Self { exact: None }
    }

    pub fn exact(mut methods: Vec<String>) -> Result<Self> {
        if methods.is_empty() {
            bail!("exact callback runtime-method surface is empty");
        }
        methods.sort();
        methods.dedup();
        for method in &methods {
            if method.len() > 128
                || !method.starts_with("runtime.")
                || method
                    .bytes()
                    .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            {
                bail!("callback runtime method `{method}` is not canonical");
            }
        }
        Ok(Self {
            exact: Some(methods),
        })
    }

    pub fn authorize(&self, method: &str) -> Result<()> {
        let Some(methods) = self.exact.as_ref() else {
            return Ok(());
        };
        if methods
            .binary_search_by(|value| value.as_str().cmp(method))
            .is_ok()
        {
            return Ok(());
        }
        bail!("callback capability does not authorize runtime method `{method}`")
    }

    fn is_exact(&self) -> bool {
        self.exact.is_some()
    }
}

/// Exact target-local authority retained for one hosted-worker boot.
///
/// The project declaration, worker root, ingress principal, and node policy
/// are inputs to admission, never independent live grants. This record is the
/// resulting intersection and is attached to the existing callback
/// capability so runtime-method, child-dispatch, expiry, and revocation
/// authority cannot diverge across parallel stores.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedWorkloadClientGrant {
    pub schema: u32,
    pub protocol: String,
    pub chain_root_id: String,
    pub placement_thread_id: String,
    pub owner_principal: String,
    pub origin_site_id: String,
    pub worker_instance_id: String,
    pub worker_boot_epoch: u64,
    pub worker_boot_identity_hash: String,
    pub root_launch_capsule_hash: String,
    pub session_capsule_hash: String,
    pub project_authority_digest: String,
    pub request_digest: String,
    pub caller_scope_digest: String,
    pub operator_grant_digest: String,
    pub root_delegation_digest: String,
    pub node_policy_generation_digest: String,
    pub ingresses: Vec<ryeos_runtime::workload_client::WorkloadClientIngress>,
    pub executions: Vec<ryeos_runtime::workload_client::WorkloadClientExecutionCeiling>,
    pub execution_presentation: serde_json::Value,
    pub effective_caps: Vec<String>,
    pub max_in_flight: u16,
    pub max_invocations_per_boot: u32,
    pub max_lifetime_seconds: u64,
    pub max_request_bytes: u32,
}

impl AdmittedWorkloadClientGrant {
    pub const SCHEMA: u32 = 2;

    pub fn validate(&self) -> Result<()> {
        ryeos_runtime::workload_client::validate_execution_presentation(
            &self.execution_presentation,
        )?;
        let presented = self
            .execution_presentation
            .as_array()
            .expect("validated presentation array");
        if presented.len() != self.executions.len()
            || presented
                .iter()
                .zip(&self.executions)
                .any(|(item, ceiling)| {
                    item.get("authority")
                        != Some(
                            &ryeos_runtime::workload_client::execution_ceiling_presentation(
                                ceiling,
                            ),
                        )
                })
        {
            bail!("workload presentation contradicts its admitted execution ceiling");
        }
        if self.schema != Self::SCHEMA
            || self.protocol != ryeos_runtime::workload_client::WORKLOAD_CLIENT_PROTOCOL
            || self.chain_root_id.is_empty()
            || self.chain_root_id.len() > 256
            || self.placement_thread_id.is_empty()
            || self.placement_thread_id.len() > 256
            || self
                .owner_principal
                .strip_prefix("fp:")
                .is_none_or(|fingerprint| !lillux::valid_hash(fingerprint))
            || crate::identity::validate_canonical_site_id(&self.origin_site_id).is_err()
            || self.worker_instance_id.is_empty()
            || self.worker_instance_id.len() > 256
            || self.worker_boot_epoch == 0
            || self.max_request_bytes == 0
            || self.max_request_bytes as usize
                > ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_FRAME_BYTES
        {
            bail!("admitted workload-client grant is outside its closed structural bounds");
        }
        for (label, digest) in [
            (
                "worker boot identity",
                self.worker_boot_identity_hash.as_str(),
            ),
            (
                "root launch capsule",
                self.root_launch_capsule_hash.as_str(),
            ),
            ("session capsule", self.session_capsule_hash.as_str()),
            ("project authority", self.project_authority_digest.as_str()),
            ("request", self.request_digest.as_str()),
            ("caller scope", self.caller_scope_digest.as_str()),
            ("operator grant", self.operator_grant_digest.as_str()),
            ("root delegation", self.root_delegation_digest.as_str()),
            (
                "node policy generation",
                self.node_policy_generation_digest.as_str(),
            ),
        ] {
            if !lillux::valid_hash(digest) {
                bail!("admitted workload-client {label} digest is not canonical");
            }
        }
        ryeos_runtime::workload_client::validate_workload_client_ingresses(&self.ingresses)?;
        ryeos_runtime::workload_client::validate_execution_ceilings(&self.executions)?;
        ryeos_runtime::workload_client::validate_workload_client_limits(
            self.max_in_flight,
            self.max_invocations_per_boot,
            self.max_lifetime_seconds,
        )?;
        if self.effective_caps.is_empty()
            || self
                .effective_caps
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self.effective_caps.iter().any(|capability| {
                !capability.starts_with("ryeos.execute.")
                    || ryeos_runtime::authorizer::validate_scope_pattern(capability).is_err()
                    || capability.contains('*')
                    || capability.contains('?')
            })
        {
            bail!("admitted workload-client capabilities are not exact, sorted, and unique");
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        let value = serde_json::to_value(self)?;
        let canonical = lillux::canonical_json(&value)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub fn authorize_action(&self, action: &ryeos_runtime::callback::ActionPayload) -> Result<()> {
        if !action.product_selections.is_empty() {
            bail!("workload-client grant does not admit product selection controls");
        }
        if action.thread != "inline" || action.facets.is_some() || action.launch_window.is_some() {
            bail!("workload-client grant admits only unary inline execution");
        }
        let execution = self
            .executions
            .binary_search_by(|entry| entry.item_ref.as_str().cmp(&action.item_id))
            .ok()
            .and_then(|index| self.executions.get(index))
            .ok_or_else(|| anyhow::anyhow!("workload-client item is outside the admitted grant"))?;
        for (name, item_ref) in &action.ref_bindings {
            let allowed = execution.ref_bindings.get(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "workload-client ref binding `{name}` is outside the admitted grant"
                )
            })?;
            if allowed.binary_search(item_ref).is_err() {
                bail!("workload-client ref binding `{name}` value is outside the admitted grant");
            }
        }
        let admitted_call = match action.call.as_ref().and_then(|call| call.method()) {
            Some(name) => ryeos_runtime::workload_client::WorkloadClientCallCeiling::Method {
                name: name.to_owned(),
            },
            None => ryeos_runtime::workload_client::WorkloadClientCallCeiling::Default,
        };
        if execution.calls.binary_search(&admitted_call).is_err() {
            bail!("workload-client method is outside the admitted grant");
        }
        Ok(())
    }

    pub fn authorize_effect_class(
        &self,
        item_ref: &str,
        effect_class: Option<ryeos_effect_contract::EffectClass>,
    ) -> Result<()> {
        let execution = self
            .executions
            .binary_search_by(|entry| entry.item_ref.as_str().cmp(item_ref))
            .ok()
            .and_then(|index| self.executions.get(index))
            .ok_or_else(|| anyhow::anyhow!("workload-client item is outside the admitted grant"))?;
        let class = effect_class.map_or("live", ryeos_effect_contract::EffectClass::as_str);
        if execution
            .effect_classes
            .binary_search_by(|value| value.as_str().cmp(class))
            .is_err()
        {
            bail!("workload-client child effect class `{class}` exceeds the admitted grant");
        }
        Ok(())
    }

    pub fn authorize_workspace_access(
        &self,
        item_ref: &str,
        access: Option<ryeos_engine::kind_registry::WorkspaceAccess>,
    ) -> Result<ryeos_engine::kind_registry::WorkspaceAccess> {
        let execution = self
            .executions
            .binary_search_by(|entry| entry.item_ref.as_str().cmp(item_ref))
            .ok()
            .and_then(|index| self.executions.get(index))
            .ok_or_else(|| anyhow::anyhow!("workload-client item is outside the admitted grant"))?;
        let access = access.ok_or_else(|| {
            anyhow::anyhow!(
                "workload-client child has no signed shared-workspace access projection"
            )
        })?;
        if access != execution.workspace_access {
            bail!(
                "workload-client child workspace access contradicts the admitted project request"
            );
        }
        Ok(access)
    }
}

#[derive(Debug, Clone)]
pub struct CallbackCapability {
    // This is the process-local bearer projection for every runtime-origin
    // callback, including a boot-bound hosted-workload client. Its exact
    // method surface lives below; future action/project/boot constraints also
    // extend this record rather than creating a parallel workload-token store.
    // Durable identity belongs in the launch/session capsule and worker rows,
    // while this store retains only the live bearer needed by the attached
    // process.
    pub token: String,
    pub invocation_id: String,
    pub thread_id: String,
    /// Exact durable launch owner allowed to use this token. Production
    /// managed launches bind it before the token is exposed to a runtime.
    pub launch_owner: Option<String>,
    /// Exact runtime RPC surface admitted for this bearer. This is checked in
    /// the UDS prelude before routing and again at security-sensitive handlers.
    pub runtime_method_surface: CallbackRuntimeMethodSurface,
    /// Chain root of the minting thread. Carried so the daemon can key
    /// cross-chain wiring from a callback without re-deriving it. It is NOT an
    /// authority source by itself — callers that act on it MUST confirm it
    /// against the authoritative thread row via
    /// [`CallbackCapability::assert_chain_root`].
    pub chain_root_id: String,
    pub project_path: PathBuf,
    pub expires_at: MonotonicDeadline,
    /// V5.5 P2: composed effective capabilities the parent thread
    /// holds. Carried on the callback token so the daemon-side
    /// dispatcher can enforce caps at the trust boundary instead of
    /// trusting the runtime to self-police. Empty = deny-all.
    pub effective_caps: Vec<String>,
    /// Required provenance from the parent dispatch. Callback children
    /// are derived from this value with `clone_for_borrowed_child()`;
    /// there is no deploy-window fallback or daemon-engine fallback.
    pub provenance: ExecutionProvenance,
    /// Bundle identity derived by the launcher from the verified root item.
    /// Runtime bundle-event APIs use this instead of trusting caller-supplied
    /// bundle IDs.
    pub effective_bundle_id: Option<String>,
    /// Root item ref that minted this callback token, used for attribution.
    pub item_ref: Option<String>,
    /// Verified raw-content digest of `item_ref`, captured at launch. Callback
    /// hook identity must match this value; resolving live during a callback
    /// would reintroduce a source-mutation race.
    pub root_raw_content_digest: String,
    /// Exact effective executable identity captured with a managed program.
    /// Non-program and deny-all callback tokens carry no invented identity.
    pub effective_definition_digest: Option<String>,
    /// Exact hook identities captured from the verified definition and
    /// configured hook roots before the runtime starts. Empty is deny-all for
    /// hook dispatch while remaining valid for ordinary callbacks.
    pub hook_dispatch_authorizations: Vec<HookDispatchAuthorization>,
    /// Kind-validator effect grants captured from the finalized program before
    /// the runtime starts. Empty is deny-all. Runtime input may select only an
    /// opaque `authorization_id`; every other identity dimension comes from
    /// this server-side list.
    pub effect_dispatch_authorizations: Vec<ryeos_effect_contract::AdmittedEffectAuthorization>,
    /// Parent thread's resolved hard limits, serialized by the launcher. The
    /// daemon passes this through out-of-band on callback-dispatched child
    /// launches so runtimes cannot spoof parent budget inheritance.
    pub hard_limits: Value,
    /// Parent thread's current spawn-tree depth. Children launch at `depth + 1`.
    pub depth: u32,
    /// Immutable accounting scope of the minting thread. Paid callback-
    /// dispatched descendants inherit this execution budget authority; it is
    /// never accepted from runtime-supplied fields.
    pub accounting_scope: Option<ryeos_state::objects::AdmittedAccountingScope>,
    /// Optional boot-local narrowing for the generic hosted workload client.
    /// Absence preserves the ordinary callback contract. This is bound once
    /// before the protected child channel is exposed and can never be widened.
    pub workload_client_grant: Option<AdmittedWorkloadClientGrant>,
}

impl CallbackCapability {
    /// Confirm this cap's carried `chain_root_id` against the authoritative
    /// chain root from state. The cap value is a convenience carrier, never
    /// trusted on its own — cross-chain wiring keys on the validated result of
    /// this check, not the raw token value.
    pub fn assert_chain_root(&self, authoritative_chain_root_id: &str) -> Result<()> {
        if self.chain_root_id != authoritative_chain_root_id {
            bail!(
                "callback capability chain_root_id mismatch: cap={}, state={}",
                self.chain_root_id,
                authoritative_chain_root_id
            );
        }
        Ok(())
    }
}

pub struct CallbackCapabilityStore {
    // Keep one callback-capability owner. A long-lived worker may receive a
    // freshly minted pair for each attached boot, but its bearer still uses
    // this same validation/revocation path. A second store would split method,
    // project, principal, expiry, and detach authority across competing
    // implementations.
    capabilities: Mutex<HashMap<String, CallbackCapability>>,
}

impl Default for CallbackCapabilityStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CallbackCapabilityStore {
    pub fn new() -> Self {
        Self {
            capabilities: Mutex::new(HashMap::new()),
        }
    }

    pub fn generate(
        &self,
        thread_id: &str,
        project_path: PathBuf,
        ttl: Duration,
        effective_caps: Vec<String>,
        provenance: ExecutionProvenance,
        root_raw_content_digest: String,
    ) -> CallbackCapability {
        self.generate_with_context(
            thread_id,
            project_path,
            ttl,
            effective_caps,
            provenance,
            None,
            None,
            root_raw_content_digest,
            None,
            Value::Null,
            0,
        )
    }

    // One argument per capability field; launch boundaries bind them
    // positionally so every authority-bearing value is explicit at mint time.
    #[allow(clippy::too_many_arguments)]
    pub fn generate_with_context(
        &self,
        thread_id: &str,
        project_path: PathBuf,
        ttl: Duration,
        effective_caps: Vec<String>,
        provenance: ExecutionProvenance,
        effective_bundle_id: Option<String>,
        item_ref: Option<String>,
        root_raw_content_digest: String,
        effective_definition_digest: Option<String>,
        hard_limits: Value,
        depth: u32,
    ) -> CallbackCapability {
        // Lillux owns OS entropy at the same boundary where it owns time and
        // descriptor authority. Higher layers consume opaque random bytes;
        // they must not grow a second direct platform RNG dependency.
        let random_bytes = lillux::crypto::generate_random_bytes::<32>();
        let hex = lillux::cas::sha256_hex(&random_bytes);
        let token = format!("cbt-{hex}");

        let inv_bytes = lillux::crypto::generate_random_bytes::<16>();
        let inv_hex = lillux::cas::sha256_hex(&inv_bytes);
        let invocation_id = format!("inv-{}", &inv_hex[..12]);

        let cap = CallbackCapability {
            token: token.clone(),
            invocation_id,
            thread_id: thread_id.to_string(),
            launch_owner: None,
            runtime_method_surface: CallbackRuntimeMethodSurface::complete_runtime_protocol(),
            // Defaults to root (chain_root == thread_id). The managed launch
            // path overrides this via `set_chain_root` with the thread's
            // authoritative chain root from state.
            chain_root_id: thread_id.to_string(),
            project_path,
            expires_at: MonotonicDeadline::after(ttl),
            effective_caps,
            provenance,
            effective_bundle_id,
            item_ref,
            root_raw_content_digest,
            effective_definition_digest,
            hook_dispatch_authorizations: Vec::new(),
            effect_dispatch_authorizations: Vec::new(),
            hard_limits,
            depth,
            accounting_scope: None,
            workload_client_grant: None,
        };

        self.capabilities.lock().unwrap().insert(token, cap.clone());
        cap
    }

    /// Bind the launch-captured hook identity allow-list before the token is
    /// exposed to a runtime. Returns whether the token was still present.
    pub fn set_hook_dispatch_authorizations(
        &self,
        token: &str,
        mut authorizations: Vec<HookDispatchAuthorization>,
    ) -> bool {
        authorizations.sort_by(|left, right| {
            left.hook_id
                .cmp(&right.hook_id)
                .then_with(|| left.event.cmp(&right.event))
                .then_with(|| left.layer.cmp(&right.layer))
                .then_with(|| left.result_mode.as_str().cmp(right.result_mode.as_str()))
        });
        authorizations.dedup();
        match self.capabilities.lock().unwrap().get_mut(token) {
            Some(cap) => {
                cap.hook_dispatch_authorizations = authorizations;
                true
            }
            None => false,
        }
    }

    pub fn set_effect_dispatch_authorizations(
        &self,
        token: &str,
        authorizations: Vec<ryeos_effect_contract::AdmittedEffectAuthorization>,
    ) -> Result<bool> {
        let mut prior: Option<&str> = None;
        for authorization in &authorizations {
            authorization.validate()?;
            if prior.is_some_and(|value| value >= authorization.authorization_id.as_str()) {
                bail!("effect dispatch authorizations must be sorted and unique by id");
            }
            prior = Some(&authorization.authorization_id);
        }
        Ok(match self.capabilities.lock().unwrap().get_mut(token) {
            Some(cap) => {
                cap.effect_dispatch_authorizations = authorizations;
                true
            }
            None => false,
        })
    }

    /// Bind the minting thread's accounting scope to a freshly-minted cap.
    /// Returns whether the token was found.
    pub fn set_accounting_scope(
        &self,
        token: &str,
        scope: ryeos_state::objects::AdmittedAccountingScope,
    ) -> bool {
        match self.capabilities.lock().unwrap().get_mut(token) {
            Some(cap) => {
                cap.accounting_scope = Some(scope);
                true
            }
            None => false,
        }
    }

    /// Override the carried chain root for a freshly-minted cap. Returns whether
    /// the token was found. Root mints default `chain_root == thread_id`; the
    /// managed launch path sets the thread's authoritative chain root (from
    /// state) here so the cap reflects real chain lineage.
    pub fn set_chain_root(&self, token: &str, chain_root_id: &str) -> bool {
        match self.capabilities.lock().unwrap().get_mut(token) {
            Some(cap) => {
                cap.chain_root_id = chain_root_id.to_string();
                true
            }
            None => false,
        }
    }

    pub fn set_launch_owner(&self, token: &str, launch_owner: String) -> bool {
        match self.capabilities.lock().unwrap().get_mut(token) {
            Some(cap) => {
                cap.launch_owner = Some(launch_owner);
                true
            }
            None => false,
        }
    }

    /// Narrow a freshly minted callback bearer to one exact runtime-method
    /// surface before it is exposed to a process. This operation never widens
    /// an already-exact surface.
    pub fn restrict_runtime_methods(
        &self,
        token: &str,
        surface: CallbackRuntimeMethodSurface,
    ) -> Result<bool> {
        if !surface.is_exact() {
            bail!("callback runtime-method restriction must be exact");
        }
        Ok(match self.capabilities.lock().unwrap().get_mut(token) {
            Some(cap) => {
                if cap.runtime_method_surface.is_exact() {
                    bail!("callback runtime-method surface was already restricted");
                }
                cap.runtime_method_surface = surface;
                true
            }
            None => false,
        })
    }

    /// Bind one already-admitted workload-client intersection to a freshly
    /// minted callback capability. Rebinding is forbidden even to an equal
    /// value so no live bearer can change authority after exposure.
    pub fn set_workload_client_grant(
        &self,
        token: &str,
        grant: AdmittedWorkloadClientGrant,
    ) -> Result<bool> {
        grant.validate()?;
        Ok(match self.capabilities.lock().unwrap().get_mut(token) {
            Some(cap) => {
                if cap.workload_client_grant.is_some() {
                    bail!("callback workload-client grant was already bound");
                }
                cap.workload_client_grant = Some(grant);
                true
            }
            None => false,
        })
    }

    pub fn validate(
        &self,
        token: &str,
        thread_id: &str,
        project_path: &std::path::Path,
    ) -> Result<CallbackCapability> {
        let map = self.capabilities.lock().unwrap();
        let cap = map
            .get(token)
            .ok_or_else(|| anyhow::anyhow!("invalid callback capability"))?;

        if cap.expires_at.has_elapsed() {
            bail!("callback capability expired");
        }

        if cap.thread_id != thread_id {
            bail!("callback capability does not match thread_id");
        }

        if cap.project_path != project_path {
            bail!("callback capability does not match project_path");
        }

        Ok(cap.clone())
    }

    /// Validate a callback token without binding it to a thread. Returns the
    /// capability if the token exists and has not expired. The caller is
    /// responsible for any access-scope check (e.g. chain membership for a
    /// read). Never use this for a write or lifecycle method — those require an
    /// exact-thread match via [`Self::validate_token_and_thread`].
    pub fn validate_token_only(&self, token: &str) -> Result<CallbackCapability> {
        let map = self.capabilities.lock().unwrap();
        let cap = map
            .get(token)
            .ok_or_else(|| anyhow::anyhow!("invalid callback capability"))?;

        if cap.expires_at.has_elapsed() {
            bail!("callback capability expired");
        }

        Ok(cap.clone())
    }

    pub fn invalidate(&self, token: &str) {
        self.capabilities.lock().unwrap().remove(token);
    }

    /// Validate callback token + thread_id without requiring project_path.
    /// Used by runtime.* UDS methods that don't carry project_path in params.
    pub fn validate_token_and_thread(
        &self,
        token: &str,
        thread_id: &str,
    ) -> Result<CallbackCapability> {
        let map = self.capabilities.lock().unwrap();
        let cap = map
            .get(token)
            .ok_or_else(|| anyhow::anyhow!("invalid callback capability"))?;

        if cap.expires_at.has_elapsed() {
            bail!("callback capability expired");
        }

        if cap.thread_id != thread_id {
            bail!("callback capability does not match thread_id");
        }

        Ok(cap.clone())
    }

    pub fn invalidate_for_thread(&self, thread_id: &str) {
        let mut map = self.capabilities.lock().unwrap();
        map.retain(|_, cap| cap.thread_id != thread_id);
    }

    pub fn prune_expired(&self) -> usize {
        let mut map = self.capabilities.lock().unwrap();
        let before = map.len();
        map.retain(|_, cap| !cap.expires_at.has_elapsed());
        before - map.len()
    }
}

/// Margin added to a run's hard timeout so the run-scoped token outlives the
/// finalization callback that fires at/just after the deadline.
const LAUNCH_TTL_MARGIN_SECS: u64 = 300;

/// Absolute backstop for a run-scoped token — far above any realistic run — so a
/// zombie run cannot hold its credential indefinitely, without re-introducing
/// the sub-run cap that caused tokens to expire mid-run.
const MAX_LAUNCH_TTL_SECS: u64 = 7 * 24 * 3600;

/// TTL for a **run-scoped** launch token (the callback + thread-auth tokens a
/// launched runtime holds for its whole life).
///
/// The token must outlive the run's hard timeout (`duration_seconds`) plus the
/// finalization window, or a run allowed
/// to exceed 3600s loses callback/auth authority before it can finalize — a
/// silent mid-run failure. The token is thread-scoped and invalidated at run
/// end, so a TTL that tracks the run's duration is the correct lifetime; a
/// generous absolute backstop bounds the pathological zombie case.
///
/// A `duration_seconds` value of 0 is the launch hard-limit sentinel for
/// "unlimited". The token still needs an explicit authority lifetime, so it gets
/// the absolute launch-token backstop rather than the short default TTL.
///
/// CAVEAT: a run whose effective finite `duration_seconds` exceeds
/// `MAX_LAUNCH_TTL_SECS` (7 days), or an unlimited run that actually lives
/// that long, can outlive callback authority. Longer runs need renewal rather
/// than a silent larger constant here.
pub fn launch_token_ttl(duration_seconds: Option<u64>) -> Duration {
    let Some(secs) = duration_seconds else {
        return Duration::from_secs(DEFAULT_CALLBACK_TTL_SECS + LAUNCH_TTL_MARGIN_SECS);
    };
    if secs == 0 {
        return Duration::from_secs(MAX_LAUNCH_TTL_SECS);
    }
    Duration::from_secs(
        secs.saturating_add(LAUNCH_TTL_MARGIN_SECS)
            .min(MAX_LAUNCH_TTL_SECS),
    )
}

pub fn effective_bundle_id_from_item_ref(item_ref: &str) -> Option<String> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref).ok()?;
    canonical
        .bare_id
        .split('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
}

/// Single source of truth for an execution request's effective bundle id.
///
/// Derived from the **resolved** canonical ref (the post-resolution identity of
/// the item that will actually run), not the requested `item_ref` which may be
/// an alias or non-canonical form. The runtime-cap minter, the manifest
/// namespace check, and the callback token's `effective_bundle_id` MUST all use
/// this one value so the minted caps and the token that carries them claim the
/// same bundle identity.
pub fn effective_bundle_id_for_request(
    resolved: &crate::thread_lifecycle::ResolvedExecutionRequest,
) -> Option<String> {
    effective_bundle_id_from_item_ref(&resolved.resolved_item.canonical_ref.to_string())
}

#[derive(Debug, Clone)]
pub struct ThreadAuthState {
    pub token: String,
    pub thread_id: String,
    pub acting_principal: String,
    pub caller_scopes: Vec<String>,
    /// Exact ingress-authenticated handler authority retained for callbacks.
    /// `None` is intentional for node-internal executions that did not enter
    /// through an authenticated handler boundary; those executions must never
    /// synthesize transport verification during a callback.
    handler_context: Option<crate::handler_context::HandlerContext>,
    pub expires_at: MonotonicDeadline,
}

impl ThreadAuthState {
    pub fn handler_context(&self) -> Option<&crate::handler_context::HandlerContext> {
        self.handler_context.as_ref()
    }

    pub fn narrowed_handler_context(
        &self,
        scopes: Vec<String>,
        current_site_id: &str,
        origin_site_id: &str,
    ) -> Result<Option<crate::handler_context::HandlerContext>> {
        self.handler_context
            .as_ref()
            .map(|context| context.narrowed_for_execution(scopes, current_site_id, origin_site_id))
            .transpose()
    }
}

pub struct ThreadAuthStore {
    // This is the paired ingress-principal proof for runtime callbacks. Hosted
    // workload clients reuse it and narrow the retained handler authority;
    // they must not invent a workload-specific principal or signing key.
    states: Mutex<HashMap<String, ThreadAuthState>>,
}

impl Default for ThreadAuthStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadAuthStore {
    pub fn new() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
        }
    }

    pub fn mint(
        &self,
        thread_id: &str,
        acting_principal: String,
        caller_scopes: Vec<String>,
        handler_context: Option<crate::handler_context::HandlerContext>,
        current_site_id: &str,
        origin_site_id: &str,
        ttl: Duration,
    ) -> Result<ThreadAuthState> {
        if let Some(context) = handler_context.as_ref() {
            context.validate_execution_authority(
                &acting_principal,
                &caller_scopes,
                current_site_id,
                origin_site_id,
            )?;
        }
        let random_bytes = lillux::crypto::generate_random_bytes::<32>();
        let hex = lillux::cas::sha256_hex(&random_bytes);
        let token = format!("tat-{hex}");

        let state = ThreadAuthState {
            token: token.clone(),
            thread_id: thread_id.to_string(),
            acting_principal,
            caller_scopes,
            handler_context,
            expires_at: MonotonicDeadline::after(ttl),
        };

        self.states.lock().unwrap().insert(token, state.clone());
        Ok(state)
    }

    pub fn validate(&self, token: &str, thread_id: &str) -> Result<ThreadAuthState> {
        let map = self.states.lock().unwrap();
        let state = map
            .get(token)
            .ok_or_else(|| anyhow::anyhow!("invalid thread auth token"))?;

        if state.expires_at.has_elapsed() {
            bail!("thread auth token expired");
        }

        if state.thread_id != thread_id {
            bail!("thread auth token does not match thread_id");
        }

        Ok(state.clone())
    }

    pub fn invalidate(&self, token: &str) {
        self.states.lock().unwrap().remove(token);
    }

    pub fn invalidate_for_thread(&self, thread_id: &str) {
        let mut map = self.states.lock().unwrap();
        map.retain(|_, s| s.thread_id != thread_id);
    }

    pub fn prune_expired(&self) -> usize {
        let mut map = self.states.lock().unwrap();
        let before = map.len();
        map.retain(|_, state| !state.expires_at.has_elapsed());
        before - map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    use crate::execution_provenance::{ExecutionProvenance, ProjectSourceKind};
    use crate::temp_dir_guard::TempDirGuard;
    use ryeos_engine::engine::Engine;

    type TestProvenance = ExecutionProvenance;

    fn minimal_engine() -> Arc<Engine> {
        Arc::new(Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
            vec![],
        ))
    }

    fn provenance(path: PathBuf) -> TestProvenance {
        let authority =
            crate::execution_policy::synthetic_test_live_project_authority(path.as_path());
        ExecutionProvenance::root_live_fs(path, minimal_engine(), authority).unwrap()
    }

    #[test]
    fn generate_and_validate_round_trip() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-test123",
            PathBuf::from("/project"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/project")),
            "0".repeat(64),
        );
        assert!(cap.token.starts_with("cbt-"));
        assert!(cap.invocation_id.starts_with("inv-"));
        assert_eq!(cap.invocation_id.len(), 16);

        let validated = store
            .validate(&cap.token, "T-test123", PathBuf::from("/project").as_path())
            .unwrap();
        assert_eq!(validated.thread_id, "T-test123");
    }

    #[test]
    fn exact_runtime_method_surface_narrows_existing_callback_bearer() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-workload",
            PathBuf::from("/project"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/project")),
            "0".repeat(64),
        );
        let surface = CallbackRuntimeMethodSurface::exact(vec![
            ryeos_runtime::RUNTIME_DISPATCH_ACTION_METHOD.to_owned(),
        ])
        .unwrap();
        assert!(store.restrict_runtime_methods(&cap.token, surface).unwrap());

        let narrowed = store
            .validate_token_and_thread(&cap.token, "T-workload")
            .unwrap();
        narrowed
            .runtime_method_surface
            .authorize(ryeos_runtime::RUNTIME_DISPATCH_ACTION_METHOD)
            .unwrap();
        assert!(
            narrowed
                .runtime_method_surface
                .authorize("runtime.vault_get")
                .is_err()
        );
        assert!(
            store
                .restrict_runtime_methods(
                    &cap.token,
                    CallbackRuntimeMethodSurface::exact(vec!["runtime.vault_get".to_owned()])
                        .unwrap(),
                )
                .is_err(),
            "an exact bearer must never be widened or replaced in place"
        );
    }

    #[test]
    fn chain_root_defaults_to_thread_id_then_set_chain_root_overrides() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate_with_context(
            "T-succ",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            None,
            None,
            "0".repeat(64),
            Some("0".repeat(64)),
            serde_json::Value::Null,
            0,
        );
        // Defaults to root (chain_root == thread_id).
        assert_eq!(cap.chain_root_id, "T-succ");
        // The managed launch path overrides with the authoritative chain root.
        store.set_chain_root(&cap.token, "T-root");
        let v = store
            .validate(&cap.token, "T-succ", PathBuf::from("/p").as_path())
            .unwrap();
        assert_eq!(v.chain_root_id, "T-root");
    }

    #[test]
    fn generate_uses_thread_id_as_chain_root() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-root",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        assert_eq!(cap.chain_root_id, "T-root");
    }

    #[test]
    fn assert_chain_root_rejects_mismatch() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-succ",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        store.set_chain_root(&cap.token, "T-root");
        let cap = store
            .validate(&cap.token, "T-succ", PathBuf::from("/p").as_path())
            .unwrap();
        assert!(cap.assert_chain_root("T-root").is_ok());
        assert!(cap.assert_chain_root("T-other").is_err());
    }

    #[test]
    fn generate_with_context_round_trips_parent_limits_and_depth() {
        let store = CallbackCapabilityStore::new();
        let hard_limits = serde_json::json!({
            "turns": 6,
            "tokens": 1000,
            "spend_usd": "0.25",
            "spawns": 2,
            "depth": 3,
            "duration_seconds": 45,
        });
        let cap = store.generate_with_context(
            "T-parent",
            PathBuf::from("/project"),
            Duration::from_secs(300),
            vec!["ryeos.*".to_string()],
            provenance(PathBuf::from("/project")),
            Some("bundle-123".to_string()),
            Some("directive:team/parent".to_string()),
            "1".repeat(64),
            Some("1".repeat(64)),
            hard_limits.clone(),
            4,
        );

        let validated = store
            .validate(&cap.token, "T-parent", PathBuf::from("/project").as_path())
            .unwrap();
        assert_eq!(validated.thread_id, "T-parent");
        assert_eq!(validated.hard_limits, hard_limits);
        assert_eq!(validated.depth, 4);
        assert_eq!(validated.effective_bundle_id.as_deref(), Some("bundle-123"));
        assert_eq!(validated.item_ref.as_deref(), Some("directive:team/parent"));
        assert_eq!(validated.root_raw_content_digest, "1".repeat(64));
        assert_eq!(validated.effective_definition_digest, Some("1".repeat(64)));
    }

    #[test]
    fn validate_rejects_unknown_token() {
        let store = CallbackCapabilityStore::new();
        let err = store
            .validate("cbt-nonexistent", "T-x", PathBuf::from("/p").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("invalid callback capability"));
    }

    #[test]
    fn invalidate_removes_capability() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-test",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        store.invalidate(&cap.token);
        assert!(
            store
                .validate(&cap.token, "T-test", PathBuf::from("/p").as_path())
                .is_err()
        );
    }

    #[test]
    fn invalidate_for_thread_removes_matching() {
        let store = CallbackCapabilityStore::new();
        let cap1 = store.generate(
            "T-1",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        let cap2 = store.generate(
            "T-2",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        store.invalidate_for_thread("T-1");
        assert!(
            store
                .validate(&cap1.token, "T-1", PathBuf::from("/p").as_path())
                .is_err()
        );
        assert!(
            store
                .validate(&cap2.token, "T-2", PathBuf::from("/p").as_path())
                .is_ok()
        );
    }

    #[test]
    fn expired_capability_is_rejected() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-test",
            PathBuf::from("/p"),
            Duration::from_secs(0),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        let err = store
            .validate(&cap.token, "T-test", PathBuf::from("/p").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("expired"));
    }

    #[test]
    fn wrong_thread_id_is_rejected() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-1",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        let err = store
            .validate(&cap.token, "T-2", PathBuf::from("/p").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("thread_id"));
    }

    #[test]
    fn wrong_project_path_is_rejected() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-1",
            PathBuf::from("/project-a"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/project-a")),
            "0".repeat(64),
        );
        let err = store
            .validate(&cap.token, "T-1", PathBuf::from("/project-b").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("project_path"));
    }

    #[test]
    fn prune_expired_removes_stale() {
        let store = CallbackCapabilityStore::new();
        store.generate(
            "T-1",
            PathBuf::from("/p"),
            Duration::from_secs(0),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.generate(
            "T-2",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        let pruned = store.prune_expired();
        assert_eq!(pruned, 1);
    }

    #[test]
    fn provenance_lifeline_arc_identity_preserved_across_generate_validate() {
        let store = CallbackCapabilityStore::new();
        let engine = minimal_engine();
        let tmp = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(tmp.path().to_path_buf()));
        let snapshot_hash = "a".repeat(64);
        let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "site:test:/original".to_string(),
            Some(PathBuf::from("/original")),
            snapshot_hash.clone(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::RetainResult,
            },
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        let provenance = ExecutionProvenance::root_pushed_head_for_test(
            tmp.path().to_path_buf(),
            PathBuf::from("/original"),
            engine.clone(),
            lifeline.clone(),
            snapshot_hash,
            project_authority,
        )
        .unwrap();

        let cap = store.generate(
            "T-test",
            tmp.path().to_path_buf(),
            Duration::from_secs(300),
            vec!["ryeos.*".to_string()],
            provenance,
            "0".repeat(64),
        );
        let validated = store.validate(&cap.token, "T-test", tmp.path()).unwrap();

        assert!(Arc::ptr_eq(validated.provenance.request_engine(), &engine));
        assert_eq!(
            validated.provenance.original_project_path(),
            Path::new("/original")
        );
        assert_eq!(
            validated.provenance.project_source(),
            ProjectSourceKind::PushedHead
        );
        assert_eq!(validated.provenance.effective_path(), tmp.path());
        match &validated.provenance {
            ExecutionProvenance::RootPinnedGeneration {
                workspace_lifeline, ..
            } => assert!(Arc::ptr_eq(workspace_lifeline, &lifeline)),
            other => panic!("expected RootPinnedGeneration, got {other:?}"),
        }
    }

    #[test]
    fn provenance_engine_arc_identity_preserved_across_clone() {
        let engine = minimal_engine();
        let cap = CallbackCapability {
            token: "cbt-test".to_string(),
            invocation_id: "inv-test".to_string(),
            thread_id: "T-test".to_string(),
            launch_owner: None,
            runtime_method_surface: CallbackRuntimeMethodSurface::complete_runtime_protocol(),
            chain_root_id: "T-test".to_string(),
            project_path: PathBuf::from("/project"),
            expires_at: MonotonicDeadline::after(Duration::from_secs(300)),
            effective_caps: vec![],
            provenance: ExecutionProvenance::root_live_fs(
                PathBuf::from("/project"),
                engine.clone(),
                crate::execution_policy::synthetic_test_live_project_authority(Path::new(
                    "/project",
                )),
            )
            .unwrap(),
            effective_bundle_id: None,
            item_ref: None,
            root_raw_content_digest: "0".repeat(64),
            effective_definition_digest: None,
            hook_dispatch_authorizations: Vec::new(),
            effect_dispatch_authorizations: Vec::new(),
            hard_limits: serde_json::Value::Null,
            depth: 0,
            accounting_scope: None,
            workload_client_grant: None,
        };

        let cloned = cap.clone();
        assert!(Arc::ptr_eq(cloned.provenance.request_engine(), &engine));
    }

    #[test]
    fn provenance_required_round_trips_through_validate() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-test",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            "0".repeat(64),
        );
        let validated = store
            .validate(&cap.token, "T-test", PathBuf::from("/p").as_path())
            .unwrap();
        assert_eq!(validated.provenance.effective_path(), Path::new("/p"));
    }

    #[test]
    fn launch_token_ttl_outlives_a_run_past_3600() {
        // The bug: a run allowed to exceed 3600s must NOT lose its callback
        // token before it finalizes. The run-scoped TTL covers the run duration
        // plus a finalization margin, not the sub-run 3600 cap.
        let run = 7200u64;
        let ttl = launch_token_ttl(Some(run));
        assert!(
            ttl >= Duration::from_secs(run),
            "run-scoped token must outlive the run's hard timeout: {ttl:?}"
        );
        assert_eq!(ttl, Duration::from_secs(run + LAUNCH_TTL_MARGIN_SECS));
    }

    #[test]
    fn launch_token_ttl_has_absolute_backstop() {
        // A pathological duration is bounded, but the backstop is far above any
        // realistic run (so it never clips a real run the way 3600 did).
        assert_eq!(
            launch_token_ttl(Some(u64::MAX)),
            Duration::from_secs(MAX_LAUNCH_TTL_SECS)
        );
        const _: () = assert!(MAX_LAUNCH_TTL_SECS > 3600);
    }

    #[test]
    fn launch_token_ttl_zero_duration_uses_backstop() {
        // Launch hard-limits use 0 as the unlimited sentinel. The run token must
        // not collapse unlimited runtime authority to only the finalization
        // margin.
        assert_eq!(
            launch_token_ttl(Some(0)),
            Duration::from_secs(MAX_LAUNCH_TTL_SECS)
        );
    }

    #[test]
    fn launch_token_ttl_defaults_when_unset() {
        assert_eq!(
            launch_token_ttl(None),
            Duration::from_secs(DEFAULT_CALLBACK_TTL_SECS + LAUNCH_TTL_MARGIN_SECS)
        );
    }

    // ── ThreadAuthStore ──────────────────────────────────────────────

    fn mint_test(
        store: &ThreadAuthStore,
        thread_id: &str,
        principal: &str,
        scopes: Vec<String>,
        ttl: Duration,
    ) -> ThreadAuthState {
        store
            .mint(
                thread_id,
                principal.to_string(),
                scopes,
                None,
                "site:test",
                "site:test",
                ttl,
            )
            .unwrap()
    }

    #[test]
    fn thread_auth_mint_and_validate_round_trip() {
        let store = ThreadAuthStore::new();
        let state = mint_test(
            &store,
            "T-abc",
            "fp:user123",
            vec!["execute".to_string()],
            Duration::from_secs(300),
        );
        assert!(state.token.starts_with("tat-"));
        assert_eq!(state.acting_principal, "fp:user123");

        let validated = store.validate(&state.token, "T-abc").unwrap();
        assert_eq!(validated.thread_id, "T-abc");
        assert_eq!(validated.acting_principal, "fp:user123");
        assert_eq!(validated.caller_scopes, vec!["execute"]);
    }

    #[test]
    fn thread_auth_rejects_unknown_token() {
        let store = ThreadAuthStore::new();
        let err = store.validate("tat-nonexistent", "T-x").unwrap_err();
        assert!(err.to_string().contains("invalid thread auth token"));
    }

    #[test]
    fn thread_auth_rejects_wrong_thread() {
        let store = ThreadAuthStore::new();
        let state = mint_test(&store, "T-1", "fp:u", vec![], Duration::from_secs(300));
        let err = store.validate(&state.token, "T-2").unwrap_err();
        assert!(err.to_string().contains("thread_id"));
    }

    #[test]
    fn thread_auth_rejects_expired() {
        let store = ThreadAuthStore::new();
        let state = mint_test(&store, "T-1", "fp:u", vec![], Duration::from_secs(0));
        std::thread::sleep(std::time::Duration::from_millis(10));
        let err = store.validate(&state.token, "T-1").unwrap_err();
        assert!(err.to_string().contains("expired"));
    }

    #[test]
    fn thread_auth_invalidate_removes_token() {
        let store = ThreadAuthStore::new();
        let state = mint_test(&store, "T-1", "fp:u", vec![], Duration::from_secs(300));
        store.invalidate(&state.token);
        assert!(store.validate(&state.token, "T-1").is_err());
    }

    #[test]
    fn thread_auth_invalidate_for_thread() {
        let store = ThreadAuthStore::new();
        let s1 = mint_test(&store, "T-1", "fp:u", vec![], Duration::from_secs(300));
        let s2 = mint_test(&store, "T-2", "fp:u", vec![], Duration::from_secs(300));
        store.invalidate_for_thread("T-1");
        assert!(store.validate(&s1.token, "T-1").is_err());
        assert!(store.validate(&s2.token, "T-2").is_ok());
    }

    #[test]
    fn thread_auth_prune_expired() {
        let store = ThreadAuthStore::new();
        mint_test(&store, "T-1", "fp:u", vec![], Duration::from_secs(0));
        std::thread::sleep(std::time::Duration::from_millis(10));
        mint_test(&store, "T-2", "fp:u", vec![], Duration::from_secs(300));
        let pruned = store.prune_expired();
        assert_eq!(pruned, 1);
    }

    #[test]
    fn thread_auth_preserves_and_narrows_remote_operator_authority() {
        let store = ThreadAuthStore::new();
        let context = crate::handler_context::HandlerContext::new_with_authority(
            "fp:operator".to_string(),
            vec!["cap:a".to_string(), "cap:b".to_string()],
            true,
            Some(crate::identity::AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:source".to_string()),
        );
        let state = store
            .mint(
                "T-remote",
                "fp:operator".to_string(),
                vec!["cap:a".to_string(), "cap:b".to_string()],
                Some(context),
                "site:target",
                "site:source",
                Duration::from_secs(300),
            )
            .unwrap();

        let narrowed = state
            .narrowed_handler_context(vec!["cap:a".to_string()], "site:target", "site:source")
            .unwrap()
            .unwrap();
        assert_eq!(narrowed.scopes, vec!["cap:a"]);
        assert_eq!(
            narrowed.authorized_key_class,
            Some(crate::identity::AuthorizedKeyPrincipalClass::RemoteOperator)
        );
        assert_eq!(
            narrowed.authenticated_origin_site_id.as_deref(),
            Some("site:source")
        );
    }

    fn workload_client_grant() -> AdmittedWorkloadClientGrant {
        let mut grant = AdmittedWorkloadClientGrant {
            schema: AdmittedWorkloadClientGrant::SCHEMA,
            protocol: ryeos_runtime::workload_client::WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            chain_root_id: "T-root".to_owned(),
            placement_thread_id: "T-placement".to_owned(),
            owner_principal: format!("fp:{}", "a".repeat(64)),
            origin_site_id: "site:source".to_owned(),
            worker_instance_id: "worker-1".to_owned(),
            worker_boot_epoch: 1,
            worker_boot_identity_hash: "b".repeat(64),
            root_launch_capsule_hash: "c".repeat(64),
            session_capsule_hash: "d".repeat(64),
            project_authority_digest: "e".repeat(64),
            request_digest: "f".repeat(64),
            caller_scope_digest: "1".repeat(64),
            operator_grant_digest: "2".repeat(64),
            root_delegation_digest: "3".repeat(64),
            node_policy_generation_digest: "4".repeat(64),
            ingresses: vec![ryeos_runtime::workload_client::WorkloadClientIngress::Cli],
            execution_presentation: serde_json::Value::Null,
            executions: vec![
                ryeos_runtime::workload_client::WorkloadClientExecutionCeiling {
                    item_ref: "tool:project/check".to_owned(),
                    ref_bindings: std::collections::BTreeMap::from([(
                        "input".to_owned(),
                        vec!["knowledge:project/source".to_owned()],
                    )]),
                    calls: vec![
                        ryeos_runtime::workload_client::WorkloadClientCallCeiling::Default,
                        ryeos_runtime::workload_client::WorkloadClientCallCeiling::Method {
                            name: "inspect".to_owned(),
                        },
                    ],
                    effect_classes: vec!["live".to_owned(), "recorded".to_owned()],
                    workspace_access:
                        ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
                },
            ],
            effective_caps: vec!["ryeos.execute.tool.project/check".to_owned()],
            max_in_flight: 2,
            max_invocations_per_boot: 8,
            max_lifetime_seconds: 300,
            max_request_bytes: 4096,
        };
        grant.execution_presentation = serde_json::Value::Array(
            grant
                .executions
                .iter()
                .map(|ceiling| {
                    serde_json::json!({
                        "authority": ryeos_runtime::workload_client::execution_ceiling_presentation(ceiling)
                    })
                })
                .collect(),
        );
        grant
    }

    fn workload_client_action() -> ryeos_runtime::callback::ActionPayload {
        ryeos_runtime::callback::ActionPayload {
            product_selections: Vec::new(),
            operation_id: Some("5".repeat(64)),
            item_id: "tool:project/check".to_owned(),
            ref_bindings: std::collections::BTreeMap::from([(
                "input".to_owned(),
                "knowledge:project/source".to_owned(),
            )]),
            params: serde_json::json!({"focused": true}),
            thread: "inline".to_owned(),
            call: None,
            facets: None,
            launch_window: None,
        }
    }

    #[test]
    fn workload_client_grant_admits_only_exact_action_surface() {
        let grant = workload_client_grant();
        grant.validate().unwrap();
        grant.authorize_action(&workload_client_action()).unwrap();

        let mut wrong_ref = workload_client_action();
        wrong_ref
            .ref_bindings
            .insert("input".to_owned(), "knowledge:project/other".to_owned());
        assert!(grant.authorize_action(&wrong_ref).is_err());

        let mut wrong_method = workload_client_action();
        wrong_method.call = Some(ryeos_runtime::callback::MethodCall {
            method: Some("write".to_owned()),
            args: None,
        });
        assert!(grant.authorize_action(&wrong_method).is_err());

        let mut detached = workload_client_action();
        detached.thread = "detached".to_owned();
        assert!(grant.authorize_action(&detached).is_err());
    }

    #[test]
    fn workload_client_grant_rechecks_resolved_child_effect_class() {
        let grant = workload_client_grant();
        grant
            .authorize_effect_class("tool:project/check", None)
            .unwrap();
        grant
            .authorize_effect_class(
                "tool:project/check",
                Some(ryeos_effect_contract::EffectClass::Recorded),
            )
            .unwrap();
        assert!(
            grant
                .authorize_effect_class(
                    "tool:project/check",
                    Some(ryeos_effect_contract::EffectClass::Sealed),
                )
                .is_err()
        );
    }

    #[test]
    fn workload_client_grant_requires_the_exact_child_workspace_projection() {
        let grant = workload_client_grant();
        assert_eq!(
            grant
                .authorize_workspace_access(
                    "tool:project/check",
                    Some(ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,),
                )
                .unwrap(),
            ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration
        );
        assert!(
            grant
                .authorize_workspace_access(
                    "tool:project/check",
                    Some(ryeos_engine::kind_registry::WorkspaceAccess::SharedExclusive),
                )
                .is_err()
        );
        assert!(
            grant
                .authorize_workspace_access("tool:project/check", None)
                .is_err()
        );
    }
}
