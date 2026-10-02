//! Invocation identity for the existing qualification-only Worker owner.
//! Constructing this value grants no authority. Every preparation/reservation
//! and first contact authenticates its actual operator or callback lane again.

use crate::{handler_context::HandlerContext, state::AppState};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;

#[derive(Clone)]
pub struct QualificationInvocation {
    owner: InvocationOwner,
}

// Deliberately no Debug/Serialize: a callback bearer must not become evidence
// or diagnostic output. The durable journal retains authenticated coordinates.
#[derive(Clone)]
enum InvocationOwner {
    Operator(HandlerContext),
    Callback(String),
}

impl QualificationInvocation {
    pub fn operator(context: &HandlerContext) -> Self {
        Self {
            owner: InvocationOwner::Operator(context.clone()),
        }
    }

    pub fn callback(token: &str) -> Self {
        Self {
            owner: InvocationOwner::Callback(token.to_owned()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn authenticate(
        &self,
        state: &AppState,
        authority: &ryeos_state::PinnedStateAuthority,
        guard: &ryeos_state::CasMutationGuard,
        limits: ryeos_state::object_closure::ObjectClosureLimits,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
        qualification_operation_id: &str,
    ) -> anyhow::Result<
        crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    > {
        use crate::operator_external_content::product_qualification::{
            authenticate_consumer_callback_root, authenticate_consumer_root,
        };
        match &self.owner {
            InvocationOwner::Operator(context) => authenticate_consumer_root(
                state,
                authority,
                guard,
                limits,
                context,
                coordinate,
                qualification_operation_id,
            ),
            InvocationOwner::Callback(token) => authenticate_consumer_callback_root(
                state,
                authority,
                guard,
                limits,
                token,
                "runtime.consumer_verification_start",
                coordinate,
                qualification_operation_id,
            ),
        }
    }
}
