//! Protocol-to-SubprocessSpec builder.
//!
//! The single function `build_subprocess_spec` is the spine that makes
//! "protocols-as-data" structurally true. It consumes a verified
//! `ProtocolDescriptor` and dispatch-time inputs (`BuildRequest`),
//! walks the descriptor's vocabulary fields, and emits a fully-formed
//! `SubprocessSpec` ready for `sandbox_wrap` and `to_lillux_request`.
//!
//! ## Design invariants
//!
//! 1. Every env key on the produced spec is declared by an
//!    `EnvInjection` in the descriptor. The builder cannot manufacture
//!    env keys.
//! 2. The launch envelope is serialized exactly once, only when the
//!    descriptor declares `stdin.shape: launch_envelope_v1`.
//! 3. Callback bookkeeping (token registration, callback URL fabrication)
//!    happens BEFORE the builder is called, in the launcher. The builder
//!    takes the resulting `CallbackBindings` and surfaces them via
//!    declared injections only.
//! 4. The builder is **pure**: no I/O, no clock reads, no env reads,
//!    no filesystem, no spawn.
//!
//! ## Distinction: base env vs injected env
//!
//! The builder produces ONLY the env entries declared by the descriptor's
//! `env_injections` array. The base allowlisted parent-env entries
//! (PATH, HOME, etc.) continue to apply at the lillux bridge — those
//! are NOT injections, they are the base env. The builder never touches
//! the base env.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use thiserror::Error;

use crate::canonical_ref::CanonicalRef;
use crate::launch_envelope_types::LaunchEnvelope;
use crate::protocol_vocabulary::{
    validate_env_name, EnvInjectionSource, StdinShape, VocabularyError,
};
use crate::protocols::descriptor::ProtocolDescriptor;
use crate::subprocess_spec::SubprocessSpec;

/// Callback details assembled by the launcher before calling the builder.
/// The builder surfaces these via declared env injections only.
#[derive(Debug, Clone)]
pub struct CallbackBindings {
    /// Unix socket path of the daemon's callback listener.
    pub socket_path: String,
    /// Opaque token string for callback authentication.
    pub token: String,
}

/// All inputs the builder needs from the dispatch layer.
///
/// Uses borrowed references where possible for zero-copy. The builder
/// only reads from this struct — it never mutates it.
#[derive(Debug)]
pub struct BuildRequest<'a> {
    /// The verified item being dispatched.
    pub item_ref: &'a CanonicalRef,

    /// Resolved binary path.
    pub binary_path: &'a Path,
    /// Argv excluding cmd[0].
    pub args: &'a [String],

    /// Working directory for the child.
    pub cwd: &'a Path,

    /// Project root for env/secret expansion contexts.
    pub project_path: &'a Path,

    /// Thread identity used by callback bookkeeping.
    pub thread_id: &'a str,

    /// Daemon-side callback details. Whether they are serialized into
    /// env or NOT is the descriptor's call — `EnvInjection`s declare
    /// which keys ship; the builder reads from here only when an
    /// injection asks for them.
    pub callback: Option<&'a CallbackBindings>,

    /// Vault-resolved secrets. Keyed by vault handle name.
    /// Only injected if a descriptor `EnvInjection` requests it.
    pub vault_bindings: &'a [(String, String)],

    /// The fully-built launch envelope when the protocol's
    /// `stdin.shape` is `launch_envelope_v1`. `None` when the protocol
    /// is opaque or parameters-json stdin.
    pub launch_envelope: Option<&'a LaunchEnvelope>,

    /// Hard timeout from policy.
    pub timeout: Duration,

    /// Acting principal fingerprint for cap injection.
    pub acting_principal: &'a str,

    /// Path to the daemon's CAS root.
    pub cas_root: &'a Path,

    /// Path to the daemon's state directory.
    pub system_space_dir: &'a Path,

    /// Per-thread auth token injected into env for callback identity.
    /// Required — no `Option` fallback.
    pub thread_auth_token: &'a str,
}

/// Errors produced by the builder.
#[derive(Debug, Error)]
pub enum BuildError {
    #[error(
        "descriptor `{0}` declared opaque stdin but caller supplied a launch envelope"
    )]
    OpaqueStdinWithEnvelope(String),

    #[error("descriptor `{0}` requires a launch envelope; caller supplied none")]
    EnvelopeRequired(String),

    #[error(
        "env injection `{key}` declared twice in descriptor `{descriptor}`"
    )]
    DuplicateEnvKey { descriptor: String, key: String },

    #[error("env injection failed: {0}")]
    Injection(#[from] VocabularyError),

    #[error("stdin envelope serialize failed: {0}")]
    StdinSerialize(String),
}

