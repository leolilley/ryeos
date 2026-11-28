//! Target-local admission-token minting through retained node authority.
//!
//! This is a normal signed Service, available against a live daemon and via
//! the existing stopped-node service runner. It must not be an isolated Tool:
//! minting writes node-private admission state and consumes the exact hosted
//! policy generation already loaded by the daemon.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::node_policy::sections::hosted::HostedNodePolicy;
use ryeos_node_runtime::node_policy::sections::membership::MembershipPolicy;

pub type Request = ryeos_core_tools::actions::authorize::MintAdmissionTokenRequest;

pub async fn handle(
    request: Request,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ryeos_app::operator_authority::require_local_configured_operator(&state, &context)
        .context("admission-token minting requires the configured local operator")?;
    // A `remote_node` token admits a peer node outside membership, so only the
    // local configured operator acting for itself may mint one: never a
    // member, never a delegated request.
    if request.class == ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::RemoteNode
        && (context.member_class.is_some() || context.delegated_via.is_some())
    {
        return Err(anyhow::Error::new(ryeos_app::handler_error::typed_refusal(
            "remote_node_token_operator_only",
            403,
            "a remote_node admission token is minted only by the local configured operator, never by a member or on a delegated request",
        )));
    }
    let params = request.into_params(state.config.app_root.clone())?;
    let membership = state
        .node_policy
        .require::<MembershipPolicy>()
        .context("load retained membership policy")?
        .clone();
    if !membership.classes.contains_key(&params.membership_class) {
        return Err(anyhow::Error::new(ryeos_app::handler_error::typed_refusal(
            "membership_class_unknown",
            400,
            format!(
                "membership class `{}` is not a class of this node's membership policy; {}",
                params.membership_class,
                if membership.classes.is_empty() {
                    "the policy names no class, so no admission token can be minted".to_owned()
                } else {
                    format!(
                        "it names {}",
                        membership
                            .classes
                            .keys()
                            .map(|id| format!("`{id}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            ),
        )));
    }
    let policy = state
        .node_policy
        .require::<HostedNodePolicy>()
        .context("load retained hosted-node policy")?
        .clone();
    let policy_source = state
        .node_policy
        .source_file::<HostedNodePolicy>()
        .context("load retained hosted-node policy source")?
        .to_path_buf();
    let identity = state.identity.clone();
    tokio::task::spawn_blocking(move || {
        let result = ryeos_core_tools::actions::authorize::mint_admission_token_with_policy(
            params,
            &policy,
            &policy_source,
            &membership,
            &identity,
        )?;
        serde_json::to_value(result).map_err(Into::into)
    })
    .await
    .context("admission-token mint worker stopped")?
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/mint-admission-token",
    endpoint: "identity.mint-admission-token",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.identity/mint-admission-token"],
    handler: |params, context, state| {
        Box::pin(async move {
            let request = crate::handler_error::parse_request(params)?;
            handle(request, context, state).await
        })
    },
};
