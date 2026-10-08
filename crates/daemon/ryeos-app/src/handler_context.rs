//! Typed handler context — the single entry point for caller identity.
//!
//! Passed as a typed parameter to every service handler. Not injected
//! into the request JSON — handlers receive it as a second argument
//! alongside `(Value, HandlerContext, Arc<AppState>)`.
//!
//! ## Fail-closed semantics
//!
//! `verified` is `true` **only** when the caller identity was
//! cryptographically verified (Ed25519 signature on the request).
//! Unauthenticated routes produce `verified: false` with a synthetic
//! fingerprint — handlers must check `verified` before trusting the
//! identity for ownership or authorization decisions.
//!
//! ## Ownership semantics
//!
//! - `require_owner()` asserts `verified == true` — unverified contexts
//!   always get `HandlerError::NotFound` (never 403).
//! - Owner check: fingerprint matches the resource owner.
//! - Not-found-never-forbidden: `require_owner()` returns
//!   `HandlerError::NotFound` (not 403) to avoid leaking resource
//!   existence to non-owners.

use crate::handler_error::HandlerError;
use std::sync::Arc;

/// Process-local clock authority derived only from the actual callback stores,
/// retained parent capsule and original execution ledger. Cloning preserves
/// the original owner; a later callback may only add its own earlier expiry.
#[derive(Clone)]
pub struct OriginalExecutionBudget {
    owner: Arc<OriginalExecutionBudgetOwner>,
    // Exact allocation payer scope verified for THIS callback. Sharing the
    // original execution clock never drops its narrower directive account.
    allocation_scope: ryeos_state::objects::AdmittedAccountingScope,
    callback_invocation_id: String,
    callback_thread_id: String,
    callback_launch_owner: String,
    callback_expires_at: lillux::time::MonotonicDeadline,
}

struct OriginalExecutionBudgetOwner {
    scope: ryeos_state::objects::AdmittedAccountingScope,
    root_chain_id: String,
    limits_digest: String,
    deadline_at_ms: i64,
    principal: String,
    current_site_id: String,
    origin_site_id: String,
    expires_at: lillux::time::MonotonicDeadline,
}

impl std::fmt::Debug for OriginalExecutionBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OriginalExecutionBudget { .. }")
    }
}

impl PartialEq for OriginalExecutionBudget {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.callback_invocation_id == other.callback_invocation_id
            && self.allocation_scope == other.allocation_scope
            && self.callback_thread_id == other.callback_thread_id
            && self.callback_launch_owner == other.callback_launch_owner
    }
}

impl Eq for OriginalExecutionBudget {}

impl OriginalExecutionBudget {
    /// A copy of the original clock, narrowed by this verified callback.
    /// This conveys no ability to construct or replace handler authority.
    pub fn contact_expiry(&self) -> lillux::time::MonotonicDeadline {
        self.owner.expires_at.min(self.callback_expires_at)
    }

    pub fn remaining(&self) -> lillux::time::Duration {
        self.contact_expiry().remaining()
    }
}

// Proposed read-only ORIGINAL authority borrow inside handler_context.rs.
// No public/serde constructor; borrowed from the existing opaque authority.
pub(crate) struct PrecontactBudgetBinding<'a> {
    pub scope: &'a ryeos_state::objects::AdmittedAccountingScope,
    pub root_chain_id: &'a str,
    pub limits_digest: &'a str,
    pub deadline_at_ms: i64,
    pub principal: &'a str,
    pub current_site_id: &'a str,
    pub origin_site_id: &'a str,
    pub callback_invocation_id: &'a str,
    pub callback_thread_id: &'a str,
    pub callback_launch_owner: &'a str,
}

impl OriginalExecutionBudget {
    pub(crate) fn precontact_binding(&self) -> PrecontactBudgetBinding<'_> {
        PrecontactBudgetBinding {
            scope: &self.allocation_scope,
            root_chain_id: &self.owner.root_chain_id,
            limits_digest: &self.owner.limits_digest,
            deadline_at_ms: self.owner.deadline_at_ms,
            principal: &self.owner.principal,
            current_site_id: &self.owner.current_site_id,
            origin_site_id: &self.owner.origin_site_id,
            callback_invocation_id: &self.callback_invocation_id,
            callback_thread_id: &self.callback_thread_id,
            callback_launch_owner: &self.callback_launch_owner,
        }
    }
}

/// Typed caller context, passed to every service handler.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerContext {
    /// Fingerprint of the authenticated caller. Empty when unauthenticated.
    pub fingerprint: String,
    /// Capability scopes granted to the caller. Empty when unauthenticated.
    pub scopes: Vec<String>,
    /// Whether the caller identity was cryptographically verified.
    /// `true` only for signed-request auth (ryeos_signed, hmac).
    /// `false` for anonymous routes and synthetic principals.
    pub verified: bool,
    /// Closed class from the node-signed authorized-key grant. `None` for
    /// non-RyeOS verifiers and synthetic contexts.
    pub authorized_key_class: Option<ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass>,
    /// Site identity established by a remote-node request, or by a
    /// remote-operator request plus its source-node co-signature. Request
    /// payloads never populate this field. `None` denotes a local or non-RyeOS
    /// caller.
    pub authenticated_origin_site_id: Option<String>,
    /// Stable target/source grant generations observed by RyeOS signed
    /// verification. This admission-only sideband is intentionally excluded
    /// from sealed invocation serialization.
    #[serde(skip)]
    pub authenticated_grant_authority:
        Option<ryeos_engine::principal_contract::AuthenticatedGrantAuthority>,
    /// Root of the currently executing recorded service invocation. The
    /// executor supplies this only after that root is durably running; it is
    /// local dispatch context, never caller input or portable authority.
    #[serde(skip)]
    recorded_service_root_id: Option<String>,
    /// Original managed caller budget. Service audit-root birth and JSON
    /// deserialization cannot create, locate or renew this authority.
    #[serde(skip)]
    original_execution_budget: Option<OriginalExecutionBudget>,
}

impl HandlerContext {
    /// Construct from a principal's identity fields.
    pub fn new(fingerprint: String, scopes: Vec<String>, verified: bool) -> Self {
        Self {
            fingerprint,
            scopes,
            verified,
            authorized_key_class: None,
            authenticated_origin_site_id: None,
            authenticated_grant_authority: None,
            recorded_service_root_id: None,
            original_execution_budget: None,
        }
    }

