//! Runtime-to-launch-preparer bindings and the dedicated launch-preparer
//! subprocess boundary.
//!
//! Launch preparers receive verified execution identities and may return
//! symbolic secret requirements, so they do not share the general
//! parser/composer runner. Enforced node policy uses the selected signed
//! isolation adapter. Disabled policy executes the hash-pinned handler directly.
//! Both paths retain the same
//! strict protocol, resource, timeout, and bounded-I/O controls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ryeos_handler_protocol::{
    HandlerRequest, HandlerResponse, LaunchPrepareRequest, LaunchPrepareResponse,
};

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;
use crate::handlers::{HandlerRegistry, HandlerServes, VerifiedHandler};
use crate::isolation::{
    IsolationLaunchContext, IsolationProjectAuthority, IsolationRuntime, IsolationVerifiedCode,
};
use crate::resolution::TrustClass;
use crate::runtime_registry::{LaunchPreparationDecl, RuntimeRegistry};

pub const LAUNCH_PREPARER_REQUEST_MAX_BYTES: usize = 10 * 1024 * 1024;
pub const LAUNCH_PREPARER_STDOUT_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const LAUNCH_PREPARER_STDERR_MAX_BYTES: usize = 64 * 1024;
pub const LAUNCH_PREPARER_TIMEOUT: Duration = Duration::from_secs(5);
const LAUNCH_PREPARER_OPEN_FILE_LIMIT: u64 = 64;

#[derive(Debug, Clone)]
struct BoundLaunchPreparer {
    handler: Arc<VerifiedHandler>,
    handler_config: serde_json::Value,
}

/// Runtime-ref keyed registry built once after the runtime and handler
/// registries have both passed boot validation.
#[derive(Debug, Clone, Default)]
pub struct LaunchPreparerRegistry {
    by_runtime: HashMap<CanonicalRef, BoundLaunchPreparer>,
    runner: Option<LaunchPreparerRunner>,
}

impl LaunchPreparerRegistry {
    pub fn from_runtimes(
        runtimes: &RuntimeRegistry,
        handlers: &HandlerRegistry,
        runner: LaunchPreparerRunner,
    ) -> Result<Self, EngineError> {
        let mut by_runtime = HashMap::new();
        let mut sorted_runtimes: Vec<_> = runtimes.all().collect();
        sorted_runtimes.sort_by_key(|runtime| runtime.canonical_ref.to_string());

        for runtime in sorted_runtimes {
            let (handler_ref, handler_config) = match &runtime.yaml.launch_contract.preparation {
                LaunchPreparationDecl::None => continue,
                LaunchPreparationDecl::Handler { handler, config } => (handler, config),
            };
            let handler =
                handlers
                    .get(handler_ref)
                    .ok_or_else(|| EngineError::SchemaLoaderError {
                        reason: format!(
                            "runtime `{}` launch preparer `{handler_ref}` is not registered",
                            runtime.canonical_ref
                        ),
                    })?;
            if handler.descriptor().serves != HandlerServes::LaunchPreparer {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "runtime `{}` handler `{handler_ref}` does not serve `launch_preparer`",
                        runtime.canonical_ref
                    ),
                });
            }
            if handler.trust_class() != TrustClass::TrustedBundle {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "runtime `{}` launch preparer `{handler_ref}` is not trusted_bundle",
                        runtime.canonical_ref
                    ),
                });
            }
            if let VerifiedHandler::Unresolved { reason, .. } = handler {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "runtime `{}` launch preparer `{handler_ref}` is unresolved: {reason}",
                        runtime.canonical_ref
                    ),
                });
            }

            by_runtime.insert(
                runtime.canonical_ref.clone(),
                BoundLaunchPreparer {
                    handler: Arc::new(handler.clone()),
                    handler_config: handler_config.clone(),
                },
            );
        }

        Ok(Self {
            by_runtime,
            runner: Some(runner),
        })
    }

    pub fn contains(&self, runtime_ref: &CanonicalRef) -> bool {
        self.by_runtime.contains_key(runtime_ref)
    }

    pub fn len(&self) -> usize {
        self.by_runtime.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_runtime.is_empty()
    }

    pub fn handler_ref_for(&self, runtime_ref: &CanonicalRef) -> Option<&str> {
        self.by_runtime
            .get(runtime_ref)
            .map(|bound| bound.handler.canonical_ref())
    }

    /// Invoke the handler bound to `runtime_ref`. The registry overwrites the
    /// request's config with the signed runtime descriptor's config so caller
    /// data can never replace launch-preparer policy.
    pub fn prepare(
        &self,
        runtime_ref: &CanonicalRef,
        mut request: LaunchPrepareRequest,
    ) -> Result<LaunchPrepareResponse, EngineError> {
        let bound = self.by_runtime.get(runtime_ref).ok_or_else(|| {
            EngineError::Internal(format!(
                "no launch preparer bound for runtime `{runtime_ref}`"
            ))
        })?;
        let runner = self.runner.as_ref().ok_or_else(|| {
            EngineError::Internal("launch-preparer registry has no isolation runner".to_owned())
        })?;
        request.handler_config = bound.handler_config.clone();
        let response = runner.run_launch_preparer_subprocess(
            &bound.handler,
            &HandlerRequest::LaunchPrepare(request),
        )?;
        match response {
            HandlerResponse::LaunchPrepare { response } => Ok(response),
            other => Err(EngineError::LaunchPreparerProtocolInvalid {
                handler: bound.handler.canonical_ref().to_owned(),
                detail: format!("unexpected launch-preparer response: {other:?}"),
            }),
        }
    }
}

