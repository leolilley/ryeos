//! Envelope checks: who signed a request and whether they may ask for it.
//!
//! The order is fixed. The node fingerprint comes first, then the signature,
//! then the signer's grant, read from disk afresh for every request. Then the
//! operation's exact scope, the authority-bearing rule, the request's time
//! window, and its schema.
//!
//! Remote operators are verified here directly against their grant's key and
//! origin site. That is deliberately not the online admission path, which
//! needs a forwarding proof an offline document cannot carry.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lillux::crypto::VerifyingKey;
use ryeos_engine::roots::RuntimeRoot;
use ryeos_node_runtime::identity::{
    AuthorizedKeyPrincipalClass, NodeIdentity, OfflineConstraint, VerifiedAuthorizedKeyGrant,
    load_verified_authorized_key, matches_only_by_wildcard,
};

use super::contract::{
    CLOCK_SKEW_ALLOWANCE_SECS, Refusal, RefusalReason, RequestBody, SignedRequest,
};
use super::facts::MANDATORY_FACTS;
use super::table::{OperationSpec, lookup, operations};

/// Where the node's authorized-key grants live. The node's bootstrap
/// configuration may name another directory; when it can be read it wins,
/// and when it cannot, the default location is used so that the grants of a
/// node with broken configuration can still be examined and replaced.
pub fn authorized_keys_dir(app_root: &Path) -> PathBuf {
    ryeos_node_runtime::config::Config::load(&ryeos_node_runtime::config::ConfigSources {
        app_root: Some(app_root.to_path_buf()),
        ..Default::default()
    })
    .map(|config| config.authorized_keys_dir)
    .unwrap_or_else(|_| RuntimeRoot::new(app_root.to_path_buf()).authorized_keys_dir())
}

/// The fingerprint and public key of the local operator, read from the node's
/// public operator records: the node never holds the operator's private key.
/// `Ok(None)` when the node has no operator genesis yet; an error when the
/// records are there and do not verify.
pub fn local_operator(app_root: &Path) -> anyhow::Result<Option<(String, VerifyingKey)>> {
    let genesis = RuntimeRoot::new(app_root.to_path_buf()).operator_genesis_path();
    if lillux::inspect_optional_entry_no_follow(&genesis)?.is_none() {
        return Ok(None);
    }
    let operator = ryeos_node_runtime::operator_identity::OperatorPublicIdentity::load(app_root)?;
    Ok(Some((
        operator.fingerprint().to_owned(),
        *operator.verifying_key(),
    )))
}

pub struct Envelope<'a> {
    pub app_root: &'a Path,
    pub node: &'a NodeIdentity,
    /// Unix seconds, from the caller's Lillux clock.
    pub now: u64,
    /// Check the request's time window. A continuation of an application that
    /// began inside its window is not checked against the clock again.
    pub enforce_clock: bool,
}

/// The constraints a delegated grant carries for one operation. When the
/// operation requires a constraint the grant must carry at least one for
/// `scope`; every constraint for `scope` must then hold: it binds a pointer
/// the operation lists, the pointed-at argument is the string it names, and
/// its own `not_after` has not passed at `now`. A constraint only narrows, so
/// an operation that does not require one is still narrowed by any the grant
/// carries.
fn check_grant_constraints(
    requires_constraint: bool,
    bindable_arguments: &[&str],
    scope: &str,
    constraints: &[OfflineConstraint],
    arguments: &serde_json::Value,
    now: u64,
) -> Result<(), Refusal> {
    let mut applicable = constraints
        .iter()
        .filter(|constraint| constraint.scope == scope)
        .peekable();
    if applicable.peek().is_none() {
        if requires_constraint {
            return refuse(
                RefusalReason::GrantConstraintMissing,
                format!(
                    "the grant carries no constraint for the scope `{scope}`, which requires one"
                ),
            );
        }
        return Ok(());
    }
    for constraint in applicable {
        if !bindable_arguments.contains(&constraint.argument.as_str()) {
            return refuse(
                RefusalReason::GrantConstraintViolated,
                format!(
                    "the grant binds `{}`, which `{scope}` does not let a grant bind",
                    constraint.argument
                ),
            );
        }
        if constraint
            .not_after
            .is_some_and(|not_after| now > not_after)
        {
            return refuse(
                RefusalReason::GrantConstraintViolated,
                format!(
                    "the grant's constraint on `{}` for `{scope}` has expired",
                    constraint.argument
                ),
            );
        }
        let held = arguments
            .pointer(&constraint.argument)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value == constraint.equals);
        if !held {
            return refuse(
                RefusalReason::GrantConstraintViolated,
                format!(
                    "the request's `{}` is not the value the grant binds for `{scope}`",
                    constraint.argument
                ),
            );
        }
    }
    Ok(())
}