    pub fn new_with_origin(
        fingerprint: String,
        scopes: Vec<String>,
        verified: bool,
        authenticated_origin_site_id: Option<String>,
    ) -> Self {
        Self {
            fingerprint,
            scopes,
            verified,
            authorized_key_class: None,
            authenticated_origin_site_id,
            authenticated_grant_authority: None,
            recorded_service_root_id: None,
            original_execution_budget: None,
        }
    }

    pub fn new_with_authority(
        fingerprint: String,
        scopes: Vec<String>,
        verified: bool,
        authorized_key_class: Option<ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass>,
        authenticated_origin_site_id: Option<String>,
    ) -> Self {
        Self {
            fingerprint,
            scopes,
            verified,
            authorized_key_class,
            authenticated_origin_site_id,
            authenticated_grant_authority: None,
            recorded_service_root_id: None,
            original_execution_budget: None,
        }
    }

    pub fn new_with_grant_authority(
        fingerprint: String,
        scopes: Vec<String>,
        authorized_key_class: ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass,
        authenticated_origin_site_id: Option<String>,
        authenticated_grant_authority:
            ryeos_engine::principal_contract::AuthenticatedGrantAuthority,
    ) -> Self {
        Self {
            fingerprint,
            scopes,
            verified: true,
            authorized_key_class: Some(authorized_key_class),
            authenticated_origin_site_id,
            authenticated_grant_authority: Some(authenticated_grant_authority),
            recorded_service_root_id: None,
            original_execution_budget: None,
        }
    }

    /// Bind the daemon-minted root of this recorded service call. This is an
    /// executor-only causality coordinate, not an authorization input.
    pub fn with_recorded_service_root_id(mut self, root_thread_id: String) -> anyhow::Result<Self> {
        if root_thread_id.is_empty()
            || root_thread_id.len() > 256
            || root_thread_id.trim() != root_thread_id
            || root_thread_id.chars().any(char::is_control)
        {
            anyhow::bail!("recorded service invocation root is not canonical");
        }
        if self
            .recorded_service_root_id
            .replace(root_thread_id)
            .is_some()
        {
            anyhow::bail!("recorded service handler context already has an invocation root");
        }
        Ok(self)
    }

    pub fn recorded_service_root_id(&self) -> Option<&str> {
        self.recorded_service_root_id.as_deref()
    }

    pub fn original_execution_budget(&self) -> Option<&OriginalExecutionBudget> {
        self.original_execution_budget.as_ref()
    }

    /// Bind the same opaque owner to a narrowed callback context. Raw request
    /// fields cannot mint it, and a different owner cannot replace one already
    /// carried by this context.
    pub fn with_original_execution_budget(
        mut self,
        budget: Option<OriginalExecutionBudget>,
    ) -> anyhow::Result<Self> {
        if let Some(budget) = budget {
            if !self.is_present()
                || self.fingerprint != budget.owner.principal
                || self.execution_origin(&budget.owner.current_site_id)
                    != budget.owner.origin_site_id
            {
                anyhow::bail!(
                    "original execution budget differs from authenticated handler authority"
                );
            }
            if self
                .original_execution_budget
                .as_ref()
                .is_some_and(|existing| !Arc::ptr_eq(&existing.owner, &budget.owner))
            {
                anyhow::bail!("handler context cannot replace its original execution budget owner");
            }
            self.original_execution_budget = Some(budget);
        }
        Ok(self)
    }

