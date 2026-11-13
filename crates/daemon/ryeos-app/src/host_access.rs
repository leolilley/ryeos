//! Admission of the host-direct execution posture.
//!
//! A tool that declares `host_authority: required` runs on the host,
//! unconfined, as the daemon user. That is operator-equivalent authority, so
//! it is admitted only when every owner agrees:
//!
//! - the node's isolation lane offers it (`host_direct: operator_granted`);
//! - the operator granted that exact item in the `host_access` node-policy
//!   section, pinned to its signer and its source-closure and runtime bytes;
//! - the caller is the configured local operator, reached directly, from an
//!   operator-rooted graph, or from an operator schedule, as the grant names.
//!
//! Every other caller is refused with a typed code whatever capabilities it
//! holds: workload-client delegation, a directive or worker parent, a remote
//! origin or target, and any principal that is not the configured operator.
//! The execute route checks at admission, before the project authority is
//! sealed; the executor checks again at launch, after launch trust, with the
//! resolved runtime. Nothing here relaxes confinement for any other launch.

use ryeos_engine::isolation::{
    IsolationHostDirect, IsolationHostDirectContainment, IsolationHostDirectGrant, IsolationRuntime,
};
use ryeos_node_runtime::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
pub use ryeos_node_runtime::node_policy::sections::host_access::{
    HostAccessContainment, HostAccessOrigin, HostAccessRefusal, HostAccessSubject,
    NodeHostAccessPolicy,
};

use crate::handler_context::HandlerContext;
use crate::state::AppState;

/// Who is asking, and from where.
#[derive(Clone, Copy)]
pub struct HostAccessCaller<'a> {
    pub acting_principal: &'a str,
    /// The ingress-authenticated caller, when the launch entered through one.
    pub handler_context: Option<&'a HandlerContext>,
    pub target_site_id: Option<&'a str>,
    pub current_site_id: &'a str,
    pub origin_site_id: &'a str,
    /// The thread that launched this one, for a child launch.
    pub parent_thread_id: Option<&'a str>,
    /// A schedule fire launched this root.
    pub scheduled: bool,
}

/// One admitted host-direct launch: the exact grant record that launch
/// provenance carries. Construct only through [`admit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedHostAccess {
    grant: IsolationHostDirectGrant,
}

impl AdmittedHostAccess {
    pub fn grant(&self) -> &IsolationHostDirectGrant {
        &self.grant
    }

    /// The per-launch isolation view this one launch runs under.
    pub fn launch_isolation(
        &self,
        isolation: &IsolationRuntime,
    ) -> Result<IsolationRuntime, HostAccessRefusal> {
        isolation
            .host_direct_launch(self.grant.clone())
            .map_err(|error| {
                let message = error.to_string();
                let code = if message.contains(HostAccessRefusal::CONTAINMENT_UNAVAILABLE) {
                    HostAccessRefusal::CONTAINMENT_UNAVAILABLE
                } else {
                    HostAccessRefusal::LANE_UNAVAILABLE
                };
                HostAccessRefusal::new(code, message)
            })
    }

    /// Whether this launch must run in a cgroup process scope.
    pub fn requires_scope(&self) -> bool {
        self.grant.containment == IsolationHostDirectContainment::Scope
    }
}

/// The effective source-closure content-manifest digest of a composed
/// subject, from its derived projection. `None` when the item has none.
pub fn source_closure_digest(
    derived: Option<&serde_json::Value>,
) -> anyhow::Result<Option<String>> {
    derived
        .map(|value| {
            ryeos_state::objects::EffectiveSourceClosureProjection::from_value(value)
                .map(|projection| projection.content_manifest_hash)
        })
        .transpose()
}

