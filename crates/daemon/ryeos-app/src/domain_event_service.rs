//! Daemon-enforced runtime API for bundle/domain events.

use std::sync::Arc;

use anyhow::Context;
use ryeos_runtime::authorizer::{AuthorizationPolicy, Authorizer};
use ryeos_state::{DomainEventAppendRequest, DomainEventAppendResult, DomainEventRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::callback_token::CallbackCapability;
use crate::state_store::StateStore;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainEventAppendParams {
    pub thread_id: String,
    pub event_kind: String,
    pub chain_id: String,
    pub event_type: String,
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub expected_chain_head_hash: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub causation_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainEventReadChainParams {
    pub thread_id: String,
    pub event_kind: String,
    pub chain_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainEventScanParams {
    pub thread_id: String,
    pub event_kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DomainEventAppendResponse {
    pub event_hash: String,
    pub chain_head_hash: String,
    pub event: ryeos_state::DomainEventObject,
    pub idempotent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DomainEventRecordsResponse {
    pub events: Vec<DomainEventRecord>,
}

pub struct DomainEventService;

impl DomainEventService {
    pub fn append(
        state_store: &Arc<StateStore>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: DomainEventAppendParams,
    ) -> anyhow::Result<DomainEventAppendResponse> {
        let effective_bundle_id = effective_bundle_id(cap)?;
        validate_domain_identifiers(&effective_bundle_id, &params.event_kind)?;
        authorize_domain_event(
            authorizer,
            &cap.effective_caps,
            "append",
            &effective_bundle_id,
            &params.event_kind,
        )?;
        let result = state_store.append_domain_event(DomainEventAppendRequest {
            effective_bundle_id,
            bundle_id: None,
            event_kind: params.event_kind,
            chain_id: params.chain_id,
            event_type: params.event_type,
            schema_version: params.schema_version,
            payload: params.payload,
            expected_chain_head_hash: params.expected_chain_head_hash,
            idempotency_key: params.idempotency_key,
            correlation_id: params.correlation_id,
            causation_id: params.causation_id,
            attribution: attribution_for_callback(cap),
        })?;
        Ok(result.into())
    }

    pub fn read_chain(
        state_store: &Arc<StateStore>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: DomainEventReadChainParams,
    ) -> anyhow::Result<DomainEventRecordsResponse> {
        let effective_bundle_id = effective_bundle_id(cap)?;
        validate_domain_identifiers(&effective_bundle_id, &params.event_kind)?;
        authorize_domain_event(
            authorizer,
            &cap.effective_caps,
            "scan",
            &effective_bundle_id,
            &params.event_kind,
        )?;
        Ok(DomainEventRecordsResponse {
            events: state_store.read_domain_event_chain(
                &effective_bundle_id,
                &params.event_kind,
                &params.chain_id,
            )?,
        })
    }

    pub fn scan(
        state_store: &Arc<StateStore>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: DomainEventScanParams,
    ) -> anyhow::Result<DomainEventRecordsResponse> {
        let effective_bundle_id = effective_bundle_id(cap)?;
        validate_domain_identifiers(&effective_bundle_id, &params.event_kind)?;
        authorize_domain_event(
            authorizer,
            &cap.effective_caps,
            "scan",
            &effective_bundle_id,
            &params.event_kind,
        )?;
        Ok(DomainEventRecordsResponse {
            events: state_store.scan_domain_events(&effective_bundle_id, &params.event_kind)?,
        })
    }
}

impl From<DomainEventAppendResult> for DomainEventAppendResponse {
    fn from(result: DomainEventAppendResult) -> Self {
        Self {
            event_hash: result.event_hash,
            chain_head_hash: result.chain_head_hash,
            event: result.event,
            idempotent: result.idempotent,
        }
    }
}

fn default_schema_version() -> u32 {
    1
}

fn effective_bundle_id(cap: &CallbackCapability) -> anyhow::Result<String> {
    cap.effective_bundle_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("callback token has no effective_bundle_id"))
}

fn validate_domain_identifiers(bundle_id: &str, event_kind: &str) -> anyhow::Result<()> {
    ryeos_state::objects::validate_domain_identifier("bundle_id", bundle_id)?;
    ryeos_state::objects::validate_domain_identifier("event_kind", event_kind)?;
    Ok(())
}

fn authorize_domain_event(
    authorizer: &Authorizer,
    effective_caps: &[String],
    verb: &str,
    bundle_id: &str,
    event_kind: &str,
) -> anyhow::Result<()> {
    let required = format!("ryeos.{verb}.domain_events.{bundle_id}/{event_kind}");
    authorizer
        .authorize(effective_caps, &AuthorizationPolicy::require(&required))
        .with_context(|| format!("missing required capability: {required}"))
}

fn attribution_for_callback(cap: &CallbackCapability) -> ryeos_state::DomainEventAttribution {
    ryeos_state::DomainEventAttribution {
        actor: None,
        tool: cap.item_ref.clone(),
        executor: None,
        site: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use crate::execution_provenance::ExecutionProvenance;

    fn cap(effective_caps: Vec<String>, effective_bundle_id: Option<&str>) -> CallbackCapability {
        let engine = Arc::new(ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            None,
            vec![],
        ));
        CallbackCapability {
            token: "cbt-test".into(),
            invocation_id: "inv-test".into(),
            thread_id: "T-test".into(),
            project_path: PathBuf::from("/tmp/test"),
            expires_at: Instant::now() + Duration::from_secs(60),
            effective_caps,
            provenance: ExecutionProvenance::root_live_fs(PathBuf::from("/tmp/test"), engine),
            effective_bundle_id: effective_bundle_id.map(str::to_string),
            item_ref: Some("tool:ryeos-email/send".into()),
        }
    }

    #[test]
    fn authorizes_domain_event_capability() {
        let authorizer = Authorizer::new();
        let cap = cap(
            vec!["ryeos.append.domain_events.ryeos-email/email_event".into()],
            Some("ryeos-email"),
        );
        authorize_domain_event(
            &authorizer,
            &cap.effective_caps,
            "append",
            "ryeos-email",
            "email_event",
        )
        .unwrap();
    }

    #[test]
    fn validates_identifiers_before_capability_check() {
        let cap = cap(
            vec!["ryeos.scan.domain_events.*".into()],
            Some("ryeos-email"),
        );
        let bundle_id = effective_bundle_id(&cap).unwrap();
        let err = validate_domain_identifiers(&bundle_id, "../bad").unwrap_err();
        assert!(format!("{err:#}").contains("unsafe character"));
    }
}