    /// Revalidate the actual server-side callback pair and its retained CAS
    /// parent before deriving a clock. This reads existing authority only; it
    /// never ensures a budget or borrows the recorded-service audit root.
    /// Existing state/ledger locks are not claimed to have finite acquisition.
    pub fn budget_from_verified_callback(
        state: &crate::state::AppState,
        callback_token: &str,
        thread_auth_token: &str,
        thread_id: &str,
    ) -> anyhow::Result<Option<OriginalExecutionBudget>> {
        use anyhow::Context;
        let cap = state
            .callback_tokens
            .validate_token_and_thread(callback_token, thread_id)?;
        let auth = state.thread_auth.validate(thread_auth_token, thread_id)?;
        let launch_owner = cap
            .launch_owner
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("callback budget has no original launch owner"))?;
        state
            .state_store
            .assert_launch_owner(thread_id, launch_owner)?;
        // Node-internal execution deliberately has no authenticated handler
        // boundary. Preserve that mode for Tool/Graph dispatch; it carries no
        // protected service sideband. Existing service selection preserves
        // verified=false and remote/run refuses it before target contact.
        let Some(handler) = auth
            .handler_context()
            .filter(|context| context.is_present())
        else {
            return Ok(None);
        };
        let limits: ryeos_engine::launch_envelope_types::HardLimits =
            if cap.accounting_scope.is_none() && cap.hard_limits.is_null() {
                // Existing unbudgeted method/augmentation capabilities use
                // Null. A PRESENT scope must still supply valid typed limits.
                Default::default()
            } else {
                serde_json::from_value(cap.hard_limits.clone())
                    .context("decode actual callback aggregate limits")?
            };
        let Some(scope) = cap.accounting_scope.as_ref() else {
            if !limits.aggregate.is_unlimited() {
                anyhow::bail!("bounded callback lost its admitted accounting scope");
            }
            if auth
                .handler_context()
                .and_then(Self::original_execution_budget)
                .is_some()
            {
                anyhow::bail!("callback lost its carried original accounting scope");
            }
            return Ok(None);
        };
        scope.validate()?;
        let caller = state
            .threads
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("callback budget parent thread is absent"))?;
        cap.assert_chain_root(&caller.chain_root_id)?;
        if caller.current_site_id != state.threads.site_id()
            || caller.requested_by.as_deref() != Some(auth.acting_principal.as_str())
        {
            anyhow::bail!("callback budget parent differs from actual thread authority");
        }
        let (parent_root, _, capsule) = state
            .state_store
            .admitted_launch_capsule_with_coordinates(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("callback budget parent has no retained CAS capsule"))?;
        if parent_root != caller.chain_root_id || capsule.accounting_scope.as_ref() != Some(scope) {
            anyhow::bail!("callback budget scope contradicts its retained CAS parent");
        }
        let sealed =
            crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
                &capsule,
            )?;
        let subject = sealed.admitted_program_subject()?;
        // The existing protected workload grant carries the admitted exact
        // delegation intersection. Its capabilities intentionally differ
        // from the root program's own grants; no request DTO can supply it.
        let capability_binding_matches = if let Some(grant) = cap.workload_client_grant.as_ref() {
            grant.validate()?;
            grant.placement_thread_id == thread_id
                && grant.chain_root_id == parent_root
                && grant.owner_principal == auth.acting_principal
                && grant.origin_site_id == caller.origin_site_id
                && grant.root_launch_capsule_hash == capsule.content_hash()?
                && grant.effective_caps == cap.effective_caps
                && grant.effective_caps == auth.caller_scopes
        } else {
            cap.effective_caps == capsule.effective_caps
        };
        if sealed.requested_by() != Some(auth.acting_principal.as_str())
            || sealed.origin_site_id() != caller.origin_site_id
            || cap.item_ref.as_deref() != Some(subject.canonical_ref.as_str())
            || cap.root_raw_content_digest != subject.raw_content_digest
            || cap.effective_definition_digest.as_deref()
                != Some(sealed.effective_definition_digest().as_str())
            || !capability_binding_matches
        {
            anyhow::bail!("callback budget capability contradicts its exact admitted parent");
        }
        let accounting = state
            .accounting
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("present callback accounting scope lost its ledger"))?;
        let (site_id, epoch) = accounting.site_identity();
        if scope.budget_authority_site_id != site_id || scope.ledger_epoch != epoch {
            anyhow::bail!("callback accounting scope differs from original ledger site/epoch");
        }
        // Count lookup work against the wall-to-monotonic conversion rather
        // than starting a fresh relative duration after the reads complete.
        let started = lillux::time::MonotonicTimer::start();
        let observed_at_ms = lillux::time::timestamp_millis();
        let retained = accounting
            .execution_resource_budget_snapshot(&scope.execution_budget_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("present callback scope lost its original resource budget")
            })?;
        let expected_limits = lillux::canonical_json(&serde_json::to_value(&limits.aggregate)?)?;
        if retained.limits_digest != lillux::sha256_hex(expected_limits.as_bytes()) {
            anyhow::bail!("callback aggregate limits differ from retained original budget");
        }
        let (root, _, root_capsule) = state
            .state_store
            .admitted_launch_capsule_with_coordinates(&retained.root_chain_id)?
            .ok_or_else(|| anyhow::anyhow!("original budget root has no retained CAS capsule"))?;
        let root_scope = root_capsule
            .accounting_scope
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("original budget CAS root has no accounting scope"))?;
        // Descendants may have their own directive account; the shared
        // execution account and its first birth must remain identical.
        if root != retained.root_chain_id
            || root_scope.budget_authority_site_id != scope.budget_authority_site_id
            || root_scope.ledger_epoch != scope.ledger_epoch
            || root_scope.execution_budget_id != scope.execution_budget_id
        {
            anyhow::bail!("original execution budget root differs from retained CAS authority");
        }
        let Some(deadline_at_ms) = retained.deadline_at_ms else {
            if limits.aggregate.duration_seconds != 0 {
                anyhow::bail!("bounded callback budget lost its original deadline");
            }
            return Ok(None);
        };
        handler
            .require_verified()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        handler.validate_execution_authority(
            &auth.acting_principal,
            &auth.caller_scopes,
            &caller.current_site_id,
            &caller.origin_site_id,
        )?;
        let callback_expires_at = handler
            .original_execution_budget()
            .map(OriginalExecutionBudget::contact_expiry)
            .unwrap_or(cap.expires_at)
            .min(cap.expires_at)
            .min(auth.expires_at);
        let owner = if let Some(existing) = handler.original_execution_budget() {
            let owner = &existing.owner;
            if owner.scope.budget_authority_site_id != scope.budget_authority_site_id
                || owner.scope.ledger_epoch != scope.ledger_epoch
                || owner.scope.execution_budget_id != scope.execution_budget_id
                || owner.root_chain_id != retained.root_chain_id
                || owner.limits_digest != retained.limits_digest
                || owner.deadline_at_ms != deadline_at_ms
                || owner.principal != auth.acting_principal
                || owner.current_site_id != caller.current_site_id
                || owner.origin_site_id != caller.origin_site_id
            {
                anyhow::bail!("callback cannot replace its carried original budget owner");
            }
            Arc::clone(owner)
        } else {
            let remaining_ms =
                u64::try_from(deadline_at_ms.saturating_sub(observed_at_ms)).unwrap_or(0);
            let remaining =
                lillux::time::Duration::from_millis(remaining_ms).saturating_sub(started.elapsed());
            Arc::new(OriginalExecutionBudgetOwner {
                scope: scope.clone(),
                root_chain_id: retained.root_chain_id,
                limits_digest: retained.limits_digest,
                deadline_at_ms,
                principal: auth.acting_principal,
                current_site_id: caller.current_site_id,
                origin_site_id: caller.origin_site_id,
                expires_at: lillux::time::MonotonicDeadline::after(remaining)
                    .min(cap.expires_at)
                    .min(auth.expires_at),
            })
        };
        let budget = OriginalExecutionBudget {
            owner,
            allocation_scope: scope.clone(),
            callback_thread_id: cap.thread_id.clone(),
            callback_launch_owner: launch_owner.to_owned(),
            callback_invocation_id: cap.invocation_id,
            callback_expires_at,
        };
        if budget.remaining().is_zero() {
            anyhow::bail!("original execution budget expired before callback preparation");
        }
        Ok(Some(budget))
    }

    /// Resolve execution origin from authenticated handler authority only.
    pub fn execution_origin(&self, current_site_id: &str) -> String {
        self.authenticated_origin_site_id
            .as_ref()
            .filter(|_| self.verified)
            .cloned()
            .unwrap_or_else(|| current_site_id.to_string())
    }

    /// Prove that this transport-authenticated context is the exact authority
    /// represented by a sealed execution principal and site pair. Scope order
    /// is not authority; duplicate or reordered grants compare canonically.
    pub fn validate_execution_authority(
        &self,
        principal: &str,
        scopes: &[String],
        current_site_id: &str,
        origin_site_id: &str,
    ) -> anyhow::Result<()> {
        if self.fingerprint != principal {
            anyhow::bail!("handler fingerprint differs from sealed execution principal");
        }
        let mut expected_scopes = scopes.to_vec();
        expected_scopes.sort();
        expected_scopes.dedup();
        let mut actual_scopes = self.scopes.clone();
        actual_scopes.sort();
        actual_scopes.dedup();
        if actual_scopes != expected_scopes {
            anyhow::bail!("handler scopes differ from sealed execution scopes");
        }
        if (!self.verified)
            && (self.authorized_key_class.is_some()
                || self.authenticated_origin_site_id.is_some()
                || self.authenticated_grant_authority.is_some())
        {
            anyhow::bail!("unverified handler context carries authenticated execution authority");
        }
        match self.authorized_key_class {
            Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::LocalClient)
                if self.authenticated_origin_site_id.is_some() =>
            {
                anyhow::bail!("local-client handler context carries a remote origin");
            }
            Some(
                ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::RemoteNode
                | ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::RemoteOperator,
            ) if self.authenticated_origin_site_id.is_none() => {
                anyhow::bail!("remote handler context has no authenticated origin");
            }
            _ => {}
        }
        if let Some(origin) = self.authenticated_origin_site_id.as_deref() {
            ryeos_node_runtime::identity::validate_canonical_site_id(origin)?;
        }
        if self.execution_origin(current_site_id) != origin_site_id {
            anyhow::bail!("handler origin differs from sealed execution origin");
        }
        Ok(())
    }

    /// Narrow only the capability vector while preserving the exact
    /// authenticated principal class and origin established at ingress.
    pub fn narrowed_for_execution(
        &self,
        scopes: Vec<String>,
        current_site_id: &str,
        origin_site_id: &str,
    ) -> anyhow::Result<Self> {
        let mut narrowed = self.clone();
        narrowed.scopes = scopes;
        // A nested execution receives its own lifecycle root. The parent
        // service's append authority must never cross that boundary.
        narrowed.recorded_service_root_id = None;
        narrowed.validate_execution_authority(
            &narrowed.fingerprint,
            &narrowed.scopes,
            current_site_id,
            origin_site_id,
        )?;
        Ok(narrowed)
    }

    /// Anonymous context — no identity, no scopes, not verified.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// Returns true when a verified fingerprint is present.
    ///
    /// Unlike checking `!fingerprint.is_empty()`, this requires
    /// `verified == true`, so synthetic principals from anonymous routes
    /// are correctly excluded.
    pub fn is_present(&self) -> bool {
        self.verified && !self.fingerprint.is_empty()
    }

    /// Returns `Ok(())` when the caller is cryptographically verified.
    ///
    /// Use this as a guard at the top of handlers that read
    /// `fingerprint` or `scopes` directly (not through `require_owner`).
    /// Returns `HandlerError::BadRequest` with a clear message.
    pub fn require_verified(&self) -> Result<(), HandlerError> {
        if self.verified && !self.fingerprint.is_empty() {
            Ok(())
        } else {
            Err(HandlerError::BadRequest(
                "verified caller context required".to_string(),
            ))
        }
    }

    /// Returns true when the caller is verified and matches the resource owner.
    pub fn is_owner(&self, owner: Option<&str>) -> bool {
        if !self.verified {
            return false;
        }
        match owner {
            Some(fp) => fp == self.fingerprint,
            None => false,
        }
    }

    /// Returns `Ok(())` when the caller is verified and is the owner.
    ///
    /// Returns `HandlerError::NotFound` on miss (never 403) to avoid
    /// leaking resource existence to non-owners.
    pub fn require_owner(&self, owner: Option<&str>) -> Result<(), HandlerError> {
        if self.is_owner(owner) {
            Ok(())
        } else {
            Err(HandlerError::NotFound)
        }
    }
}

