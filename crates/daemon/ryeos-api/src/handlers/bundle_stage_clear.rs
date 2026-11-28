//! `bundle.stage-clear` — remove staged bundles from the node's staging area.
//!
//! A stage is inert bytes, so clearing one is safe while the node runs. Only
//! the node's admitted operator may clear: the capability names the action,
//! but the key that requested a stage must never be able to delete another
//! key's stage, nor free its own share of the caps.
//!
//! Stages are chosen by digest, by a refused apply, or all of them. There is
//! no age selector. The clear is not an offline operation and writes no
//! ledger receipt: it answers with what was removed and logs one line per
//! removed stage naming the operator.

use std::sync::Arc;

use anyhow::Result;
use ryeos_node::bundle_staging::StagingArea;
use ryeos_node::bundle_staging::clear::{ClearSelector, Cleared};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

/// Exactly one selector: `digests`, `apply_refused` or `all`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    /// The stages to remove, by tree digest. One digest from the command line
    /// arrives as a single string.
    #[serde(
        default,
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    pub digests: Vec<String>,
    /// Remove every stage whose latest signed apply was refused.
    pub apply_refused: bool,
    /// Remove every stage and every stale remnant of the area.
    pub all: bool,
}

impl Request {
    fn selector(self) -> Result<ClearSelector, HandlerError> {
        let chosen = usize::from(!self.digests.is_empty())
            + usize::from(self.apply_refused)
            + usize::from(self.all);
        if chosen != 1 {
            return Err(HandlerError::BadRequest(
                "name exactly one of `digests`, `apply_refused` or `all`".to_owned(),
            ));
        }
        Ok(if self.all {
            ClearSelector::All
        } else if self.apply_refused {
            ClearSelector::ApplyRefused
        } else {
            ClearSelector::Digests(self.digests)
        })
    }
}

fn into_error(refusal: &ryeos_node::bundle_staging::StagingRefusal) -> anyhow::Error {
    anyhow::Error::new(crate::handlers::remote_bundle_install::stage_refusal_error(
        refusal,
    ))
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let operator = ryeos_app::operator_authority::require_admitted_operator(&state, &ctx)?;
    let selector = req.selector()?;

    let app_root = state.config.app_root.clone();
    let node_key = state.identity.signing_key().verifying_key();
    let cleared = {
        let selector = selector.clone();
        tokio::task::spawn_blocking(move || -> Result<Cleared> {
            match StagingArea::open_if_present(&app_root).map_err(|e| into_error(&e))? {
                Some(area) => area
                    .remove_matching(&selector, &node_key)
                    .map_err(|e| into_error(&e)),
                // A node that never staged has no area: there is nothing to
                // remove, and naming a stage it does not hold is not found.
                None => match &selector {
                    ClearSelector::Digests(digests) => {
                        let first = digests.first().cloned().unwrap_or_default();
                        Err(into_error(
                            &ryeos_node::bundle_staging::StagingRefusal::NotFound(first),
                        ))
                    }
                    ClearSelector::ApplyRefused | ClearSelector::All => Ok(Cleared::default()),
                },
            }
        })
        .await??
    };

    for stage in &cleared.removed {
        tracing::info!(
            operator = %operator,
            tree_digest = %stage.tree_digest,
            bundle = %stage.name,
            requested_by = %stage.requested_by,
            bytes = stage.bytes,
            "cleared a staged bundle"
        );
    }
    if cleared.remnants > 0 {
        tracing::info!(
            operator = %operator,
            remnants = cleared.remnants,
            "cleared remnants of the bundle staging area"
        );
    }
    Ok(serde_json::to_value(&cleared)?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/stage-clear",
    endpoint: "bundle.stage-clear",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle/stage-clear"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_selector_is_required() {
        assert!(Request::default().selector().is_err());
        assert!(
            Request {
                all: true,
                apply_refused: true,
                ..Request::default()
            }
            .selector()
            .is_err()
        );
        assert_eq!(
            Request {
                all: true,
                ..Request::default()
            }
            .selector()
            .unwrap(),
            ClearSelector::All
        );
        assert_eq!(
            Request {
                apply_refused: true,
                ..Request::default()
            }
            .selector()
            .unwrap(),
            ClearSelector::ApplyRefused
        );
        assert_eq!(
            Request {
                digests: vec!["a".repeat(64)],
                ..Request::default()
            }
            .selector()
            .unwrap(),
            ClearSelector::Digests(vec!["a".repeat(64)])
        );
    }
}
