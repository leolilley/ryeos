use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::protocol_vocabulary::error::VocabularyError;
use crate::subprocess_spec::SubprocessBuildRequest;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EnvInjectionSource {
    /// Full URL the child posts callbacks to. Requires
    /// CallbackChannel != None.
    CallbackTokenUrl,
    /// Stable thread identifier the daemon uses to correlate.
    ThreadId,
    /// Effective project root path on disk.
    ProjectPath,
    /// Acting principal fingerprint (key used to authorize the dispatch).
    ActingPrincipal,
    /// Path to the daemon's CAS root (objects directory).
    CasRoot,
    /// Vault handle the child uses to fetch decrypted secrets.
    VaultHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvInjection {
    pub name: String,
    pub source: EnvInjectionSource,
}

pub const RESERVED_ENV_NAMES: &[&str] = &[
    "PATH", "HOME", "USER", "SHELL", "TERM", "LANG", "LC_ALL",
    "PWD", "OLDPWD",
    // LD_* prefix matched by starts_with("LD_")
    // RUST_* prefix matched by starts_with("RUST_")
];

pub fn is_reserved_env_name(name: &str) -> bool {
    RESERVED_ENV_NAMES.contains(&name)
        || name.starts_with("LD_")
        || name.starts_with("RUST_")
}

pub fn produce_env_value(
    source: EnvInjectionSource,
    request: &SubprocessBuildRequest,
) -> Result<String, EngineError> {
    match source {
        EnvInjectionSource::ThreadId => Ok(request.thread_id.clone()),
        EnvInjectionSource::ProjectPath => Ok(request.project_path.to_string_lossy().to_string()),
        EnvInjectionSource::ActingPrincipal => Ok(request.acting_principal.clone()),
        EnvInjectionSource::CasRoot => Ok(request.cas_root.to_string_lossy().to_string()),
        EnvInjectionSource::CallbackTokenUrl => {
            request.callback_token.clone().ok_or_else(|| {
                EngineError::Internal(
                    "callback_token_url requested but no callback_token available".into(),
                )
            })
        }
        EnvInjectionSource::VaultHandle => {
            request.vault_handle.clone().ok_or_else(|| {
                EngineError::Internal(
                    "vault_handle requested but no vault_handle available".into(),
                )
            })
        }
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
            acting_principal: "fp:abc123".to_string(),
            cas_root: PathBuf::from("/cas/root"),
            callback_token: Some("http://localhost:8080/cb".to_string()),
            vault_handle: Some("vault-handle-1".to_string()),
            params: serde_json::json!({}),
            resolution_output: None,
        }
    }

    #[test]
    fn round_trip_all_sources() {
        for src in [
            EnvInjectionSource::CallbackTokenUrl,
            EnvInjectionSource::ThreadId,
            EnvInjectionSource::ProjectPath,
            EnvInjectionSource::ActingPrincipal,
            EnvInjectionSource::CasRoot,
            EnvInjectionSource::VaultHandle,
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
    fn producer_callback_token_url() {
        let req = make_request();
        let val = produce_env_value(EnvInjectionSource::CallbackTokenUrl, &req).unwrap();
        assert_eq!(val, "http://localhost:8080/cb");
    }

    #[test]
    fn producer_callback_token_url_errors_when_missing() {
        let mut req = make_request();
        req.callback_token = None;
        let result = produce_env_value(EnvInjectionSource::CallbackTokenUrl, &req);
        assert!(result.is_err());
    }

    #[test]
    fn producer_vault_handle() {
        let req = make_request();
        let val = produce_env_value(EnvInjectionSource::VaultHandle, &req).unwrap();
        assert_eq!(val, "vault-handle-1");
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
        for name in &["RYE_FOO", "MY_VAR", "SOME_CUSTOM_ENV"] {
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
