//! Pure policy and materialization helpers for subprocess dispatch.

use super::*;

/// Strip the `bin/<triple>/` prefix from a runtime YAML's `binary_ref`.
pub(crate) fn strip_binary_ref_prefix(binary_ref: &str) -> Result<String, DispatchError> {
    let parts: Vec<&str> = binary_ref.split('/').collect();
    if parts.len() < 3 || parts[0] != "bin" || parts[1].is_empty() || parts[2].is_empty() {
        return Err(DispatchError::SchemaMisconfigured {
            kind: ROOT_KIND_RUNTIME.into(),
            detail: format!(
                "runtime binary_ref '{binary_ref}' has unexpected shape; expected 'bin/<triple>/<binary>'"
            ),
        });
    }
    Ok(parts[2..].join("/"))
}

/// Reject a bare terminal executor launched as a root tool.
pub(super) fn require_terminal_executor_id(
    verified: Option<&VerifiedItem>,
    item_ref: &str,
) -> Result<(), DispatchError> {
    if verified.is_some_and(|item| item.resolved.metadata.executor_id.is_none()) {
        return Err(DispatchError::RootExecutorMissing {
            item_ref: item_ref.to_string(),
            detail: "items with no executor_id, including terminal executors such as `tool:ryeos/core/subprocess/execute`, cannot be launched as root tools. Create a wrapper tool with `executor_id: \"@subprocess\"` and a `config:` block, then execute the wrapper."
                .into(),
        });
    }
    Ok(())
}

/// Enforce runtime-declared capabilities against caller scopes.
pub(super) fn enforce_runtime_caps(
    authorizer: &ryeos_runtime::authorizer::Authorizer,
    item_ref: &str,
    required_caps: &[String],
    caller_scopes: &[String],
) -> Result<(), DispatchError> {
    if required_caps.is_empty() {
        return Ok(());
    }
    let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require_all(
        &required_caps.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    );
    authorizer
        .authorize(caller_scopes, &policy)
        .map_err(|_| DispatchError::InsufficientCaps {
            runtime: item_ref.to_string(),
            required: required_caps.to_vec(),
            caller_scopes: caller_scopes.to_vec(),
        })
}

/// Pure resolution output for a managed subprocess launch.
pub struct PreparedManagedLaunch {
    pub resolved: ResolvedExecutionRequest,
    pub executor_ref: String,
    pub provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    pub acting_principal: String,
    pub project_path: PathBuf,
}

pub(super) fn prepare_managed_launch(
    verified_runtime: &VerifiedRuntime,
    root_subject: Option<RootSubject>,
    hop_thread_profile: &str,
    hop_verified: Option<&VerifiedItem>,
    runtime_ref: &str,
    ctx: &ExecutionContext,
    request: &DispatchRequest<'_>,
) -> Result<PreparedManagedLaunch, DispatchError> {
    let bare = strip_binary_ref_prefix(&verified_runtime.yaml.binary_ref)?;
    let executor_ref = format!("native:{bare}");
    let subject = root_subject.unwrap_or_else(|| RootSubject {
        item_ref: runtime_ref.to_string(),
        thread_profile: hop_thread_profile.to_string(),
        verified: hop_verified.cloned(),
    });
    let resolved_item = match subject.verified {
        Some(v) => v.resolved,
        None => {
            let canonical = CanonicalRef::parse(&subject.item_ref)
                .map_err(|e| DispatchError::InvalidRef(subject.item_ref.clone(), e.to_string()))?;
            ctx.engine.resolve(&ctx.plan_ctx, &canonical).map_err(|e| {
                DispatchError::SchemaMisconfigured {
                    kind: canonical.kind.clone(),
                    detail: format!("subject resolution failed for '{}': {e}", subject.item_ref),
                }
            })?
        }
    };
    let resolved = ResolvedExecutionRequest {
        kind: subject.thread_profile.clone(),
        item_ref: subject.item_ref.clone(),
        executor_ref: executor_ref.clone(),
        launch_mode: "inline".to_string(),
        current_site_id: ctx.plan_ctx.current_site_id.clone(),
        origin_site_id: ctx.plan_ctx.origin_site_id.clone(),
        target_site_id: None,
        requested_by: Some(request.acting_principal.to_string()),
        usage_subject: request.usage_subject.clone(),
        usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
        parameters: request.params.clone(),
        resolved_item,
        plan_context: ctx.plan_ctx.clone(),
    };
    Ok(PreparedManagedLaunch {
        resolved,
        executor_ref,
        provenance: request.provenance.clone(),
        acting_principal: request.acting_principal.to_string(),
        project_path: request.project_path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_ref_materialization_preserves_nested_binary_path() {
        assert_eq!(
            strip_binary_ref_prefix("bin/x86_64-unknown-linux-gnu/tools/runtime").unwrap(),
            "tools/runtime"
        );
        assert!(strip_binary_ref_prefix("runtime").is_err());
    }

    #[test]
    fn runtime_cap_policy_allows_wildcard_and_rejects_missing_scope() {
        let auth = ryeos_runtime::authorizer::Authorizer::new();
        let required = vec!["runtime.execute".to_string()];
        assert!(
            enforce_runtime_caps(&auth, "runtime:test", &required, &["runtime.*".to_string()])
                .is_ok()
        );
        assert!(enforce_runtime_caps(&auth, "runtime:test", &required, &[]).is_err());
    }
}