fn refuse<T>(reason: RefusalReason, detail: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal::new(reason, detail))
}

/// The authority-bearing operations this node's signed policy lets a remote
/// operator request. Read only when a remote signer asks for one; a policy
/// that cannot be read delegates nothing.
fn delegated_remote_operations(app_root: &Path) -> anyhow::Result<BTreeSet<String>> {
    // A node with no policy generation at all delegates nothing; one whose
    // generation is there and cannot be read is an error, which the caller
    // treats as a failed read to retry, not as a refusal.
    if lillux::inspect_optional_entry_no_follow(
        &ryeos_node_runtime::node_policy::generation::policy_directory(app_root),
    )?
    .is_none()
    {
        return Ok(BTreeSet::new());
    }
    let trust = ryeos_engine::trust::TrustStore::load_node_registry(app_root)?;
    let snapshot = ryeos_node_runtime::node_policy::load_snapshot(
        app_root,
        &trust,
        &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
    )?;
    Ok(
        ryeos_node_runtime::node_policy::sections::maintenance::remote_offline_operations(
            &snapshot,
        )?
        .into_iter()
        // A delegation can only ever cover what the table marks delegable.
        .filter(|id| lookup(id).is_some_and(|s| s.authority_bearing && !s.local_signer_only))
        .collect(),
    )
}

impl Envelope<'_> {
    /// Admit the request or say why not. The checks run in the order the
    /// module states.
    pub fn check(&self, request: &SignedRequest) -> Result<(), Refusal> {
        let body = &request.body;
        if body.node_fingerprint != self.node.fingerprint() {
            return refuse(
                RefusalReason::WrongNode,
                format!(
                    "the request is for node {}, this node is {}",
                    body.node_fingerprint,
                    self.node.fingerprint()
                ),
            );
        }
        let grant = self.admit_signer(request)?;
        let Some(spec) = lookup(&body.operation) else {
            return refuse(
                RefusalReason::UnknownOperation,
                format!("this node has no operation `{}`", body.operation),
            );
        };
        if let Some(grant) = &grant {
            // Wildcards never satisfy an offline scope: the exact scope must
            // be named by the grant.
            if !grant.scopes.iter().any(|scope| *scope == spec.scope()) {
                return refuse(
                    RefusalReason::ScopeNotGranted,
                    format!("the grant does not name the scope `{}`", spec.scope()),
                );
            }
            if spec.local_signer_only {
                return refuse(
                    RefusalReason::LocalSignerOnly,
                    format!("`{}` may be signed only by the local operator", spec.id),
                );
            }
            check_grant_constraints(
                spec.requires_grant_constraint,
                spec.bindable_arguments,
                &spec.scope(),
                &grant.offline_constraints,
                &body.arguments,
                self.now,
            )?;
            if spec.authority_bearing {
                match delegated_remote_operations(self.app_root) {
                    Ok(delegated) if delegated.contains(spec.id) => {}
                    Ok(_) => {
                        return refuse(
                            RefusalReason::NotDelegable,
                            format!(
                                "`{}` is authority-bearing and this node's signed profile does not delegate it to remote operators",
                                spec.id
                            ),
                        );
                    }
                    Err(error) => {
                        return Err(Refusal::retryable(
                            RefusalReason::NotDelegable,
                            format!(
                                "`{}` is authority-bearing and the node's signed policy cannot be read to find a delegation: {error:#}",
                                spec.id
                            ),
                        ));
                    }
                }
            }
        }
        if self.enforce_clock {
            self.check_time(body)?;
        }
        check_shape(spec, body)
    }

    /// The signer's grant, or `None` for the local operator, whose authority
    /// is the key on this node's volume.
    fn admit_signer(
        &self,
        request: &SignedRequest,
    ) -> Result<Option<VerifiedAuthorizedKeyGrant>, Refusal> {
        let body = &request.body;
        let local = local_operator(self.app_root).map_err(|error| {
            Refusal::retryable(
                RefusalReason::UnknownSigner,
                format!("the local operator identity cannot be read: {error:#}"),
            )
        })?;
        if let Some((fingerprint, key)) = local
            && fingerprint == body.signer_fingerprint
        {
            request
                .verify_signature(&key)
                .map_err(|e| Refusal::new(RefusalReason::BadSignature, format!("{e:#}")))?;
            if body.signer_grant_digest.is_some() {
                return refuse(
                    RefusalReason::UnknownSigner,
                    "the local operator signs without a grant",
                );
            }
            return Ok(None);
        }
        let directory = authorized_keys_dir(self.app_root);
        let grant =
            match load_verified_authorized_key(&body.signer_fingerprint, &directory, self.node) {
                Ok(Some(grant)) => grant,
                Ok(None) => {
                    return refuse(
                        RefusalReason::UnknownSigner,
                        format!("{} holds no grant on this node", body.signer_fingerprint),
                    );
                }
                Err(error) => {
                    return Err(Refusal::retryable(
                        RefusalReason::UnknownSigner,
                        format!("the signer's grant cannot be verified: {error:#}"),
                    ));
                }
            };
        // Never a remote node, and no ordinary client: only a remote
        // operator may sign an offline operation from outside the volume.
        if grant.principal_class != AuthorizedKeyPrincipalClass::RemotePrincipal {
            return refuse(
                RefusalReason::SignerClassNotPermitted,
                format!(
                    "a `{}` grant may not sign offline operations",
                    grant.principal_class.as_str()
                ),
            );
        }
        request
            .verify_signature(&grant.public_key)
            .map_err(|e| Refusal::new(RefusalReason::BadSignature, format!("{e:#}")))?;
        let Some(signed_under) = &body.signer_grant_digest else {
            return refuse(
                RefusalReason::GrantChanged,
                "a remote operator's request names the grant it was signed under",
            );
        };
        if *signed_under != grant.source_file_hash {
            return refuse(
                RefusalReason::GrantChanged,
                "the signer's grant on this node is not the one the request was signed under",
            );
        }
        let configured = grant.configured_origin_site_id.clone().unwrap_or_default();
        if body.signer_origin_site_id.as_deref() != Some(configured.as_str()) {
            return refuse(
                RefusalReason::OriginMismatch,
                "the request names another origin site than the grant is bound to",
            );
        }
        Ok(Some(grant))
    }

    fn check_time(&self, body: &RequestBody) -> Result<(), Refusal> {
        body.validate_lifetime()
            .map_err(|e| Refusal::new(RefusalReason::InvalidLifetime, format!("{e:#}")))?;
        let not_after = body
            .not_after()
            .map_err(|e| Refusal::new(RefusalReason::InvalidLifetime, format!("{e:#}")))?;
        let issued_at = body
            .issued_at()
            .map_err(|e| Refusal::new(RefusalReason::InvalidLifetime, format!("{e:#}")))?;
        if self.now > not_after.unix_secs() {
            return refuse(
                RefusalReason::Expired,
                format!("the request expired at {}", body.not_after),
            );
        }
        if issued_at.unix_secs() > self.now.saturating_add(CLOCK_SKEW_ALLOWANCE_SECS) {
            return refuse(
                RefusalReason::FromTheFuture,
                format!(
                    "the request was issued at {}, after this node's clock",
                    body.issued_at
                ),
            );
        }
        Ok(())
    }
}