/// Admit one host-direct subject for one caller, or refuse with a typed code.
pub fn admit(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    subject: &HostAccessSubject<'_>,
    caller: &HostAccessCaller<'_>,
) -> Result<AdmittedHostAccess, HostAccessRefusal> {
    let inspection = state.isolation.inspection();
    if inspection.host_direct != IsolationHostDirect::OperatorGranted {
        return Err(HostAccessRefusal::new(
            HostAccessRefusal::LANE_UNAVAILABLE,
            format!(
                "`{}` declares host_authority: required, but this node's host class `{}` says host_direct: {}; a host-authority tool cannot run on this node",
                subject.item,
                inspection.host_class,
                inspection.host_direct.as_str()
            ),
        ));
    }
    let operator =
        NodeIdentity::load(&state.config.operator_signing_key_path).map_err(|error| {
            HostAccessRefusal::new(
                HostAccessRefusal::PRINCIPAL_REFUSED,
                format!("the configured local operator identity cannot be loaded: {error:#}"),
            )
        })?;
    let operator_principal = operator.principal_id();
    require_no_remote_path(subject, caller)?;
    if caller.acting_principal != operator_principal {
        return Err(HostAccessRefusal::new(
            HostAccessRefusal::PRINCIPAL_REFUSED,
            format!(
                "`{}` runs on the host with the operator's authority; only the configured local operator may run it, and the acting principal is `{}` ({})",
                subject.item,
                caller.acting_principal,
                principal_class(caller.handler_context)
            ),
        ));
    }
    if let Some(context) = caller.handler_context
        && (!context.verified
            || context.authorized_key_class != Some(AuthorizedKeyPrincipalClass::LocalClient)
            || context.authenticated_origin_site_id.is_some())
    {
        return Err(HostAccessRefusal::new(
            HostAccessRefusal::PRINCIPAL_REFUSED,
            format!(
                "`{}` admits only a verified local_client request from the configured operator; this request is {}",
                subject.item,
                principal_class(Some(context))
            ),
        ));
    }
    let origin = launch_origin(state, engine, subject, caller, &operator_principal)?;
    let signer = subject.signer.ok_or_else(|| {
        HostAccessRefusal::new(
            HostAccessRefusal::SIGNER_REFUSED,
            format!(
                "`{}` is unsigned; host access needs a pinned signer",
                subject.item
            ),
        )
    })?;
    if signer == state.identity.fingerprint() {
        return Err(HostAccessRefusal::new(
            HostAccessRefusal::SIGNER_REFUSED,
            format!(
                "`{}` is signed by this node's own key {signer}; runtime-authored items never gain host access",
                subject.item
            ),
        ));
    }
    let policy = state
        .node_policy
        .require::<NodeHostAccessPolicy>()
        .map_err(|error| {
            HostAccessRefusal::new(
                HostAccessRefusal::NOT_GRANTED,
                format!("node host_access policy is unavailable: {error:#}"),
            )
        })?;
    let grant = policy.admit(subject, origin)?;
    let runtime = subject.runtime.unwrap_or((
        grant.runtime.runtime_ref.as_str(),
        grant.runtime.content_hash.as_str(),
    ));
    Ok(AdmittedHostAccess {
        grant: IsolationHostDirectGrant {
            policy_generation: state.node_policy.generation_digest().to_owned(),
            grant_id: grant.id.clone(),
            item: grant.item.clone(),
            signer: grant.signer.clone(),
            source_closure: grant.source_closure.clone(),
            runtime_ref: runtime.0.to_owned(),
            runtime_content_hash: runtime.1.to_owned(),
            origin: origin.as_str().to_owned(),
            containment: match grant.containment {
                HostAccessContainment::ScopeRequired => IsolationHostDirectContainment::Scope,
                HostAccessContainment::GroupAllowed => IsolationHostDirectContainment::Group,
            },
            host_env: grant.host_env.clone(),
        },
    })
}

