//! Daemon-enforced runtime callbacks for scoped access to the RyeOS vault.

use std::sync::Arc;

use anyhow::Context;
use ryeos_bundle::manifest::RuntimeVaultOperation;
use ryeos_bundle::runtime_authority::runtime_vault_cap;
use ryeos_runtime::authorizer::{AuthorizationPolicy, Authorizer};
use serde::{Deserialize, Serialize};

use crate::callback_token::CallbackCapability;
use crate::vault::{
    runtime_vault_ref, validate_runtime_vault_segment, validate_secret_value, NodeVault, VaultScope,
};

const VAULT_BUNDLE_REF_PREFIX: &str = "vault://bundle/";
const DEFAULT_RUNTIME_VAULT_LIST_LIMIT: usize = 64;
const MAX_RUNTIME_VAULT_LIST_LIMIT: usize = 128;
const MAX_RUNTIME_VAULT_LIST_RESPONSE_BYTES: usize = 64 * 1024;

fn default_runtime_vault_list_limit() -> usize {
    DEFAULT_RUNTIME_VAULT_LIST_LIMIT
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeVaultPutParams {
    pub thread_id: String,
    pub namespace: String,
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeVaultRefParams {
    pub thread_id: String,
    pub r#ref: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeVaultListParams {
    pub thread_id: String,
    pub namespace: String,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_runtime_vault_list_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeVaultPutResponse {
    pub r#ref: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeVaultGetResponse {
    pub r#ref: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeVaultDeleteResponse {
    pub r#ref: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeVaultListResponse {
    pub namespace: String,
    pub keys: Vec<String>,
    pub next_cursor: Option<String>,
}

pub struct RuntimeVaultService;

impl RuntimeVaultService {
    pub fn put(
        vault: &Arc<dyn NodeVault>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: RuntimeVaultPutParams,
    ) -> anyhow::Result<RuntimeVaultPutResponse> {
        let bundle_id = effective_bundle_id(cap)?;
        validate_runtime_vault_segment("namespace", &params.namespace)?;
        validate_runtime_vault_segment("key", &params.key)?;
        authorize_runtime_vault(
            authorizer,
            &cap.effective_caps,
            &RuntimeVaultOperation::Put,
            &bundle_id,
            &params.namespace,
        )?;
        validate_secret_value(&params.value)?;
        let scope = VaultScope::runtime_bundle(&bundle_id, &params.namespace)?;
        vault.put_scoped_secret(&scope, &params.key, &params.value)?;
        Ok(RuntimeVaultPutResponse {
            r#ref: runtime_vault_ref(&bundle_id, &params.namespace, &params.key),
        })
    }

    pub fn get(
        vault: &Arc<dyn NodeVault>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: RuntimeVaultRefParams,
    ) -> anyhow::Result<RuntimeVaultGetResponse> {
        let bundle_id = effective_bundle_id(cap)?;
        let parsed = parse_ref_for_bundle(&params.r#ref, &bundle_id)?;
        authorize_runtime_vault(
            authorizer,
            &cap.effective_caps,
            &RuntimeVaultOperation::Get,
            &bundle_id,
            &parsed.namespace,
        )?;
        let scope = VaultScope::runtime_bundle(&bundle_id, &parsed.namespace)?;
        let value = vault
            .get_scoped_secret(&scope, &parsed.key)?
            .ok_or_else(|| anyhow::anyhow!("runtime vault secret not found"))?;
        validate_secret_value(&value)?;
        Ok(RuntimeVaultGetResponse {
            r#ref: params.r#ref,
            value,
        })
    }

    pub fn delete(
        vault: &Arc<dyn NodeVault>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: RuntimeVaultRefParams,
    ) -> anyhow::Result<RuntimeVaultDeleteResponse> {
        let bundle_id = effective_bundle_id(cap)?;
        let parsed = parse_ref_for_bundle(&params.r#ref, &bundle_id)?;
        authorize_runtime_vault(
            authorizer,
            &cap.effective_caps,
            &RuntimeVaultOperation::Delete,
            &bundle_id,
            &parsed.namespace,
        )?;
        let scope = VaultScope::runtime_bundle(&bundle_id, &parsed.namespace)?;
        let deleted = vault.delete_scoped_secret(&scope, &parsed.key)?;
        Ok(RuntimeVaultDeleteResponse {
            r#ref: params.r#ref,
            deleted,
        })
    }

    pub fn list(
        vault: &Arc<dyn NodeVault>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: RuntimeVaultListParams,
    ) -> anyhow::Result<RuntimeVaultListResponse> {
        let bundle_id = effective_bundle_id(cap)?;
        validate_runtime_vault_segment("namespace", &params.namespace)?;
        if let Some(cursor) = params.cursor.as_deref() {
            validate_runtime_vault_segment("cursor", cursor)?;
        }
        if params.limit == 0 || params.limit > MAX_RUNTIME_VAULT_LIST_LIMIT {
            anyhow::bail!(
                "runtime vault list limit must be between 1 and {MAX_RUNTIME_VAULT_LIST_LIMIT}"
            );
        }
        authorize_runtime_vault(
            authorizer,
            &cap.effective_caps,
            &RuntimeVaultOperation::List,
            &bundle_id,
            &params.namespace,
        )?;
        let scope = VaultScope::runtime_bundle(&bundle_id, &params.namespace)?;
        let mut scoped_keys = vault.list_scoped_secret_keys(&scope)?;
        for key in &scoped_keys {
            validate_runtime_vault_segment("persisted key", key)?;
        }
        scoped_keys.sort();
        scoped_keys.dedup();
        let mut keys = scoped_keys
            .into_iter()
            .filter(|key| params.cursor.as_ref().is_none_or(|cursor| key > cursor))
            .take(params.limit + 1)
            .collect::<Vec<_>>();
        let has_more = keys.len() > params.limit;
        keys.truncate(params.limit);
        let next_cursor = has_more.then(|| keys.last().cloned()).flatten();
        let response_bytes = serde_json::to_vec(&RuntimeVaultListResponse {
            namespace: params.namespace.clone(),
            keys: keys.clone(),
            next_cursor: next_cursor.clone(),
        })?
        .len();
        if response_bytes > MAX_RUNTIME_VAULT_LIST_RESPONSE_BYTES {
            anyhow::bail!(
                "runtime vault list response is {response_bytes} bytes; maximum is {MAX_RUNTIME_VAULT_LIST_RESPONSE_BYTES}"
            );
        }
        Ok(RuntimeVaultListResponse {
            namespace: params.namespace,
            keys,
            next_cursor,
        })
    }
}

#[derive(Debug)]
struct ParsedRuntimeVaultRef {
    namespace: String,
    key: String,
}

#[cfg(test)]
mod parameter_tests {
    use super::*;

    #[test]
    fn list_params_default_and_explicit_pagination_decode() {
        let defaults: RuntimeVaultListParams = serde_json::from_value(serde_json::json!({
            "thread_id": "thread-1",
            "namespace": "weights"
        }))
        .unwrap();
        assert_eq!(defaults.limit, DEFAULT_RUNTIME_VAULT_LIST_LIMIT);
        assert_eq!(defaults.cursor, None);

        let paged: RuntimeVaultListParams = serde_json::from_value(serde_json::json!({
            "thread_id": "thread-1",
            "namespace": "weights",
            "cursor": "checkpoint_1",
            "limit": MAX_RUNTIME_VAULT_LIST_LIMIT
        }))
        .unwrap();
        assert_eq!(paged.cursor.as_deref(), Some("checkpoint_1"));
        assert_eq!(paged.limit, MAX_RUNTIME_VAULT_LIST_LIMIT);
    }
}

fn parse_ref_for_bundle(
    value: &str,
    expected_bundle_id: &str,
) -> anyhow::Result<ParsedRuntimeVaultRef> {
    let rest = value
        .strip_prefix(VAULT_BUNDLE_REF_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("runtime vault ref must start with vault://bundle/"))?;
    let parts = rest.split('/').collect::<Vec<_>>();
    if parts.len() != 3 {
        anyhow::bail!("runtime vault ref must be vault://bundle/<bundle>/<namespace>/<key>");
    }
    let bundle_id = parts[0];
    let namespace = parts[1];
    let key = parts[2];
    if bundle_id != expected_bundle_id {
        anyhow::bail!("runtime vault ref bundle does not match callback bundle identity");
    }
    ryeos_state::objects::validate_bundle_identifier("bundle_id", bundle_id)?;
    validate_runtime_vault_segment("namespace", namespace)?;
    validate_runtime_vault_segment("key", key)?;
    Ok(ParsedRuntimeVaultRef {
        namespace: namespace.to_string(),
        key: key.to_string(),
    })
}

fn effective_bundle_id(cap: &CallbackCapability) -> anyhow::Result<String> {
    cap.effective_bundle_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("callback token has no effective_bundle_id"))
}

fn authorize_runtime_vault(
    authorizer: &Authorizer,
    effective_caps: &[String],
    op: &RuntimeVaultOperation,
    bundle_id: &str,
    namespace: &str,
) -> anyhow::Result<()> {
    let required = runtime_vault_cap(op, bundle_id, namespace);
    authorizer
        .authorize(effective_caps, &AuthorizationPolicy::require(&required))
        .with_context(|| {
            format!(
                "missing required capability: {required} — runtime-vault access is runtime \
                 authority: declare `runtime_authority.runtime_vault:` for namespace \
                 '{namespace}' in this bundle's `.ai/manifest.source.yaml` and sign it \
                 (`ryeos bundle publish`), then request it from the item under \
                 `requires.capabilities.manifest.runtime_authority`. It cannot be self-granted \
                 under `requires.capabilities.declared`."
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ref_for_expected_bundle() {
        let parsed = parse_ref_for_bundle(
            "vault://bundle/agent-kiwi/oauth/google_account_123",
            "agent-kiwi",
        )
        .unwrap();
        assert_eq!(parsed.namespace, "oauth");
        assert_eq!(parsed.key, "google_account_123");
    }

    #[test]
    fn rejects_other_bundle_ref() {
        let err = parse_ref_for_bundle(
            "vault://bundle/other-bundle/oauth/google_account_123",
            "agent-kiwi",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("does not match"));
    }
}