/// Build a `SubprocessSpec` from a protocol descriptor and dispatch inputs.
///
/// This is the single entry point for all protocol-backed subprocess
/// construction. Both tool and runtime paths converge through this
/// function.
pub fn build_subprocess_spec(
    descriptor: &ProtocolDescriptor,
    request: &BuildRequest<'_>,
) -> Result<SubprocessSpec, BuildError> {
    let descriptor_id = format!("{}:{}", descriptor.category, descriptor.name);

    // 1. Build stdin bytes from the descriptor's stdin shape.
    let stdin_data = match descriptor.stdin.shape {
        StdinShape::LaunchEnvelopeV1 => {
            match request.launch_envelope {
                Some(envelope) => serde_json::to_vec(envelope).map_err(|e| {
                    BuildError::StdinSerialize(format!(
                        "launch_envelope_v1 serialize failed: {e}"
                    ))
                })?,
                None => return Err(BuildError::EnvelopeRequired(descriptor_id)),
            }
        }
        StdinShape::ParametersJson => {
            if request.launch_envelope.is_some() {
                return Err(BuildError::OpaqueStdinWithEnvelope(descriptor_id));
            }
            // ParametersJson: the caller is expected to serialize params
            // and pass them as the launch_envelope field. If no envelope
            // is provided, stdin is empty (the caller may feed it out of band).
            // For now, produce empty stdin — the daemon caller serializes
            // params separately in the tool dispatch path.
            Vec::new()
        }
        StdinShape::Opaque => {
            if request.launch_envelope.is_some() {
                return Err(BuildError::OpaqueStdinWithEnvelope(descriptor_id));
            }
            Vec::new()
        }
    };

    // 2. Build env from the descriptor's env_injections.
    let mut env_map: BTreeMap<String, String> = BTreeMap::new();
    for injection in &descriptor.env_injections {
        // Validate env name.
        validate_env_name(&injection.name)?;

        // Check for duplicate keys.
        if env_map.contains_key(&injection.name) {
            return Err(BuildError::DuplicateEnvKey {
                descriptor: descriptor_id,
                key: injection.name.clone(),
            });
        }

        // Produce the value based on the source.
        let value = match injection.source {
            EnvInjectionSource::CallbackTokenUrl => {
                request
                    .callback
                    .map(|cb| cb.token.clone())
                    .ok_or_else(|| {
                        VocabularyError::UnknownEnvInjection(
                            injection.name.clone(),
                            "callback_token_url (no callback bindings)".into(),
                        )
                    })?
            }
            EnvInjectionSource::CallbackSocketPath => request
                .callback
                .map(|cb| cb.socket_path.clone())
                .ok_or_else(|| {
                    VocabularyError::UnknownEnvInjection(
                        injection.name.clone(),
                        "callback_socket_path (no callback bindings)".into(),
                    )
                })?,
            EnvInjectionSource::CallbackToken => request
                .callback
                .map(|cb| cb.token.clone())
                .ok_or_else(|| {
                    VocabularyError::UnknownEnvInjection(
                        injection.name.clone(),
                        "callback_token (no callback bindings)".into(),
                    )
                })?,
            // For vocabulary sources that go through SubprocessBuildRequest,
            // build a synthetic request. This avoids duplicating the
            // production logic while keeping the builder pure.
            EnvInjectionSource::ThreadId => request.thread_id.to_string(),
            EnvInjectionSource::ProjectPath => {
                request.project_path.to_string_lossy().to_string()
            }
            EnvInjectionSource::ActingPrincipal => request.acting_principal.to_string(),
            EnvInjectionSource::CasRoot => request.cas_root.to_string_lossy().to_string(),
            EnvInjectionSource::SystemSpaceDir => request.system_space_dir.to_string_lossy().to_string(),
            EnvInjectionSource::ThreadAuthToken => request.thread_auth_token.to_string(),
            EnvInjectionSource::VaultHandle => {
                // Look up the vault handle from vault_bindings.
                // For now, vault_handle injection is not yet wired through
                // BuildRequest; fail with a clear error if the source
                // doesn't have bindings.
                return Err(BuildError::Injection(VocabularyError::UnknownEnvInjection(
                    injection.name.clone(),
                    "vault_handle (vault bindings not yet supported in builder)".into(),
                )));
            }
        };

        env_map.insert(injection.name.clone(), value);
    }

    // Convert BTreeMap to Vec<(String, String)> for SubprocessSpec.
    let env: Vec<(String, String)> = env_map.into_iter().collect();

    // 3. Record stdout shape from descriptor.
    let stdout_shape = descriptor.stdout.shape;

    // 4. Record callback channel from descriptor.
    let callback_channel = descriptor.callback_channel;

    // 5. Assemble the SubprocessSpec.
    Ok(SubprocessSpec {
        cmd: request.binary_path.to_path_buf(),
        args: request.args.to_vec(),
        cwd: request.cwd.to_path_buf(),
        env,
        stdin: stdin_data,
        timeout: request.timeout,
        stdout_shape,
        callback_channel,
        item_ref: request.item_ref.clone(),
        thread_id: request.thread_id.to_string(),
        project_path: request.project_path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol_vocabulary::{
        CallbackChannel, EnvInjection, LifecycleMode, ProtocolCapabilities, StdoutShape,
    };
    use crate::protocols::descriptor::{ProtocolLifecycle, ProtocolStdin, ProtocolStdout};
    use std::path::PathBuf;

    /// Helper to create a minimal runtime_v1-like descriptor.
    fn runtime_v1_descriptor() -> ProtocolDescriptor {
        ProtocolDescriptor {
            kind: "protocol".to_string(),
            name: "runtime_v1".to_string(),
            category: "ryeos/core".to_string(),
            abi_version: "v1".to_string(),
            description: Some("test descriptor".to_string()),
            stdin: ProtocolStdin {
                shape: StdinShape::LaunchEnvelopeV1,
            },
            stdout: ProtocolStdout {
                shape: StdoutShape::RuntimeResultV1,
                mode: crate::protocol_vocabulary::StdoutMode::Terminal,
            },
            env_injections: vec![
                EnvInjection {
                    name: "RYEOSD_SOCKET_PATH".to_string(),
                    source: EnvInjectionSource::CallbackSocketPath,
                },
                EnvInjection {
                    name: "RYEOSD_CALLBACK_TOKEN".to_string(),
                    source: EnvInjectionSource::CallbackToken,
                },
                EnvInjection {
                    name: "RYEOSD_THREAD_ID".to_string(),
                    source: EnvInjectionSource::ThreadId,
                },
                EnvInjection {
                    name: "RYEOSD_PROJECT_PATH".to_string(),
                    source: EnvInjectionSource::ProjectPath,
                },
            ],
            capabilities: ProtocolCapabilities {
                allows_pushed_head: false,
                allows_target_site: false,
                allows_detached: false,
            },
            lifecycle: ProtocolLifecycle {
                mode: LifecycleMode::Managed,
            },
            callback_channel: CallbackChannel::HttpV1,
        }
    }

    /// Helper to create a minimal tool descriptor (parameters_json stdin).
    fn tool_descriptor() -> ProtocolDescriptor {
        ProtocolDescriptor {
            kind: "protocol".to_string(),
            name: "opaque".to_string(),
            category: "ryeos/core".to_string(),
            abi_version: "v1".to_string(),
            description: None,
            stdin: ProtocolStdin {
                shape: StdinShape::ParametersJson,
            },
            stdout: ProtocolStdout {
                shape: StdoutShape::OpaqueBytes,
                mode: crate::protocol_vocabulary::StdoutMode::Terminal,
            },
            env_injections: vec![EnvInjection {
                name: "RYEOSD_PROJECT_PATH".to_string(),
                source: EnvInjectionSource::ProjectPath,
            }],
            capabilities: ProtocolCapabilities {
                allows_pushed_head: false,
                allows_target_site: false,
                allows_detached: false,
            },
            lifecycle: ProtocolLifecycle {
                mode: LifecycleMode::Managed,
            },
            callback_channel: CallbackChannel::None,
        }
    }

    fn make_callback_bindings() -> CallbackBindings {
        CallbackBindings {
            socket_path: "/tmp/ryeos-callback.sock".to_string(),
            token: "tok-test-123".to_string(),
        }
    }

    /// Returns owned values so lifetimes work in tests.
    fn make_test_fixtures() -> (
        CanonicalRef,
        CallbackBindings,
        Vec<String>,
        LaunchEnvelope,
    ) {
        let item_ref = CanonicalRef::parse("runtime:spawn").unwrap();
        let cb = CallbackBindings {
            socket_path: "/tmp/ryeos-callback.sock".to_string(),
            token: "tok-test-123".to_string(),
        };
        let args = vec![
            "--project-path".to_string(),
            "/project".to_string(),
        ];
        let envelope = make_minimal_envelope();
        (item_ref, cb, args, envelope)
    }

    fn make_runtime_request_from_fixtures<'a>(
        item_ref: &'a CanonicalRef,
        args: &'a [String],
        callback: &'a CallbackBindings,
    ) -> BuildRequest<'a> {
        BuildRequest {
            item_ref,
            binary_path: Path::new("/usr/bin/ryeos-runtime"),
            args,
            cwd: Path::new("/project"),
            project_path: Path::new("/project"),
            thread_id: "T-test-thread",
            callback: Some(callback),
            vault_bindings: &[],
            launch_envelope: None, // set per-test
            timeout: Duration::from_secs(120),
            acting_principal: "fp:abc123",
            cas_root: Path::new("/cas/root"),
            system_space_dir: Path::new("/var/lib/ryeos"),
            thread_auth_token: "tat-test-123",
        }
    }

    fn make_minimal_envelope() -> LaunchEnvelope {
        use crate::launch_envelope_types::{
            EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, HardLimits,
        };
        use crate::resolution::{ResolutionOutput, ResolvedAncestor, TrustClass};
        use std::collections::HashMap;

        LaunchEnvelope {
            invocation_id: "inv-test".to_string(),
            thread_id: "T-test-thread".to_string(),
            roots: EnvelopeRoots {
                project_root: PathBuf::from("/project"),
                user_root: None,
                system_roots: vec![],
            },
            request: EnvelopeRequest {
                inputs: serde_json::json!({}),
                previous_thread_id: None,
                parent_thread_id: None,
                parent_capabilities: None,
                depth: 0,
            },
            policy: EnvelopePolicy {
                effective_caps: vec![],
                hard_limits: HardLimits::default(),
            },
            callback: EnvelopeCallback {
                socket_path: PathBuf::from("/tmp/ryeos-callback.sock"),
                token: "tok-test-123".to_string(),
            },
            resolution: ResolutionOutput {
                root: ResolvedAncestor {
                    requested_id: "runtime:spawn".to_string(),
                    resolved_ref: "runtime:spawn".to_string(),
                    source_path: PathBuf::from("/project/.ai/runtimes/ryeos/core/runtime.yaml"),
                    trust_class: TrustClass::Unsigned,
                    alias_resolution: None,
                    added_by: crate::resolution::ResolutionStepName::PipelineInit,
                    raw_content: String::new(),
                    raw_content_digest: "0".repeat(64),
                },
                ancestors: vec![],
                references_edges: vec![],
                step_outputs: HashMap::new(),
                executor_trust_class: TrustClass::Unsigned,
                composed: crate::resolution::KindComposedView::identity(serde_json::json!({})),
                referenced_items: vec![],
            },
            inventory: HashMap::new(),
        }
    }

    #[test]
    fn opaque_stdin_with_envelope_fails_loud() {
        let desc = tool_descriptor();
        let (item_ref, cb, args, envelope) = make_test_fixtures();
        let mut req = make_runtime_request_from_fixtures(&item_ref, &args, &cb);
        req.launch_envelope = Some(&envelope);
        let result = build_subprocess_spec(&desc, &req);
        assert!(matches!(result, Err(BuildError::OpaqueStdinWithEnvelope(_))));
    }

    #[test]
    fn launch_envelope_v1_without_envelope_fails_loud() {
        let desc = runtime_v1_descriptor();
        let (item_ref, cb, args, _envelope) = make_test_fixtures();
        let req = make_runtime_request_from_fixtures(&item_ref, &args, &cb);
        let result = build_subprocess_spec(&desc, &req);
        assert!(matches!(result, Err(BuildError::EnvelopeRequired(_))));
    }

    #[test]
    fn duplicate_env_key_fails_loud() {
        let mut desc = tool_descriptor();
        desc.env_injections.push(EnvInjection {
            name: "RYEOSD_PROJECT_PATH".to_string(), // duplicate!
            source: EnvInjectionSource::ProjectPath,
        });
        let (item_ref, cb, args, _envelope) = make_test_fixtures();
        let req = make_runtime_request_from_fixtures(&item_ref, &args, &cb);
        let result = build_subprocess_spec(&desc, &req);
        assert!(matches!(result, Err(BuildError::DuplicateEnvKey { .. })));
    }

    #[test]
    fn reserved_env_name_fails_loud() {
        let mut desc = tool_descriptor();
        desc.env_injections[0].name = "PATH".to_string(); // reserved!
        let (item_ref, cb, args, _envelope) = make_test_fixtures();
        let req = make_runtime_request_from_fixtures(&item_ref, &args, &cb);
        let result = build_subprocess_spec(&desc, &req);
        assert!(result.is_err());
    }

    #[test]
    fn runtime_v1_descriptor_produces_expected_spec() {
        let desc = runtime_v1_descriptor();
        let (item_ref, cb, args, envelope) = make_test_fixtures();
        let mut req = make_runtime_request_from_fixtures(&item_ref, &args, &cb);
        req.launch_envelope = Some(&envelope);

        let spec = build_subprocess_spec(&desc, &req).unwrap();

        // Assert env contains exactly the 4 declared keys.
        let env_keys: std::collections::HashSet<&String> =
            spec.env.iter().map(|(k, _)| k).collect();
        assert_eq!(env_keys.len(), 4);
        assert!(env_keys.contains(&"RYEOSD_SOCKET_PATH".to_string()));
        assert!(env_keys.contains(&"RYEOSD_CALLBACK_TOKEN".to_string()));
        assert!(env_keys.contains(&"RYEOSD_THREAD_ID".to_string()));
        assert!(env_keys.contains(&"RYEOSD_PROJECT_PATH".to_string()));

        // Assert env values.
        let env_map: BTreeMap<String, String> = spec.env.into_iter().collect();
        assert_eq!(env_map["RYEOSD_SOCKET_PATH"], "/tmp/ryeos-callback.sock");
        assert_eq!(env_map["RYEOSD_CALLBACK_TOKEN"], "tok-test-123");
        assert_eq!(env_map["RYEOSD_THREAD_ID"], "T-test-thread");
        assert_eq!(env_map["RYEOSD_PROJECT_PATH"], "/project");

        // Assert callback_channel and stdout_shape propagated.
        assert_eq!(spec.callback_channel, CallbackChannel::HttpV1);
        assert_eq!(spec.stdout_shape, StdoutShape::RuntimeResultV1);

        // Assert stdin parses back as the launch envelope.
        let parsed: LaunchEnvelope =
            serde_json::from_slice(&spec.stdin).expect("stdin must be valid LaunchEnvelope");
        assert_eq!(parsed.thread_id, "T-test-thread");
        assert_eq!(parsed.invocation_id, "inv-test");
    }

    #[test]
    fn tool_descriptor_produces_expected_spec() {
        let desc = tool_descriptor();
        let (item_ref, cb, args, _envelope) = make_test_fixtures();
        let req = make_runtime_request_from_fixtures(&item_ref, &args, &cb);

        let spec = build_subprocess_spec(&desc, &req).unwrap();

        // Assert env contains only the 1 declared key.
        assert_eq!(spec.env.len(), 1);
        assert_eq!(spec.env[0].0, "RYEOSD_PROJECT_PATH");

        // Assert no stdin data.
        assert!(spec.stdin.is_empty());

        // Assert callback_channel and stdout_shape.
        assert_eq!(spec.callback_channel, CallbackChannel::None);
        assert_eq!(spec.stdout_shape, StdoutShape::OpaqueBytes);

        // Assert provenance fields.
        assert_eq!(spec.thread_id, "T-test-thread");
        assert_eq!(spec.project_path, PathBuf::from("/project"));
    }

    #[test]
    fn no_callback_bindings_when_injection_needs_it_fails() {
        let desc = runtime_v1_descriptor();
        let (item_ref, _cb, args, envelope) = make_test_fixtures();
        let cb = make_callback_bindings();
        let mut req = make_runtime_request_from_fixtures(&item_ref, &args, &cb);
        req.callback = None; // no callback!
        req.launch_envelope = Some(&envelope);

        let result = build_subprocess_spec(&desc, &req);
        // Should fail because the descriptor asks for callback_socket_path
        // but no bindings are provided.
        assert!(result.is_err());
    }

    #[test]
    fn builder_is_deterministic() {
        let desc = tool_descriptor();
        let (item_ref, cb, args, _envelope) = make_test_fixtures();
        let req = make_runtime_request_from_fixtures(&item_ref, &args, &cb);

        let spec1 = build_subprocess_spec(&desc, &req).unwrap();
        let spec2 = build_subprocess_spec(&desc, &req).unwrap();

        assert_eq!(spec1.cmd, spec2.cmd);
        assert_eq!(spec1.env, spec2.env);
        assert_eq!(spec1.stdin, spec2.stdin);
    }
}