fn require_no_remote_path(
    subject: &HostAccessSubject<'_>,
    caller: &HostAccessCaller<'_>,
) -> Result<(), HostAccessRefusal> {
    let link = if let Some(target) = caller.target_site_id {
        Some(format!("a remote target site `{target}`"))
    } else if caller.origin_site_id != caller.current_site_id {
        Some(format!("a remote origin site `{}`", caller.origin_site_id))
    } else {
        caller
            .handler_context
            .and_then(|context| context.authenticated_origin_site_id.as_deref())
            .map(|site| format!("a forwarded request from site `{site}`"))
    };
    match link {
        Some(link) => Err(HostAccessRefusal::new(
            HostAccessRefusal::DELEGATION_REFUSED,
            format!(
                "`{}` runs only on this node for its local operator; the request carries {link}",
                subject.item
            ),
        )),
        None => Ok(()),
    }
}

fn launch_origin(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    subject: &HostAccessSubject<'_>,
    caller: &HostAccessCaller<'_>,
    operator_principal: &str,
) -> Result<HostAccessOrigin, HostAccessRefusal> {
    let Some(parent) = caller.parent_thread_id else {
        return Ok(if caller.scheduled {
            HostAccessOrigin::Schedule
        } else {
            HostAccessOrigin::Direct
        });
    };
    let delegation = |detail: String| {
        HostAccessRefusal::new(
            HostAccessRefusal::DELEGATION_REFUSED,
            format!(
                "`{}` was launched from thread `{parent}`: {detail}; only an operator-rooted graph may run a host-authority tool as a child",
                subject.item
            ),
        )
    };
    let record = state
        .threads
        .get_thread(parent)
        .map_err(|error| delegation(format!("the parent cannot be read ({error:#})")))?
        .ok_or_else(|| delegation("the parent thread does not exist".to_owned()))?;
    let item_kind = ryeos_engine::canonical_ref::CanonicalRef::parse(subject.item)
        .map(|item| item.kind)
        .map_err(|error| delegation(format!("the item ref is invalid ({error})")))?;
    let graph_profiles = engine
        .kinds
        .get(&item_kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.host_authority.as_ref())
        .map(|declaration| declaration.graph_parent_profiles.as_slice())
        .unwrap_or(&[]);
    // A thread record's kind is its kind schema's thread profile name.
    if !graph_profiles.iter().any(|name| *name == record.kind) {
        return Err(delegation(format!(
            "its parent is a `{}` thread running `{}`, which is delegation, not an operator graph",
            record.kind, record.item_ref
        )));
    }
    if record.requested_by.as_deref() != Some(operator_principal) {
        return Err(delegation(format!(
            "the parent graph was requested by `{}`, not the configured operator",
            record.requested_by.as_deref().unwrap_or("nobody")
        )));
    }
    Ok(HostAccessOrigin::GraphChild)
}

fn principal_class(context: Option<&HandlerContext>) -> String {
    match context {
        None => "node-internal launch".to_owned(),
        Some(context) => match (
            context.authorized_key_class,
            context.authenticated_origin_site_id.as_deref(),
        ) {
            (Some(class), None) => format!("class {class:?}"),
            (Some(class), Some(site)) => format!("class {class:?} from site `{site}`"),
            (None, _) if context.fingerprint.starts_with("session:") => {
                "a browser session".to_owned()
            }
            (None, _) => "no authorized-key class".to_owned(),
        },
    }
}

/// Map an admitted grant's containment onto the sealed launch requirement.
pub fn requires_scope(admitted: Option<&AdmittedHostAccess>) -> bool {
    admitted.is_some_and(AdmittedHostAccess::requires_scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_closure_digest_reads_only_the_manifest_hash() {
        assert_eq!(source_closure_digest(None).unwrap(), None);
        assert!(source_closure_digest(Some(&serde_json::json!({"schema": 99}))).is_err());
    }

    #[test]
    fn principal_class_names_what_was_seen() {
        assert_eq!(principal_class(None), "node-internal launch");
        let session = HandlerContext::new("session:abc".into(), Vec::new(), true);
        assert_eq!(principal_class(Some(&session)), "a browser session");
    }
}
