use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::protocol_vocabulary::error::VocabularyError;
use crate::subprocess_spec::SubprocessBuildRequest;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvInjectionSource {
    /// Unix socket path of the daemon's callback listener. Used when
    /// the descriptor wants the raw socket path (e.g., `RYEOSD_SOCKET_PATH`).
    CallbackSocketPath,
    /// Opaque token string the child includes in callbacks for auth.
    CallbackToken,
    /// Stable thread identifier the daemon uses to correlate.
    ThreadId,
    /// Effective project root path on disk.
    ProjectPath,
    /// Opaque durable-state namespace derived from admitted project identity.
    /// Projectless executions receive an empty value.
    ProjectStateScope,
    /// Daemon-owned directory this launch may write its own state into. A
    /// read-only pinned project keeps it apart from the project; every other
    /// project authority keeps it at the project path.
    RuntimeStateRoot,
    /// The one node-owned bundle state namespace this launch may use: the
    /// isolation mount destination under enforced isolation, the real
    /// directory otherwise. Yields no variable when the launch opened no
    /// state home.
    BundleStateHome,
    /// Acting principal fingerprint (key used to authorize the dispatch).
    ActingPrincipal,
    /// Path to the daemon's CAS root (objects directory).
    CasRoot,
    /// Per-thread auth token proving subprocess identity on callbacks.
    /// Required on every `runtime.dispatch_action` call.
    ThreadAuthToken,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvInjection {
    pub name: String,
    pub source: EnvInjectionSource,
}

pub const RESERVED_ENV_NAMES: &[&str] = &[
    "PATH", "HOME", "USER", "SHELL", "TERM", "LANG", "LC_ALL", "PWD",
    "OLDPWD",
    // LD_* prefix matched by starts_with("LD_")
    // RUST_* prefix matched by starts_with("RUST_")
];

pub fn is_reserved_env_name(name: &str) -> bool {
    RESERVED_ENV_NAMES.contains(&name) || name.starts_with("LD_") || name.starts_with("RUST_")
}

/// The environment value `source` yields for `request`, or `None` when the
/// source is optional and the launch has nothing to name (a launch without a
/// state home sets no state-home variable). Every other source is required.
pub fn produce_optional_env_value(
    source: EnvInjectionSource,
    request: &SubprocessBuildRequest,
) -> Result<Option<String>, EngineError> {
    match (source, request.bundle_state_home.as_deref()) {
        (EnvInjectionSource::BundleStateHome, None) => Ok(None),
        _ => produce_env_value(source, request).map(Some),
    }
}

pub fn produce_env_value(
    source: EnvInjectionSource,
    request: &SubprocessBuildRequest,
) -> Result<String, EngineError> {
    match source {
        EnvInjectionSource::ThreadId => Ok(request.thread_id.clone()),
        EnvInjectionSource::ProjectPath => request
            .project_path
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| EngineError::Internal("project path is not valid UTF-8".into())),
        EnvInjectionSource::ProjectStateScope => {
            Ok(request.project_state_scope.clone().unwrap_or_default())
        }
        EnvInjectionSource::RuntimeStateRoot => request
            .runtime_state_root
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| EngineError::Internal("runtime state root is not valid UTF-8".into())),
        EnvInjectionSource::BundleStateHome => request
            .bundle_state_home
            .as_deref()
            .ok_or_else(|| {
                EngineError::Internal(
                    "bundle_state_home requested but the launch opened no state home".into(),
                )
            })?
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| EngineError::Internal("bundle state home is not valid UTF-8".into())),
        EnvInjectionSource::ActingPrincipal => Ok(request.acting_principal.clone()),
        EnvInjectionSource::CasRoot => request
            .cas_root
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| EngineError::Internal("CAS root is not valid UTF-8".into())),
        EnvInjectionSource::CallbackSocketPath => {
            request.callback_socket_path.clone().ok_or_else(|| {
                EngineError::Internal(
                    "callback_socket_path requested but no callback_socket_path available".into(),
                )
            })
        }
        EnvInjectionSource::CallbackToken => request.callback_token.clone().ok_or_else(|| {
            EngineError::Internal("callback_token requested but no callback_token available".into())
        }),
        EnvInjectionSource::ThreadAuthToken => request.thread_auth_token.clone().ok_or_else(|| {
            EngineError::Internal(
                "thread_auth_token requested but no thread_auth_token available".into(),
            )
        }),
    }
}