/// Arguments, input slots, pinned facts and pinned results against the
/// operation's compiled schema.
fn check_shape(spec: &OperationSpec, body: &RequestBody) -> Result<(), Refusal> {
    (spec.validate_request)(body)
        .map_err(|e| Refusal::new(RefusalReason::InvalidArguments, format!("{e:#}")))?;
    for slot in body.inputs.keys() {
        if spec.slot(slot).is_none() {
            return refuse(
                RefusalReason::InvalidInputs,
                format!("`{}` has no input slot `{slot}`", spec.id),
            );
        }
    }
    for slot in spec.inputs.iter().filter(|slot| slot.required) {
        if !body.inputs.contains_key(slot.name) {
            return refuse(
                RefusalReason::InvalidInputs,
                format!("`{}` requires the input `{}`", spec.id, slot.name),
            );
        }
    }
    let wanted: BTreeSet<&str> = MANDATORY_FACTS
        .iter()
        .chain(spec.preconditions)
        .copied()
        .collect();
    let pinned: BTreeSet<&str> = body.precondition.keys().map(String::as_str).collect();
    if wanted != pinned {
        return refuse(
            RefusalReason::PreconditionFailed,
            format!(
                "`{}` pins the facts {wanted:?}; the request pins {pinned:?}",
                spec.id
            ),
        );
    }
    for fact in body.expected_result.keys() {
        if !spec.expected_result.contains(&fact.as_str()) {
            return refuse(
                RefusalReason::ExpectedResultMismatch,
                format!("`{}` cannot report the result fact `{fact}`", spec.id),
            );
        }
    }
    for fact in spec.required_expected_result {
        if !body.expected_result.contains_key(*fact) {
            return refuse(
                RefusalReason::ExpectedResultMismatch,
                format!(
                    "`{}` requires the request to pin the result fact `{fact}`",
                    spec.id
                ),
            );
        }
    }
    Ok(())
}

/// A grant that would have satisfied an offline scope under ordinary
/// wildcard matching, and the operations it would have reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WildcardGrant {
    pub fingerprint: String,
    pub operations: Vec<&'static str>,
}

