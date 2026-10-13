//! Node-owned aggregate resource policy for persistent subprocess sessions.

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

/// Node-wide admission budgets for trusted, bundle-signed persistent workers.
/// Signed kind contracts can narrow them but cannot raise them. The per-tree
/// RLIMITs applied after admission are not aggregate hostile-workload
/// enforcement; that stronger boundary requires a cgroup-backed backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionPoolLimits {
    pub max_pool_groups: usize,
    pub max_total_processes: usize,
    pub max_total_address_space_bytes: u64,
    pub max_total_cpu_seconds: u64,
    /// Maximum per-worker RLIMIT_NPROC that a signed workload contract may
    /// request for the worker's real UID.
    pub max_real_uid_process_limit: u64,
    pub max_open_streams: usize,
    pub max_active_streams: usize,
    pub max_active_streams_per_subject: usize,
    pub max_stream_backlog_bytes: usize,
    pub max_total_backlog_bytes: usize,
}

impl Default for PersistentSessionPoolLimits {
    fn default() -> Self {
        Self {
            max_pool_groups: 128,
            max_total_processes: 8,
            max_total_address_space_bytes: 32 * 1024 * 1024 * 1024,
            max_total_cpu_seconds: 8 * 60 * 60,
            max_real_uid_process_limit: 4096,
            max_open_streams: 256,
            max_active_streams: 32,
            max_active_streams_per_subject: 4,
            max_stream_backlog_bytes: 16 * 1024 * 1024,
            max_total_backlog_bytes: 64 * 1024 * 1024,
        }
    }
}

impl PersistentSessionPoolLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_pool_groups == 0
            || self.max_pool_groups > 4096
            || self.max_total_processes == 0
            || self.max_total_processes > 256
            || self.max_total_address_space_bytes == 0
            || self.max_total_address_space_bytes > 4 * 1024 * 1024 * 1024 * 1024
            || self.max_total_cpu_seconds == 0
            || self.max_total_cpu_seconds > 365 * 24 * 60 * 60
            || self.max_real_uid_process_limit == 0
            || self.max_real_uid_process_limit > 4096
            || self.max_open_streams == 0
            || self.max_open_streams > 4096
            || self.max_active_streams == 0
            || self.max_active_streams > self.max_open_streams
            || self.max_active_streams_per_subject == 0
            || self.max_active_streams_per_subject > self.max_active_streams
            || self.max_stream_backlog_bytes == 0
            || self.max_stream_backlog_bytes > 64 * 1024 * 1024
            || self.max_total_backlog_bytes < self.max_stream_backlog_bytes
            || self.max_total_backlog_bytes > 1024 * 1024 * 1024
        {
            bail!("persistent-session node pool limits are incoherent");
        }
        Ok(())
    }
}

pub const SECTION_NAME: &str = "persistent_sessions";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionPolicy {
    pub schema: u32,
    pub enabled: bool,
    pub limits: Option<PersistentSessionPoolLimits>,
}

impl PersistentSessionPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("persistent-session node policy schema is not current");
        }
        match (self.enabled, self.limits.as_ref()) {
            (true, Some(limits)) => limits.validate(),
            (false, None) => Ok(()),
            (true, None) => bail!("enabled persistent sessions require exact node limits"),
            (false, Some(_)) => bail!("disabled persistent sessions must not retain latent limits"),
        }
    }

    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            schema: 1,
            enabled: false,
            limits: None,
        }
    }
}

pub struct PersistentSessionPolicySection;

impl TypedNodePolicy for PersistentSessionPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

impl NodePolicySection for PersistentSessionPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: PersistentSessionPolicy =
            serde_json::from_value(body.clone()).context("parse persistent-session node policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_value() -> Value {
        serde_json::to_value(PersistentSessionPolicy {
            schema: 1,
            enabled: true,
            limits: Some(PersistentSessionPoolLimits::default()),
        })
        .unwrap()
    }

    #[test]
    fn policy_is_node_owned_and_parses_exact_limits() {
        let section = PersistentSessionPolicySection;
        let context = NodePolicyContext {
            section: "persistent_sessions".to_owned(),
            source_file: "/node/policies/persistent_sessions.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        };
        let parsed = section.parse(&context, &policy_value()).unwrap();
        let record = parsed
            .as_any()
            .downcast_ref::<PersistentSessionPolicy>()
            .unwrap();
        assert!(record.enabled);
        assert_eq!(record.limits, Some(PersistentSessionPoolLimits::default()));
    }

    #[test]
    fn policy_rejects_unknown_schema_and_incoherent_limits() {
        let mut unknown_schema = policy_value();
        unknown_schema["schema"] = Value::from(2);
        let record: PersistentSessionPolicy = serde_json::from_value(unknown_schema).unwrap();
        assert!(record.validate().is_err());

        let mut incoherent = policy_value();
        incoherent["limits"]["max_total_processes"] = Value::from(0);
        let record: PersistentSessionPolicy = serde_json::from_value(incoherent).unwrap();
        assert!(record.validate().is_err());

        assert!(PersistentSessionPolicy::disabled().validate().is_ok());
    }
}
use std::sync::Arc;