/// Signed persistence fixture for the joined clock controls. This deliberately
/// bypasses engine root admission and executes no runtime: it tests actual
/// stores, retained CAS verification and ledger reads, never GPU qualification.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod budget_test_support {
    use super::*;
    use ryeos_state::objects::*;
    use std::collections::BTreeMap;

    pub struct Fixture {
        pub state: crate::state::AppState,
        pub context: HandlerContext,
        pub callback_token: String,
        pub thread_auth_token: String,
        pub thread_id: String,
    }

    pub fn build(
        root: &std::path::Path,
        callback_ttl: lillux::time::Duration,
    ) -> anyhow::Result<Fixture> {
        build_with_spend_limit(root, callback_ttl, None)
    }

    /// Actual-store financial fixture; the existing no-cap clock controls
    /// retain their original behavior. No executable root/GPU admission.
    pub fn build_with_spend_limit(
        root: &std::path::Path,
        callback_ttl: lillux::time::Duration,
        spend_limit: Option<ryeos_accounting::UsdNanos>,
    ) -> anyhow::Result<Fixture> {
        let mut state = crate::state::test_support::build(root)?;
        let thread_id = "T-original-budget-fixture";
        let operator = ryeos_node_runtime::identity::NodeIdentity::load(
            &state.config.operator_signing_key_path,
        )?;
        let principal = operator.principal_id();
        let site = state.threads.site_id().to_string();
        let context = HandlerContext::new_with_authority(
            principal.clone(),
            Vec::new(),
            true,
            Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::LocalClient),
            None,
        );
        let accounting = Arc::new(crate::accounting_db::AccountingDb::open_default(
            &root.join("budget-fixture-ledger"),
        )?);
        accounting.create_execution_account_prepared(
            "B-original-fixture",
            thread_id,
            spend_limit,
        )?;
        let limits = ryeos_engine::launch_envelope_types::HardLimits {
            spend_usd: spend_limit.unwrap_or(ryeos_accounting::UsdNanos::ZERO),
            aggregate: ryeos_engine::launch_envelope_types::AggregateExecutionLimits {
                duration_seconds: 60,
                worker_executions: 1,
                provider_contacts: 1,
            },
            ..Default::default()
        };
        accounting.ensure_execution_resource_budget("B-original-fixture", &limits.aggregate)?;
        let (budget_authority_site_id, ledger_epoch) = accounting.site_identity();
        let scope = AdmittedAccountingScope {
            budget_authority_site_id,
            ledger_epoch,
            execution_budget_id: "B-original-fixture".into(),
            directive_budget_id: None,
        };
        state.accounting = Some(accounting);
        let pinned = ExecutionProjectAuthority::pinned(
            "project:budget-fixture".into(),
            None,
            "b".repeat(64),
            PinnedProjectRealization::ReadOnly,
            EnvironmentAuthority::None,
            Vec::new(),
        )?;
        let mut metadata = crate::state_store::external_fixture_launch_metadata(
            pinned,
            serde_json::json!({"binding_records":{}}),
            serde_json::json!({}),
        );
        let project = ExecutionProjectAuthority::PROJECTLESS;
        let mut sealed = serde_json::to_value(metadata.sealed_root_request.as_ref().unwrap())?;
        sealed["current_site_id"] = serde_json::json!(site);
        sealed["origin_site_id"] = serde_json::json!(site);
        sealed["handler_context"] = serde_json::to_value(&context)?;
        sealed["requested_by"] = serde_json::json!(principal);
        sealed["planning_principal"]["fingerprint"] = serde_json::json!(principal);
        sealed["project_authority"] = serde_json::to_value(&project)?;
        sealed["project_context"] =
            serde_json::to_value(ryeos_engine::contracts::ProjectContext::None)?;
        let subject_authority =
            serde_json::to_value(ryeos_engine::contracts::SubjectResolutionAuthority::Projectless)?;
        sealed["project_binding_subject_authority"] = subject_authority.clone();
        sealed["resolution_subject_authority"] = subject_authority.clone();
        sealed["verified_subject"]["subject_resolution_authority"] = subject_authority;
        let AdmittedLaunchArtifactIdentity::ManagedRuntime {
            runtime_content_hash,
            runtime_signer_fingerprint,
            runtime_ref,
            ..
        } = metadata.admitted_artifact_identity.as_ref().unwrap()
        else {
            unreachable!()
        };
        sealed["executor_route"]["runtime_content_hash"] = serde_json::json!(runtime_content_hash);
        sealed["executor_route"]["runtime_signer_fingerprint"] =
            serde_json::json!(runtime_signer_fingerprint);
        let contract_ref = runtime_ref.clone();
        let contract_digest = runtime_content_hash.clone();
        metadata.sealed_root_request = Some(serde_json::from_value(sealed)?);
        metadata.admitted_project_authority = Some(project.clone());
        metadata.accounting_scope = Some(scope.clone());
        let resume = metadata.resume_context.as_mut().unwrap();
        resume.current_site_id = site.clone();
        resume.origin_site_id = site.clone();
        resume.project_authority = project.clone();
        resume.project_context = ryeos_engine::contracts::ProjectContext::None;
        resume.original_snapshot_hash = None;
        resume.requested_by = ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: principal.clone(),
                scopes: Vec::new(),
            },
        );
        let provisional = metadata.admitted_launch_capsule()?.unwrap();
        let authority = provisional.launch_authority();
        let substrate = crate::execution_identity_probe::boot_node_execution_identity(
            &state.state_store,
            &state.identity,
            &crate::build_info::get(),
        )?;
        let realization = AdmittedExecutionRealization {
            schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ADMITTED_EXECUTION_REALIZATION_KIND.into(),
            substrate_identity_hash: substrate.identity_hash.clone(),
            substrate_attestation_hash: substrate.attestation_hash.clone(),
            launch_authority_digest: authority.digest()?,
            effective_definition_digest: provisional.exact_program["effective_definition_digest"]
                .as_str()
                .unwrap()
                .into(),
            artifact_identity_digest: authority.artifact_identity_digest()?,
            execution_closure_digest: authority.execution_closure_digest()?,
            contract_ref,
            contract_digest,
            components: Vec::new(),
            properties: BTreeMap::new(),
        };
        let cas = state.state_store.pinned_state_authority()?.cas_store()?;
        metadata.execution_realization_hash = Some(cas.store_object(&realization.to_value()?)?);
        let sealed = metadata.sealed_root_request.as_ref().unwrap();
        let record = crate::state_store::NewThreadRecord {
            thread_id: thread_id.into(),
            chain_root_id: thread_id.into(),
            kind: "graph_run".into(),
            item_ref: sealed.item_ref().into(),
            executor_ref: sealed.executor_ref().into(),
            launch_mode: "detached".into(),
            current_site_id: site.clone(),
            origin_site_id: site.clone(),
            upstream_thread_id: None,
            requested_by: Some(principal.clone()),
            project_root: None,
            project_authority: project.clone(),
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(sealed.captured_history_policy().clone()),
        };
        state
            .state_store
            .create_root_thread_for_test_with_launch_metadata(&record, &metadata)?;
        let claim = state.state_store.claim_thread_launch(
            thread_id,
            "budget-fixture-claim",
            "daemon:budget-fixture",
        )?;
        anyhow::ensure!(claim == crate::runtime_db::LaunchClaimOutcome::Claimed);
        let launch_owner = state
            .state_store
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("budget fixture lost its original launch claim"))?
            .claimed_by;
        let workspace = root.join("budget-fixture-workspace");
        std::fs::create_dir_all(&workspace)?;
        let provenance = crate::execution_provenance::ExecutionProvenance::root_projectless(
            workspace.clone(),
            state.engine.clone(),
            Arc::new(crate::temp_dir_guard::TempDirGuard::new(workspace)),
            project,
        )?;
        let subject = sealed.admitted_program_subject()?;
        let cap = state.callback_tokens.generate_with_context(
            thread_id,
            root.to_path_buf(),
            callback_ttl,
            Vec::new(),
            provenance,
            None,
            Some(subject.canonical_ref),
            subject.raw_content_digest,
            Some(sealed.effective_definition_digest().to_string()),
            serde_json::to_value(limits)?,
            0,
        );
        anyhow::ensure!(
            state
                .callback_tokens
                .set_launch_owner(&cap.token, launch_owner)
        );
        anyhow::ensure!(
            state
                .callback_tokens
                .set_accounting_scope(&cap.token, scope)
        );
        let auth = state.thread_auth.mint(
            thread_id,
            principal,
            Vec::new(),
            Some(context),
            &site,
            &site,
            callback_ttl,
        )?;
        let budget = HandlerContext::budget_from_verified_callback(
            &state,
            &cap.token,
            &auth.token,
            thread_id,
        )?;
        let context = auth
            .handler_context()
            .unwrap()
            .clone()
            .with_original_execution_budget(budget)?;
        Ok(Fixture {
            state,
            context,
            callback_token: cap.token,
            thread_auth_token: auth.token,
            thread_id: thread_id.into(),
        })
    }

    /// Derive a narrower real callback scope while preserving the original
    /// clock Arc. This records an actual separate audit root/capsule and uses
    /// the real callback and thread-auth stores; no raw budget constructor.
    pub fn derive_directive_callback_for_test(
        fixture: &Fixture,
        directive: &str,
        limit: ryeos_accounting::UsdNanos,
    ) -> anyhow::Result<OriginalExecutionBudget> {
        use anyhow::Context as _;
        let state = &fixture.state;
        let parent = fixture
            .context
            .original_execution_budget()
            .context("actual original budget absent")?;
        let original_remaining = parent.remaining();
        let accounting = state.accounting.as_ref().context("actual ledger absent")?;
        let mut scope = parent.precontact_binding().scope.clone();
        anyhow::ensure!(
            scope.directive_budget_id.is_none(),
            "test parent must have no directive"
        );
        accounting.create_directive_account_prepared(
            &scope.execution_budget_id,
            directive,
            Some(limit),
        )?;
        accounting.activate_account(&scope.execution_budget_id, "directive_item", directive)?;
        scope.directive_budget_id = Some(directive.to_owned());
        let mut metadata = state
            .state_store
            .get_launch_metadata(&fixture.thread_id)?
            .context("actual root metadata absent")?
            .with_accounting_scope(scope.clone());
        let provisional = metadata
            .admitted_launch_capsule()?
            .context("actual child capsule absent")?;
        let authority = state.state_store.pinned_state_authority()?;
        let cas = authority.cas_store()?;
        let mut realization = crate::execution_realization::load_realization(
            state,
            metadata
                .execution_realization_hash
                .as_deref()
                .context("actual node realization absent")?,
        )?;
        realization.launch_authority_digest = provisional.launch_authority().digest()?;
        metadata.execution_realization_hash = Some(cas.store_object(&realization.to_value()?)?);
        let sealed = metadata
            .sealed_root_request
            .as_ref()
            .context("actual sealed subject absent")?;
        let thread_id = "T-original-budget-directive-fixture";
        let site = state.threads.site_id();
        let record = crate::state_store::NewThreadRecord {
            thread_id: thread_id.into(),
            chain_root_id: thread_id.into(),
            kind: "graph_run".into(),
            item_ref: sealed.item_ref().into(),
            executor_ref: sealed.executor_ref().into(),
            launch_mode: "detached".into(),
            current_site_id: site.into(),
            origin_site_id: site.into(),
            upstream_thread_id: None,
            requested_by: Some(fixture.context.fingerprint.clone()),
            project_root: None,
            project_authority: ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(sealed.captured_history_policy().clone()),
        };
        state
            .state_store
            .create_root_thread_for_test_with_launch_metadata(&record, &metadata)?;
        anyhow::ensure!(
            state.state_store.claim_thread_launch(
                thread_id,
                "directive-fixture-claim",
                "daemon:directive-fixture"
            )? == crate::runtime_db::LaunchClaimOutcome::Claimed
        );
        let launch_owner = state
            .state_store
            .get_launch_claim(thread_id)?
            .context("actual directive claim absent")?
            .claimed_by;
        let original = state
            .callback_tokens
            .validate_token_and_thread(&fixture.callback_token, &fixture.thread_id)?;
        let cap = state.callback_tokens.generate_with_context(
            thread_id,
            original.project_path.clone(),
            lillux::time::Duration::from_secs(60),
            original.effective_caps.clone(),
            original.provenance.clone(),
            original.effective_bundle_id.clone(),
            original.item_ref.clone(),
            original.root_raw_content_digest.clone(),
            original.effective_definition_digest.clone(),
            original.hard_limits.clone(),
            original.depth,
        );
        anyhow::ensure!(
            state
                .callback_tokens
                .set_launch_owner(&cap.token, launch_owner)
        );
        anyhow::ensure!(
            state
                .callback_tokens
                .set_accounting_scope(&cap.token, scope.clone())
        );
        let auth = state.thread_auth.mint(
            thread_id,
            fixture.context.fingerprint.clone(),
            original.effective_caps.clone(),
            Some(fixture.context.clone()),
            site,
            site,
            lillux::time::Duration::from_secs(60),
        )?;
        let budget = HandlerContext::budget_from_verified_callback(
            state,
            &cap.token,
            &auth.token,
            thread_id,
        )?
        .context("actual directive callback budget absent")?;
        anyhow::ensure!(
            Arc::ptr_eq(&parent.owner, &budget.owner),
            "directive callback renewed original clock owner"
        );
        anyhow::ensure!(
            budget.precontact_binding().scope == &scope,
            "directive callback lost exact allocation scope"
        );
        anyhow::ensure!(
            budget.remaining() <= original_remaining,
            "directive callback renewed original expiry"
        );
        Ok(budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_remote_origin_is_authoritative() {
        let context = HandlerContext::new_with_origin(
            "fp:remote".to_string(),
            Vec::new(),
            true,
            Some("site:remote".to_string()),
        );
        assert_eq!(context.execution_origin("site:local"), "site:remote");
    }

    #[test]
    fn unverified_origin_claim_is_ignored() {
        let context = HandlerContext::new_with_origin(
            "fp:synthetic".to_string(),
            Vec::new(),
            false,
            Some("site:spoofed".to_string()),
        );
        assert_eq!(context.execution_origin("site:local"), "site:local");
    }

    #[test]
    fn original_budget_authority_requires_actual_stores_cas_and_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let fixture =
            budget_test_support::build(directory.path(), lillux::time::Duration::from_secs(5))
                .unwrap();
        // The same finite parent may legitimately dispatch internal Tool or
        // Graph work without a transport-authenticated handler boundary.
        for context in [
            None,
            Some(HandlerContext::new(
                fixture.context.fingerprint.clone(),
                Vec::new(),
                false,
            )),
        ] {
            let internal = fixture
                .state
                .thread_auth
                .mint(
                    &fixture.thread_id,
                    fixture.context.fingerprint.clone(),
                    Vec::new(),
                    context,
                    fixture.state.threads.site_id(),
                    fixture.state.threads.site_id(),
                    lillux::time::Duration::from_secs(5),
                )
                .unwrap();
            let budget = HandlerContext::budget_from_verified_callback(
                &fixture.state,
                &fixture.callback_token,
                &internal.token,
                &fixture.thread_id,
            )
            .unwrap();
            assert!(budget.is_none());
            let prepared_context = internal
                .narrowed_handler_context(
                    Vec::new(),
                    fixture.state.threads.site_id(),
                    fixture.state.threads.site_id(),
                )
                .unwrap()
                .map(|context| context.with_original_execution_budget(budget))
                .transpose()
                .unwrap();
            assert!(
                !prepared_context
                    .as_ref()
                    .is_some_and(HandlerContext::is_present)
            );
            assert!(
                prepared_context
                    .as_ref()
                    .and_then(HandlerContext::original_execution_budget)
                    .is_none()
            );
        }
        let original_cap = fixture
            .state
            .callback_tokens
            .validate_token_and_thread(&fixture.callback_token, &fixture.thread_id)
            .unwrap();
        // Persistence/store proof only: this fixture intentionally bypasses
        // actual worker/session admission. The real issuer already admits the
        // intersection and stores it before exposing the callback bearer.
        let (_, _, root_capsule) = fixture
            .state
            .state_store
            .admitted_launch_capsule_with_coordinates(&fixture.thread_id)
            .unwrap()
            .unwrap();
        let delegated_caps = vec!["ryeos.execute.tool.project/check".to_owned()];
        assert_ne!(delegated_caps, root_capsule.effective_caps);
        let execution = ryeos_runtime::workload_client::WorkloadClientExecutionCeiling {
            item_ref: "tool:project/check".into(),
            ref_bindings: std::collections::BTreeMap::new(),
            calls: vec![ryeos_runtime::workload_client::WorkloadClientCallCeiling::Default],
            effect_classes: vec!["live".into()],
            workspace_access:
                ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
        };
        let grant = crate::callback_token::AdmittedWorkloadClientGrant {
            schema: crate::callback_token::AdmittedWorkloadClientGrant::SCHEMA,
            protocol: ryeos_runtime::workload_client::WORKLOAD_CLIENT_PROTOCOL.into(),
            chain_root_id: fixture.thread_id.clone(),
            placement_thread_id: fixture.thread_id.clone(),
            owner_principal: fixture.context.fingerprint.clone(),
            origin_site_id: fixture.state.threads.site_id().into(),
            worker_instance_id: "worker-budget-storage-fixture".into(),
            worker_boot_epoch: 1,
            worker_boot_identity_hash: "b".repeat(64),
            root_launch_capsule_hash: root_capsule.content_hash().unwrap(),
            session_capsule_hash: "d".repeat(64),
            project_authority_digest: "e".repeat(64),
            request_digest: "f".repeat(64),
            caller_scope_digest: "1".repeat(64),
            operator_grant_digest: "2".repeat(64),
            root_delegation_digest: "3".repeat(64),
            node_policy_generation_digest: "4".repeat(64),
            ingresses: vec![ryeos_runtime::workload_client::WorkloadClientIngress::Cli],
            execution_presentation: serde_json::json!([{
                "authority": ryeos_runtime::workload_client::execution_ceiling_presentation(&execution)
            }]),
            executions: vec![execution],
            execution_product_selections: std::collections::BTreeMap::new(),
            effective_caps: delegated_caps.clone(),
            max_in_flight: 1,
            max_invocations_per_boot: 1,
            max_lifetime_seconds: 5,
            max_request_bytes: 4096,
        };
        let delegated_budget =
            |stored_grant: Option<crate::callback_token::AdmittedWorkloadClientGrant>,
             cap_scopes: Vec<String>,
             auth_scopes: Vec<String>| {
                let callback = fixture.state.callback_tokens.generate_with_context(
                    &fixture.thread_id,
                    original_cap.project_path.clone(),
                    lillux::time::Duration::from_secs(5),
                    cap_scopes,
                    original_cap.provenance.clone(),
                    original_cap.effective_bundle_id.clone(),
                    original_cap.item_ref.clone(),
                    original_cap.root_raw_content_digest.clone(),
                    original_cap.effective_definition_digest.clone(),
                    original_cap.hard_limits.clone(),
                    original_cap.depth,
                );
                assert!(fixture.state.callback_tokens.set_launch_owner(
                    &callback.token,
                    original_cap.launch_owner.as_ref().unwrap().clone(),
                ));
                assert!(fixture.state.callback_tokens.set_accounting_scope(
                    &callback.token,
                    original_cap.accounting_scope.clone().unwrap()
                ));
                if let Some(grant) = stored_grant {
                    assert!(
                        fixture
                            .state
                            .callback_tokens
                            .set_workload_client_grant(&callback.token, grant)
                            .unwrap()
                    );
                }
                let narrowed = fixture
                    .context
                    .narrowed_for_execution(
                        auth_scopes.clone(),
                        fixture.state.threads.site_id(),
                        fixture.state.threads.site_id(),
                    )
                    .unwrap();
                let auth = fixture
                    .state
                    .thread_auth
                    .mint(
                        &fixture.thread_id,
                        fixture.context.fingerprint.clone(),
                        auth_scopes,
                        Some(narrowed),
                        fixture.state.threads.site_id(),
                        fixture.state.threads.site_id(),
                        lillux::time::Duration::from_secs(5),
                    )
                    .unwrap();
                HandlerContext::budget_from_verified_callback(
                    &fixture.state,
                    &callback.token,
                    &auth.token,
                    &fixture.thread_id,
                )
            };
        let before_delegation = fixture
            .context
            .original_execution_budget()
            .unwrap()
            .remaining();
        let delegated = delegated_budget(
            Some(grant.clone()),
            delegated_caps.clone(),
            delegated_caps.clone(),
        )
        .unwrap()
        .unwrap();
        assert!(Arc::ptr_eq(
            &delegated.owner,
            &fixture.context.original_execution_budget().unwrap().owner
        ));
        assert!(delegated.remaining() <= before_delegation);
        let wrong_caps = vec!["ryeos.execute.tool.project/other".to_owned()];
        assert!(delegated_budget(None, delegated_caps.clone(), delegated_caps.clone()).is_err());
        assert!(
            delegated_budget(
                Some(grant.clone()),
                wrong_caps.clone(),
                delegated_caps.clone()
            )
            .is_err()
        );
        assert!(delegated_budget(Some(grant.clone()), delegated_caps.clone(), wrong_caps).is_err());
        for dimension in 0..5 {
            let mut wrong = grant.clone();
            match dimension {
                0 => wrong.root_launch_capsule_hash = "0".repeat(64),
                1 => wrong.owner_principal = format!("fp:{}", "0".repeat(64)),
                2 => wrong.origin_site_id = "site:other".into(),
                3 => wrong.placement_thread_id = "T-other".into(),
                _ => wrong.chain_root_id = "T-other".into(),
            }
            assert!(
                delegated_budget(Some(wrong), delegated_caps.clone(), delegated_caps.clone())
                    .is_err()
            );
        }
        let unscoped = fixture.state.callback_tokens.generate(
            &fixture.thread_id,
            fixture.state.config.app_root.clone(),
            lillux::time::Duration::from_secs(5),
            Vec::new(),
            original_cap.provenance.clone(),
            original_cap.root_raw_content_digest.clone(),
        );
        assert!(fixture.state.callback_tokens.set_launch_owner(
            &unscoped.token,
            original_cap.launch_owner.as_ref().unwrap().clone(),
        ));
        assert!(
            HandlerContext::budget_from_verified_callback(
                &fixture.state,
                &unscoped.token,
                &fixture.thread_auth_token,
                &fixture.thread_id,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            fixture
                .state
                .callback_tokens
                .set_accounting_scope(&unscoped.token, original_cap.accounting_scope.unwrap())
        );
        assert!(
            HandlerContext::budget_from_verified_callback(
                &fixture.state,
                &unscoped.token,
                &fixture.thread_auth_token,
                &fixture.thread_id,
            )
            .is_err(),
            "PRESENT verified scope with Null limits must refuse"
        );
        let before = fixture
            .context
            .original_execution_budget()
            .unwrap()
            .remaining();
        let narrowed = fixture
            .context
            .narrowed_for_execution(
                Vec::new(),
                fixture.state.threads.site_id(),
                fixture.state.threads.site_id(),
            )
            .unwrap()
            .with_recorded_service_root_id("T-service-audit".into())
            .unwrap();
        assert_eq!(
            narrowed.original_execution_budget(),
            fixture.context.original_execution_budget()
        );
        assert!(narrowed.original_execution_budget().unwrap().remaining() <= before);
        let inherited_auth = fixture
            .state
            .thread_auth
            .mint(
                &fixture.thread_id,
                fixture.context.fingerprint.clone(),
                Vec::new(),
                Some(narrowed.clone()),
                fixture.state.threads.site_id(),
                fixture.state.threads.site_id(),
                lillux::time::Duration::from_secs(30),
            )
            .unwrap();
        let inherited = HandlerContext::budget_from_verified_callback(
            &fixture.state,
            &fixture.callback_token,
            &inherited_auth.token,
            &fixture.thread_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            Some(&inherited),
            fixture.context.original_execution_budget()
        );
        assert!(
            inherited.remaining() <= before,
            "later authentication cannot restart the original clock"
        );
        let wire = serde_json::to_value(&narrowed).unwrap();
        assert!(wire.get("original_execution_budget").is_none());
        let restored: HandlerContext = serde_json::from_value(wire.clone()).unwrap();
        assert!(restored.original_execution_budget().is_none());
        let mut spoofed = wire;
        spoofed["original_execution_budget"] = serde_json::json!({"duration_seconds":60});
        assert!(serde_json::from_value::<HandlerContext>(spoofed).is_err());
        let foreign = HandlerContext::new("session:other".into(), Vec::new(), true);
        assert!(
            foreign
                .with_original_execution_budget(
                    fixture.context.original_execution_budget().cloned()
                )
                .is_err()
        );
        assert!(
            HandlerContext::budget_from_verified_callback(
                &fixture.state,
                "cbt-json-forgery",
                &fixture.thread_auth_token,
                &fixture.thread_id
            )
            .is_err()
        );
        let scope = fixture
            .state
            .callback_tokens
            .validate_token_and_thread(&fixture.callback_token, &fixture.thread_id)
            .unwrap()
            .accounting_scope
            .unwrap();
        for dimension in 0..3 {
            let mut wrong = scope.clone();
            match dimension {
                0 => wrong.budget_authority_site_id = "site:other".into(),
                1 => wrong.ledger_epoch += 1,
                _ => wrong.execution_budget_id = "B-other".into(),
            }
            assert!(
                fixture
                    .state
                    .callback_tokens
                    .set_accounting_scope(&fixture.callback_token, wrong)
            );
            assert!(
                HandlerContext::budget_from_verified_callback(
                    &fixture.state,
                    &fixture.callback_token,
                    &fixture.thread_auth_token,
                    &fixture.thread_id,
                )
                .is_err(),
                "stored scope must agree with its original CAS parent"
            );
        }
        assert!(
            fixture
                .state
                .callback_tokens
                .set_accounting_scope(&fixture.callback_token, scope)
        );
        assert!(
            HandlerContext::budget_from_verified_callback(
                &fixture.state,
                &fixture.callback_token,
                "tat-json-forgery",
                &fixture.thread_id
            )
            .is_err()
        );
        let mut missing_ledger = fixture.state.clone();
        missing_ledger.accounting = None;
        assert!(
            HandlerContext::budget_from_verified_callback(
                &missing_ledger,
                &fixture.callback_token,
                &fixture.thread_auth_token,
                &fixture.thread_id
            )
            .is_err()
        );
        fixture
            .state
            .state_store
            .release_thread_launch_claim(&fixture.thread_id, "budget-fixture-claim")
            .unwrap();
        assert!(
            HandlerContext::budget_from_verified_callback(
                &fixture.state,
                &fixture.callback_token,
                &fixture.thread_auth_token,
                &fixture.thread_id
            )
            .is_err()
        );
    }
}