/// Grants whose wildcard scopes would match offline operations. The node
/// reports them at each startup; they carry no offline power. The local
/// operator's own grant is left out: the operator signs offline operations by
/// holding the key on this node's volume, not through any grant.
pub fn wildcard_only_grants(app_root: &Path, node: &NodeIdentity) -> Vec<WildcardGrant> {
    let operator = local_operator(app_root)
        .ok()
        .flatten()
        .map(|(fingerprint, _)| fingerprint);
    let directory = authorized_keys_dir(app_root);
    let Ok(Some(pinned)) = lillux::PinnedDirectory::open(&directory) else {
        return Vec::new();
    };
    let Ok(entries) = pinned.entries_no_follow_bounded(4096) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries {
        let Some(name) = entry.name.to_str() else {
            continue;
        };
        let Some(fingerprint) = name.strip_suffix(".toml") else {
            continue;
        };
        if operator.as_deref() == Some(fingerprint) {
            continue;
        }
        let Ok(Some(grant)) = load_verified_authorized_key(fingerprint, &directory, node) else {
            continue;
        };
        let reached: Vec<&'static str> = operations()
            .iter()
            .filter(|spec| matches_only_by_wildcard(&grant.scopes, &spec.scope()))
            .map(|spec| spec.id)
            .collect();
        if !reached.is_empty() {
            found.push(WildcardGrant {
                fingerprint: fingerprint.to_owned(),
                operations: reached,
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCOPE: &str = "ryeos.offline.node.key.link";
    const BINDABLE: &[&str] = &["/link/body/key"];

    fn constraint(equals: &str, not_after: Option<u64>) -> OfflineConstraint {
        OfflineConstraint {
            scope: SCOPE.to_owned(),
            argument: "/link/body/key".to_owned(),
            equals: equals.to_owned(),
            max_uses: 1,
            not_after,
        }
    }

    fn check(
        requires: bool,
        constraints: &[OfflineConstraint],
        arguments: &serde_json::Value,
        now: u64,
    ) -> Result<(), Refusal> {
        check_grant_constraints(requires, BINDABLE, SCOPE, constraints, arguments, now)
    }

    fn reason(result: Result<(), Refusal>) -> RefusalReason {
        result.unwrap_err().reason
    }

    #[test]
    fn a_grant_without_a_constraint_is_refused_where_one_is_required() {
        let arguments = json!({ "link": { "body": { "key": "fp:a" } } });
        assert_eq!(
            reason(check(true, &[], &arguments, 10)),
            RefusalReason::GrantConstraintMissing
        );
        let other_scope = OfflineConstraint {
            scope: "ryeos.offline.node.allow-restart".to_owned(),
            ..constraint("fp:a", None)
        };
        assert_eq!(
            reason(check(true, &[other_scope], &arguments, 10)),
            RefusalReason::GrantConstraintMissing
        );
        check(false, &[], &arguments, 10).unwrap();
    }

    #[test]
    fn a_constraint_admits_only_the_bound_argument() {
        let constraints = [constraint("fp:a", None)];
        check(
            true,
            &constraints,
            &json!({ "link": { "body": { "key": "fp:a" } } }),
            10,
        )
        .unwrap();
        for arguments in [
            json!({ "link": { "body": { "key": "fp:b" } } }),
            json!({ "link": { "body": {} } }),
            json!({ "link": { "body": { "key": 7 } } }),
            json!({}),
        ] {
            assert_eq!(
                reason(check(true, &constraints, &arguments, 10)),
                RefusalReason::GrantConstraintViolated,
                "{arguments}"
            );
        }
    }

    #[test]
    fn an_expired_constraint_is_refused() {
        let arguments = json!({ "link": { "body": { "key": "fp:a" } } });
        let constraints = [constraint("fp:a", Some(100))];
        check(true, &constraints, &arguments, 100).unwrap();
        assert_eq!(
            reason(check(true, &constraints, &arguments, 101)),
            RefusalReason::GrantConstraintViolated
        );
    }

    #[test]
    fn every_constraint_for_the_scope_must_hold() {
        let arguments = json!({ "link": { "body": { "key": "fp:a" } } });
        let constraints = [constraint("fp:a", None), constraint("fp:b", None)];
        assert_eq!(
            reason(check(true, &constraints, &arguments, 10)),
            RefusalReason::GrantConstraintViolated
        );
    }

    #[test]
    fn a_constraint_on_an_unbindable_pointer_never_holds() {
        let unbindable = OfflineConstraint {
            argument: "/link/body/principal".to_owned(),
            ..constraint("pid:x", None)
        };
        assert_eq!(
            reason(check(
                true,
                &[unbindable],
                &json!({ "link": { "body": { "principal": "pid:x" } } }),
                10
            )),
            RefusalReason::GrantConstraintViolated
        );
    }
}