/// Validate an env var name: POSIX-compliant, not reserved.
pub fn validate_env_name(name: &str) -> Result<(), VocabularyError> {
    // POSIX: [A-Z_][A-Z0-9_]*
    let re = regex::Regex::new(r"^[A-Z_][A-Z0-9_]*$").unwrap();
    if !re.is_match(name) {
        return Err(VocabularyError::InvalidEnvName { name: name.into() });
    }
    if is_reserved_env_name(name) {
        return Err(VocabularyError::ReservedEnvName { name: name.into() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol_vocabulary::error::VocabularyError;
    use std::path::PathBuf;

    fn make_request() -> SubprocessBuildRequest {
        SubprocessBuildRequest {
            cmd: PathBuf::from("/bin/echo"),
            args: vec![],
            cwd: PathBuf::from("/tmp"),
            timeout: std::time::Duration::from_secs(30),
            item_ref: crate::canonical_ref::CanonicalRef::parse("tool:test/id").unwrap(),
            thread_id: "T-test-thread".to_string(),
            project_path: PathBuf::from("/project"),
            runtime_state_root: PathBuf::from("/state/launch"),
            bundle_state_home: None,
            acting_principal: "fp:abc123".to_string(),
            cas_root: PathBuf::from("/cas/root"),
            callback_token: Some("tok-abc".to_string()),
            callback_socket_path: Some("/tmp/ryeos-callback.sock".to_string()),
            project_state_scope: Some("a".repeat(64)),
            thread_auth_token: Some("tat-abc123".to_string()),
            params: serde_json::json!({}),
            resolution_output: None,
        }
    }

    #[test]
    fn round_trip_all_sources() {
        for src in [
            EnvInjectionSource::CallbackSocketPath,
            EnvInjectionSource::CallbackToken,
            EnvInjectionSource::ThreadId,
            EnvInjectionSource::ProjectPath,
            EnvInjectionSource::ProjectStateScope,
            EnvInjectionSource::RuntimeStateRoot,
            EnvInjectionSource::BundleStateHome,
            EnvInjectionSource::ActingPrincipal,
            EnvInjectionSource::CasRoot,
            EnvInjectionSource::ThreadAuthToken,
        ] {
            let yaml = serde_yaml::to_string(&src).unwrap();
            let parsed: EnvInjectionSource = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(parsed, src);
        }
    }

    #[test]
    fn reject_unknown_source() {
        let err = serde_yaml::from_str::<EnvInjectionSource>("unknown_source");
        assert!(err.is_err());
    }

    #[test]
    fn producer_thread_id() {
        let req = make_request();
        let val = produce_env_value(EnvInjectionSource::ThreadId, &req).unwrap();
        assert_eq!(val, "T-test-thread");
    }

    #[test]
    fn producer_project_path() {
        let req = make_request();
        let val = produce_env_value(EnvInjectionSource::ProjectPath, &req).unwrap();
        assert_eq!(val, "/project");
    }

    #[test]
    fn producer_project_state_scope_is_opaque_and_projectless_is_empty() {
        let mut req = make_request();
        let val = produce_env_value(EnvInjectionSource::ProjectStateScope, &req).unwrap();
        assert_eq!(val, "a".repeat(64));
        assert!(!val.contains('/'));

        req.project_state_scope = None;
        assert_eq!(
            produce_env_value(EnvInjectionSource::ProjectStateScope, &req).unwrap(),
            ""
        );
    }

    #[test]
    fn producer_runtime_state_root_is_not_the_project_path() {
        let req = make_request();
        let val = produce_env_value(EnvInjectionSource::RuntimeStateRoot, &req).unwrap();
        assert_eq!(val, "/state/launch");
    }

    #[test]
    fn producer_bundle_state_home_is_optional() {
        let mut req = make_request();
        assert_eq!(
            produce_optional_env_value(EnvInjectionSource::BundleStateHome, &req).unwrap(),
            None
        );
        assert!(produce_env_value(EnvInjectionSource::BundleStateHome, &req).is_err());
        req.bundle_state_home = Some(PathBuf::from("/run/ryeos/bundle-state"));
        assert_eq!(
            produce_optional_env_value(EnvInjectionSource::BundleStateHome, &req)
                .unwrap()
                .as_deref(),
            Some("/run/ryeos/bundle-state")
        );
        // Required sources stay required.
        assert!(
            produce_optional_env_value(EnvInjectionSource::ThreadId, &req)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn producer_acting_principal() {
        let req = make_request();
        let val = produce_env_value(EnvInjectionSource::ActingPrincipal, &req).unwrap();
        assert_eq!(val, "fp:abc123");
    }

    #[test]
    fn producer_cas_root() {
        let req = make_request();
        let val = produce_env_value(EnvInjectionSource::CasRoot, &req).unwrap();
        assert_eq!(val, "/cas/root");
    }

    #[test]
    fn producer_callback_socket_path() {
        let req = make_request();
        let val = produce_env_value(EnvInjectionSource::CallbackSocketPath, &req).unwrap();
        assert_eq!(val, "/tmp/ryeos-callback.sock");
    }

    #[test]
    fn producer_callback_token() {
        let req = make_request();
        let val = produce_env_value(EnvInjectionSource::CallbackToken, &req).unwrap();
        assert_eq!(val, "tok-abc");
    }

    #[test]
    fn producer_callback_socket_path_errors_when_missing() {
        let mut req = make_request();
        req.callback_socket_path = None;
        let result = produce_env_value(EnvInjectionSource::CallbackSocketPath, &req);
        assert!(result.is_err());
    }

    #[test]
    fn producer_callback_token_errors_when_missing() {
        let mut req = make_request();
        req.callback_token = None;
        let result = produce_env_value(EnvInjectionSource::CallbackToken, &req);
        assert!(result.is_err());
    }

    #[test]
    fn reserved_env_names_rejected() {
        for name in &["LD_PRELOAD", "RUST_LOG", "PATH", "HOME"] {
            assert!(is_reserved_env_name(name), "{name} should be reserved");
            let err = validate_env_name(name);
            assert!(err.is_err(), "{name} should fail validation");
        }
    }

    #[test]
    fn non_reserved_env_names_accepted() {
        for name in &["RYEOS_FOO", "MY_VAR", "SOME_CUSTOM_ENV"] {
            assert!(!is_reserved_env_name(name));
            validate_env_name(name).unwrap();
        }
    }

    #[test]
    fn lowercase_rejected() {
        let err = validate_env_name("my_var");
        assert!(matches!(err, Err(VocabularyError::InvalidEnvName { .. })));
    }

    #[test]
    fn leading_digit_rejected() {
        let err = validate_env_name("1BAD");
        assert!(matches!(err, Err(VocabularyError::InvalidEnvName { .. })));
    }

    #[test]
    fn special_chars_rejected() {
        let err = validate_env_name("MY-VAR");
        assert!(matches!(err, Err(VocabularyError::InvalidEnvName { .. })));
    }
}
