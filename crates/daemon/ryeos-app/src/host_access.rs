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
    use ryeos_node_runtime::node_policy::sections::host_access::{
        HostAccessGrant, HostAccessRuntimePin,
    };

    const ITEM: &str = "tool:ryeos/development/dev-host/guarded-cargo";
    const RUNTIME: &str = "tool:ryeos/core/runtimes/python/script";

    fn signer() -> String {
        "a".repeat(64)
    }

    fn grant(origins: Vec<HostAccessOrigin>) -> HostAccessGrant {
        HostAccessGrant {
            id: "guarded-cargo".into(),
            item: ITEM.into(),
            signer: signer(),
            source_closure: Some("b".repeat(64)),
            runtime: HostAccessRuntimePin {
                runtime_ref: RUNTIME.into(),
                content_hash: "c".repeat(64),
            },
            origins,
            host_env: vec!["PATH".into()],
            containment: HostAccessContainment::GroupAllowed,
        }
    }

    /// A composed App on a lane that offers host access, holding `grants`.
    fn state_with(
        root: &std::path::Path,
        lane: IsolationHostDirect,
        grants: Vec<HostAccessGrant>,
    ) -> (crate::state_store::test_native::TestStateStore, AppState) {
        let (store, mut state) = crate::state::test_support::build(root).unwrap();
        let mut policy = ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring();
        policy.host_direct = lane;
        state.isolation = std::sync::Arc::new(IsolationRuntime::for_test_policy(policy).unwrap());
        state.node_policy = std::sync::Arc::new(
            ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(vec![
                std::sync::Arc::new(NodeHostAccessPolicy { schema: 1, grants }),
            ]),
        );
        (store, state)
    }

    fn operator(state: &AppState) -> String {
        NodeIdentity::load(&state.config.operator_signing_key_path)
            .unwrap()
            .principal_id()
    }

    fn subject<'a>(signer: &'a str, closure: &'a str) -> HostAccessSubject<'a> {
        HostAccessSubject {
            item: ITEM,
            signer: Some(signer),
            source_closure: Some(closure),
            runtime: None,
        }
    }

    fn caller<'a>(
        state: &'a AppState,
        principal: &'a str,
        context: Option<&'a HandlerContext>,
    ) -> HostAccessCaller<'a> {
        let site = state.threads.site_id();
        HostAccessCaller {
            acting_principal: principal,
            handler_context: context,
            target_site_id: None,
            current_site_id: site,
            origin_site_id: site,
            parent_thread_id: None,
            scheduled: false,
        }
    }

    fn local_operator(principal: &str) -> HandlerContext {
        HandlerContext::new_with_authority(
            principal.to_owned(),
            vec!["*".into()],
            true,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        )
    }

    fn refused(result: Result<AdmittedHostAccess, HostAccessRefusal>) -> HostAccessRefusal {
        match result {
            Ok(admitted) => panic!("expected a refusal, admitted {admitted:?}"),
            Err(refusal) => refusal,
        }
    }

    #[test]
    fn the_configured_operator_is_admitted_with_the_exact_grant() {
        let root = tempfile::tempdir().unwrap();
        let (_store, state) = state_with(
            root.path(),
            IsolationHostDirect::OperatorGranted,
            vec![grant(vec![HostAccessOrigin::Direct])],
        );
        let principal = operator(&state);
        let context = local_operator(&principal);
        let (signer, closure) = (signer(), "b".repeat(64));
        let admitted = admit(
            &state,
            &state.engine,
            &subject(&signer, &closure),
            &caller(&state, &principal, Some(&context)),
        )
        .unwrap();
        assert_eq!(admitted.grant().grant_id, "guarded-cargo");
        assert_eq!(admitted.grant().origin, "direct");
        assert_eq!(admitted.grant().runtime_ref, RUNTIME);
        assert!(!admitted.requires_scope());
        assert_eq!(
            admitted.grant().policy_generation,
            state.node_policy.generation_digest()
        );
    }

    #[test]
    fn an_unavailable_lane_refuses_first() {
        let root = tempfile::tempdir().unwrap();
        let (_store, state) = state_with(
            root.path(),
            IsolationHostDirect::Unavailable,
            vec![grant(vec![HostAccessOrigin::Direct])],
        );
        let principal = operator(&state);
        let (signer, closure) = (signer(), "b".repeat(64));
        let refusal = refused(admit(
            &state,
            &state.engine,
            &subject(&signer, &closure),
            &caller(&state, &principal, None),
        ));
        assert_eq!(refusal.code, HostAccessRefusal::LANE_UNAVAILABLE);
        assert!(refusal.message.contains("unavailable"), "{refusal}");
    }

    #[test]
    fn host_access_principal_refused_for_anyone_but_the_local_operator() {
        let root = tempfile::tempdir().unwrap();
        let (_store, state) = state_with(
            root.path(),
            IsolationHostDirect::OperatorGranted,
            vec![grant(vec![HostAccessOrigin::Direct])],
        );
        let (signer, closure) = (signer(), "b".repeat(64));
        let item = subject(&signer, &closure);

        // Another principal holding `*` is still refused.
        let other = format!("fp:{}", "d".repeat(64));
        let context = local_operator(&other);
        let refusal = refused(admit(
            &state,
            &state.engine,
            &item,
            &caller(&state, &other, Some(&context)),
        ));
        assert_eq!(refusal.code, HostAccessRefusal::PRINCIPAL_REFUSED);
        assert!(refusal.message.contains(&other), "{refusal}");

        // A browser session.
        let session = "session:browser-1";
        let context = HandlerContext::new(session.into(), vec!["*".into()], true);
        let refusal = refused(admit(
            &state,
            &state.engine,
            &item,
            &caller(&state, session, Some(&context)),
        ));
        assert_eq!(refusal.code, HostAccessRefusal::PRINCIPAL_REFUSED);
        assert!(refusal.message.contains("browser session"), "{refusal}");

        // The operator's fingerprint over an unverified or non-local key class.
        let principal = operator(&state);
        for context in [
            HandlerContext::new_with_authority(
                principal.clone(),
                vec!["*".into()],
                false,
                Some(AuthorizedKeyPrincipalClass::LocalClient),
                None,
            ),
            HandlerContext::new_with_authority(
                principal.clone(),
                vec!["*".into()],
                true,
                Some(AuthorizedKeyPrincipalClass::RemoteNode),
                None,
            ),
        ] {
            let refusal = refused(admit(
                &state,
                &state.engine,
                &item,
                &caller(&state, &principal, Some(&context)),
            ));
            assert_eq!(refusal.code, HostAccessRefusal::PRINCIPAL_REFUSED);
        }
    }

    #[test]
    fn host_access_delegation_refused_for_remote_paths_and_unknown_parents() {
        let root = tempfile::tempdir().unwrap();
        let (_store, state) = state_with(
            root.path(),
            IsolationHostDirect::OperatorGranted,
            vec![grant(vec![
                HostAccessOrigin::Direct,
                HostAccessOrigin::GraphChild,
            ])],
        );
        let principal = operator(&state);
        let (signer, closure) = (signer(), "b".repeat(64));
        let item = subject(&signer, &closure);

        let mut remote_target = caller(&state, &principal, None);
        remote_target.target_site_id = Some("site:elsewhere");
        let refusal = refused(admit(&state, &state.engine, &item, &remote_target));
        assert_eq!(refusal.code, HostAccessRefusal::DELEGATION_REFUSED);
        assert!(refusal.message.contains("site:elsewhere"), "{refusal}");

        let mut remote_origin = caller(&state, &principal, None);
        remote_origin.origin_site_id = "site:origin";
        let refusal = refused(admit(&state, &state.engine, &item, &remote_origin));
        assert_eq!(refusal.code, HostAccessRefusal::DELEGATION_REFUSED);

        let forwarded = HandlerContext::new_with_authority(
            principal.clone(),
            vec!["*".into()],
            true,
            Some(AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:source".into()),
        );
        let refusal = refused(admit(
            &state,
            &state.engine,
            &item,
            &caller(&state, &principal, Some(&forwarded)),
        ));
        assert_eq!(refusal.code, HostAccessRefusal::DELEGATION_REFUSED);
        assert!(refusal.message.contains("site:source"), "{refusal}");

        // A child whose parent is not a known operator graph thread.
        let mut child = caller(&state, &principal, None);
        child.parent_thread_id = Some("T-not-a-thread");
        let refusal = refused(admit(&state, &state.engine, &item, &child));
        assert_eq!(refusal.code, HostAccessRefusal::DELEGATION_REFUSED);
        assert!(refusal.message.contains("T-not-a-thread"), "{refusal}");
    }

    #[test]
    fn origin_and_signer_refusals_name_what_was_seen() {
        let root = tempfile::tempdir().unwrap();
        let (_store, state) = state_with(
            root.path(),
            IsolationHostDirect::OperatorGranted,
            vec![grant(vec![HostAccessOrigin::Direct])],
        );
        let principal = operator(&state);
        let (signer, closure) = (signer(), "b".repeat(64));
        let mut scheduled = caller(&state, &principal, None);
        scheduled.scheduled = true;
        let refusal = refused(admit(
            &state,
            &state.engine,
            &subject(&signer, &closure),
            &scheduled,
        ));
        assert_eq!(refusal.code, HostAccessRefusal::ORIGIN_NOT_GRANTED);
        assert!(refusal.message.contains("schedule"), "{refusal}");

        let node_key = state.identity.fingerprint().to_owned();
        let refusal = refused(admit(
            &state,
            &state.engine,
            &subject(&node_key, &closure),
            &caller(&state, &principal, None),
        ));
        assert_eq!(refusal.code, HostAccessRefusal::SIGNER_REFUSED);

        let unsigned = HostAccessSubject {
            item: ITEM,
            signer: None,
            source_closure: Some(&closure),
            runtime: None,
        };
        let refusal = refused(admit(
            &state,
            &state.engine,
            &unsigned,
            &caller(&state, &principal, None),
        ));
        assert_eq!(refusal.code, HostAccessRefusal::SIGNER_REFUSED);
    }

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
