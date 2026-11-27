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

/// `fp:` and the bare fingerprint, normalised to the bare form.
fn bare_fingerprint(text: &str) -> &str {
    text.strip_prefix("fp:").unwrap_or(text)
}

/// The constraints a delegated grant carries for one operation. When the
/// operation requires a constraint the grant must carry at least one for
/// `spec`'s scope; every constraint for that scope must then hold: it binds a
/// pointer the operation lists, the pointed-at argument is the string it
/// names, and its own `not_after` has not passed at `now`. A constraint only
/// narrows, so an operation that does not require one is still narrowed by
/// any the grant carries.
///
/// An operation with a `single_use_grant` also needs every constraint to
/// allow exactly one use and to name a `not_after`, which `not_after_ceiling`
/// (an absolute Unix time, when the node has one) may not exceed. An operation
/// with a `signer_bound_argument` needs that argument to name the request's
/// own signer, and one constraint to bind exactly that pointer.
fn check_grant_constraints(
    spec: &OperationSpec,
    constraints: &[OfflineConstraint],
    arguments: &serde_json::Value,
    signer_fingerprint: &str,
    now: u64,
    not_after_ceiling: Option<u64>,
) -> Result<(), Refusal> {
    let scope = spec.scope();
    let scope = scope.as_str();
    let mut applicable = constraints
        .iter()
        .filter(|constraint| constraint.scope == scope)
        .peekable();
    if applicable.peek().is_none() {
        if spec.requires_grant_constraint {
            return refuse(
                RefusalReason::GrantConstraintMissing,
                format!(
                    "the grant carries no constraint for the scope `{scope}`, which requires one"
                ),
            );
        }
        return Ok(());
    }
    if let Some(pointer) = spec.signer_bound_argument {
        let names_signer = arguments
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| bare_fingerprint(value) == bare_fingerprint(signer_fingerprint));
        if !names_signer {
            return refuse(
                RefusalReason::SignerBoundArgumentMismatch,
                format!(
                    "the request's `{pointer}` is not the signer's own key, which `{scope}` requires"
                ),
            );
        }
    }
    let mut binds_signer_pointer = false;
    for constraint in applicable {
        if !spec
            .bindable_arguments
            .contains(&constraint.argument.as_str())
        {
            return refuse(
                RefusalReason::GrantConstraintViolated,
                format!(
                    "the grant binds `{}`, which `{scope}` does not let a grant bind",
                    constraint.argument
                ),
            );
        }
        binds_signer_pointer |= spec.signer_bound_argument == Some(constraint.argument.as_str());
        if spec.single_use_grant {
            if constraint.max_uses != 1 {
                return refuse(
                    RefusalReason::GrantConstraintViolated,
                    format!(
                        "`{scope}` takes only a single-use grant; the constraint on `{}` allows {} uses",
                        constraint.argument, constraint.max_uses
                    ),
                );
            }
            let Some(not_after) = constraint.not_after else {
                return refuse(
                    RefusalReason::GrantConstraintViolated,
                    format!(
                        "`{scope}` takes only a grant whose constraint on `{}` names a not_after",
                        constraint.argument
                    ),
                );
            };
            let Some(ceiling) = not_after_ceiling else {
                return refuse(
                    RefusalReason::GrantCeilingUnavailable,
                    format!(
                        "`{scope}` takes only a single-use grant, and this node's signed policy carries no limit on how long one may last"
                    ),
                );
            };
            if not_after > ceiling {
                return refuse(
                    RefusalReason::GrantConstraintViolated,
                    format!(
                        "the grant's constraint on `{}` for `{scope}` lasts longer than this node allows",
                        constraint.argument
                    ),
                );
            }
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
    if let Some(pointer) = spec.signer_bound_argument
        && !binds_signer_pointer
    {
        return refuse(
            RefusalReason::GrantConstraintMissing,
            format!("no constraint of the grant binds `{pointer}` for the scope `{scope}`"),
        );
    }
    Ok(())
}

/// The fewest uses any constraint of the grant allows for `scope`, which is
/// how many uses the grant has for it; `None` when it carries no constraint
/// for the scope.
pub(super) fn smallest_max_uses(constraints: &[OfflineConstraint], scope: &str) -> Option<u32> {
    constraints
        .iter()
        .filter(|constraint| constraint.scope == scope)
        .map(|constraint| constraint.max_uses)
        .min()
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
            if spec.operator_issued_grant {
                self.check_issued_by_operator_owner(grant)?;
            }
            check_grant_constraints(
                spec,
                &grant.offline_constraints,
                &body.arguments,
                &body.signer_fingerprint,
                self.now,
                self.grant_not_after_ceiling(),
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

    /// The latest `not_after`, as an absolute Unix time, a single-use grant's
    /// constraint may carry. This is where the node's own bound on how far
    /// ahead a delegated grant may reach plugs in: it is node data, and no
    /// such data exists yet, so no bound is applied.
    fn grant_not_after_ceiling(&self) -> Option<u64> {
        None
    }

    /// The grant must have been asked for by an active key of the principal
    /// that owns the node's operator key: only the operator's own side may
    /// delegate the right to join a key to the operator.
    fn check_issued_by_operator_owner(
        &self,
        grant: &VerifiedAuthorizedKeyGrant,
    ) -> Result<(), Refusal> {
        let reason = RefusalReason::KeyLinkGrantNotOperatorIssued;
        let operator = local_operator(self.app_root).map_err(|error| {
            Refusal::retryable(
                reason,
                format!("the local operator identity cannot be read: {error:#}"),
            )
        })?;
        let Some((operator_fingerprint, _)) = operator else {
            return refuse(reason, "this node has no operator to have issued the grant");
        };
        let trust = ryeos_engine::trust::TrustStore::load_node_registry(self.app_root).map_err(
            |error| {
                Refusal::retryable(
                    reason,
                    format!("the node's signer registry cannot be read: {error}"),
                )
            },
        )?;
        crate::key_link::ensure_issued_by_operator_owner(
            &trust,
            &operator_fingerprint,
            &grant.authorized_by.principal,
        )
        .or_else(|detail| refuse(reason, detail))
    }

    /// The signer's grant, or `None` for the local operator, whose authority
    /// is the key on this node's volume.
    pub(super) fn admit_signer(
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
    const POINTER: &str = "/link/body/key";

    fn signer() -> String {
        "ab".repeat(32)
    }

    fn signer_key() -> String {
        format!("fp:{}", signer())
    }

    fn constraint(equals: &str, not_after: Option<u64>) -> OfflineConstraint {
        OfflineConstraint {
            scope: SCOPE.to_owned(),
            argument: POINTER.to_owned(),
            equals: equals.to_owned(),
            max_uses: 1,
            not_after,
        }
    }

    fn arguments_for(key: &str) -> serde_json::Value {
        json!({ "link": { "body": { "key": key } } })
    }

    /// The constraints against `node.key.link`, signed by `signer()`.
    fn check(
        constraints: &[OfflineConstraint],
        arguments: &serde_json::Value,
        now: u64,
    ) -> Result<(), Refusal> {
        check_grant_constraints(
            lookup("node.key.link").unwrap(),
            constraints,
            arguments,
            &signer(),
            now,
            Some(u64::MAX),
        )
    }

    fn reason(result: Result<(), Refusal>) -> RefusalReason {
        result.unwrap_err().reason
    }

    #[test]
    fn a_grant_without_a_constraint_is_refused_where_one_is_required() {
        let arguments = arguments_for(&signer_key());
        assert_eq!(
            reason(check(&[], &arguments, 10)),
            RefusalReason::GrantConstraintMissing
        );
        let other_scope = OfflineConstraint {
            scope: "ryeos.offline.node.allow-restart".to_owned(),
            ..constraint(&signer_key(), Some(100))
        };
        assert_eq!(
            reason(check(&[other_scope], &arguments, 10)),
            RefusalReason::GrantConstraintMissing
        );
        check_grant_constraints(
            lookup("node.allow-restart").unwrap(),
            &[],
            &arguments,
            &signer(),
            10,
            None,
        )
        .unwrap();
    }

    #[test]
    fn the_signer_bound_argument_must_name_the_signers_own_key() {
        let constraints = [constraint(&signer_key(), Some(100))];
        check(&constraints, &arguments_for(&signer_key()), 10).unwrap();
        // The bare fingerprint names the same key.
        check(
            &[constraint(&signer(), Some(100))],
            &arguments_for(&signer()),
            10,
        )
        .unwrap();
        let other = format!("fp:{}", "cd".repeat(32));
        for arguments in [
            arguments_for(&other),
            json!({ "link": { "body": {} } }),
            json!({ "link": { "body": { "key": 7 } } }),
            json!({}),
        ] {
            // Even a grant that binds exactly that other key does not admit it.
            assert_eq!(
                reason(check(&[constraint(&other, Some(100))], &arguments, 10)),
                RefusalReason::SignerBoundArgumentMismatch,
                "{arguments}"
            );
        }
    }

    #[test]
    fn a_constraint_admits_only_the_bound_argument() {
        let other = format!("fp:{}", "cd".repeat(32));
        assert_eq!(
            reason(check(
                &[constraint(&other, Some(100))],
                &arguments_for(&signer_key()),
                10
            )),
            RefusalReason::GrantConstraintViolated
        );
    }

    #[test]
    fn an_expired_constraint_is_refused() {
        let arguments = arguments_for(&signer_key());
        let constraints = [constraint(&signer_key(), Some(100))];
        check(&constraints, &arguments, 100).unwrap();
        assert_eq!(
            reason(check(&constraints, &arguments, 101)),
            RefusalReason::GrantConstraintViolated
        );
    }

    #[test]
    fn a_single_use_operation_refuses_many_uses_and_a_missing_not_after() {
        let arguments = arguments_for(&signer_key());
        let mut many = constraint(&signer_key(), Some(100));
        many.max_uses = 2;
        assert_eq!(
            reason(check(&[many], &arguments, 10)),
            RefusalReason::GrantConstraintViolated
        );
        assert_eq!(
            reason(check(&[constraint(&signer_key(), None)], &arguments, 10)),
            RefusalReason::GrantConstraintViolated
        );
    }

    #[test]
    fn a_single_use_grant_is_refused_while_the_node_has_no_ceiling() {
        let arguments = arguments_for(&signer_key());
        let constraints = [constraint(&signer_key(), Some(100))];
        assert_eq!(
            reason(check_grant_constraints(
                lookup("node.key.link").unwrap(),
                &constraints,
                &arguments,
                &signer(),
                10,
                None,
            )),
            RefusalReason::GrantCeilingUnavailable
        );
    }

    #[test]
    fn the_not_after_ceiling_bounds_a_single_use_constraint() {
        let arguments = arguments_for(&signer_key());
        let constraints = [constraint(&signer_key(), Some(100))];
        let with_ceiling = |ceiling| {
            check_grant_constraints(
                lookup("node.key.link").unwrap(),
                &constraints,
                &arguments,
                &signer(),
                10,
                ceiling,
            )
        };
        with_ceiling(Some(100)).unwrap();
        assert_eq!(
            reason(with_ceiling(Some(99))),
            RefusalReason::GrantConstraintViolated
        );
    }

    #[test]
    fn every_constraint_for_the_scope_must_hold() {
        let arguments = arguments_for(&signer_key());
        let other = format!("fp:{}", "cd".repeat(32));
        let constraints = [
            constraint(&signer_key(), Some(100)),
            constraint(&other, Some(100)),
        ];
        assert_eq!(
            reason(check(&constraints, &arguments, 10)),
            RefusalReason::GrantConstraintViolated
        );
    }

    #[test]
    fn a_constraint_on_an_unbindable_pointer_never_holds() {
        let unbindable = OfflineConstraint {
            argument: "/link/body/principal".to_owned(),
            ..constraint("pid:x", Some(100))
        };
        assert_eq!(
            reason(check(&[unbindable], &arguments_for(&signer_key()), 10)),
            RefusalReason::GrantConstraintViolated
        );
    }

    #[test]
    fn the_uses_a_grant_has_are_the_smallest_any_constraint_allows() {
        let mut two = constraint(&signer_key(), Some(100));
        two.max_uses = 2;
        let constraints = [two, constraint(&signer_key(), Some(100))];
        assert_eq!(smallest_max_uses(&constraints, SCOPE), Some(1));
        assert_eq!(smallest_max_uses(&constraints, "other"), None);
    }
}