/// Policy-selected process runner shared by boot-time config validation and
/// live launch preparation.
#[derive(Debug, Clone)]
pub struct LaunchPreparerRunner {
    isolation: Arc<IsolationRuntime>,
    bundle_roots: Arc<[PathBuf]>,
}

impl LaunchPreparerRunner {
    /// Select execution from the daemon's immutable node-isolation snapshot.
    /// Disabled policy does not require a backend bundle.
    pub fn from_isolation_runtime(
        isolation: Arc<IsolationRuntime>,
        bundle_roots: &[PathBuf],
    ) -> Result<Self, EngineError> {
        Ok(Self {
            isolation,
            bundle_roots: bundle_roots.to_vec().into(),
        })
    }

    pub(crate) fn run_launch_preparer_subprocess(
        &self,
        handler: &VerifiedHandler,
        request: &HandlerRequest,
    ) -> Result<HandlerResponse, EngineError> {
        let (canonical_ref, binary_path, binary_hash) = match handler {
            VerifiedHandler::Resolved {
                canonical_ref,
                resolved_binary_path,
                resolved_binary_hash,
                ..
            } => (
                canonical_ref.clone(),
                resolved_binary_path.clone(),
                resolved_binary_hash.clone(),
            ),
            VerifiedHandler::Unresolved {
                canonical_ref,
                reason,
                ..
            } => {
                return Err(EngineError::LaunchPreparerUnavailable {
                    handler: canonical_ref.clone(),
                    detail: format!("handler binary is unresolved: {reason}"),
                });
            }
        };

        let request_value = serde_json::to_value(request).map_err(|error| {
            EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: format!("encode handler request: {error}"),
            }
        })?;
        let request_json = lillux::canonical_json(&request_value)
            .map(String::into_bytes)
            .map_err(|error| EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: format!("canonicalize handler request: {error}"),
            })?;
        if request_json.len() > LAUNCH_PREPARER_REQUEST_MAX_BYTES {
            return Err(EngineError::LaunchPreparerLimitExceeded {
                handler: canonical_ref,
                detail: format!(
                    "launch-preparer request is {} bytes; limit is {}",
                    request_json.len(),
                    LAUNCH_PREPARER_REQUEST_MAX_BYTES
                ),
            });
        }
        ryeos_handler_protocol::from_json_slice_strict::<serde_json::Value>(&request_json)
            .map_err(|error| EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: format!("invalid launch-preparer request JSON: {error}"),
            })?;
        let request_json = String::from_utf8(request_json).map_err(|error| {
            EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: format!("launch-preparer request JSON is not UTF-8: {error}"),
            }
        })?;

        let project_path = binary_path.parent().unwrap_or(Path::new("/"));
        let verified_code = [IsolationVerifiedCode {
            source_path: binary_path.clone(),
            content_hash: binary_hash,
        }];
        let item_ref = canonical_ref.to_string();
        let subprocess = self.isolation.apply(
            lillux::SubprocessRequest {
                cmd: binary_path.to_string_lossy().into_owned(),
                args: Vec::new(),
                cwd: Some(project_path.to_string_lossy().into_owned()),
                envs: Vec::new(),
                stdin_data: Some(request_json),
                timeout: LAUNCH_PREPARER_TIMEOUT.as_secs_f64(),
                limits: Some(lillux::SubprocessLimits {
                    max_open_files: Some(LAUNCH_PREPARER_OPEN_FILE_LIMIT),
                    max_stdout_bytes: Some(LAUNCH_PREPARER_STDOUT_MAX_BYTES as u64),
                    max_stderr_bytes: Some(LAUNCH_PREPARER_STDERR_MAX_BYTES as u64),
                }),
                inherited_fds: Vec::new(),
                supervised_status: None,
            },
            IsolationLaunchContext {
                project_path,
                project_authority: IsolationProjectAuthority::ReadOnly,
                state_root: None,
                checkpoint_dir: None,
                daemon_socket_path: None,
                bundle_roots: &self.bundle_roots,
                node_trusted_keys_dir: None,
                verified_code: &verified_code,
                item_ref: &item_ref,
                thread_id: "launch-preparer",
            },
        )?;
        let result = lillux::run(subprocess);
        if !result.success {
            return Err(EngineError::LaunchPreparerUnavailable {
                handler: canonical_ref.clone(),
                detail: if let Some(refusal) = result.launcher_refusal {
                    format!("isolation adapter refused launch: {refusal}")
                } else {
                    format!("launch preparer failed: {}", result.stderr.trim())
                },
            });
        }

        let response: HandlerResponse = ryeos_handler_protocol::from_json_slice_strict(
            result.stdout.as_bytes(),
        )
        .map_err(|error| EngineError::LaunchPreparerProtocolInvalid {
            handler: canonical_ref.clone(),
            detail: error.to_string(),
        })?;
        let response_value = serde_json::to_value(&response).map_err(|error| {
            EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: format!("encode launch-preparer response: {error}"),
            }
        })?;
        let canonical_response = lillux::canonical_json(&response_value).map_err(|error| {
            EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref,
                detail: format!("canonicalize launch-preparer response: {error}"),
            }
        })?;
        if canonical_response.len() > LAUNCH_PREPARER_STDOUT_MAX_BYTES {
            return Err(EngineError::LaunchPreparerLimitExceeded {
                handler: handler.canonical_ref().to_owned(),
                detail: format!(
                    "canonical launch-preparer response is {} bytes; limit is {}",
                    canonical_response.len(),
                    LAUNCH_PREPARER_STDOUT_MAX_BYTES
                ),
            });
        }
        Ok(response)
    }
}
