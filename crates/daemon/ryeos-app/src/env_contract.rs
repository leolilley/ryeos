use std::collections::{BTreeMap, HashSet};

use thiserror::Error;

use ryeos_engine::protocol_vocabulary::EnvInjectionSource;

pub const BASE_ALLOWLIST_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TMPDIR",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "RYEOSD_TEST_STDERR_DIR",
    // Test-only: lets the graph crash-recovery e2e propagate its
    // checkpoint-park node name into the graph runtime subprocess. Prod-inert
    // (unset in every non-test launch), same pattern as RYEOSD_TEST_STDERR_DIR.
    "RYEOS_GRAPH_TEST_BLOCK_AFTER_CHECKPOINT",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

const DAEMON_ROOT_NAMES: &[&str] = &["RYEOS_APP_ROOT"];

const ENGINE_PLAN_NAMES: &[&str] = &[
    "RYEOS_ITEM_PATH",
    "RYEOS_ITEM_KIND",
    "RYEOS_ITEM_REF",
    "RYEOS_PROJECT_ROOT",
    "RYEOS_SITE_ID",
    "RYEOS_ORIGIN_SITE_ID",
    "RYEOS_THREAD_ID",
    "RYEOS_CHAIN_ROOT_ID",
];

const DAEMON_CALLBACK_NAMES: &[&str] = &[
    "RYEOSD_SOCKET_PATH",
    "RYEOSD_CALLBACK_TOKEN",
    "RYEOSD_THREAD_ID",
    "RYEOSD_PROJECT_PATH",
    "RYEOSD_THREAD_AUTH_TOKEN",
];

const DAEMON_RESUME_NAMES: &[&str] = &["RYEOS_CHECKPOINT_DIR", "RYEOS_RESUME"];

const PROXY_AND_CA_NAMES: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvSourceKind {
    BaseAllowlist,
    DaemonRoot,
    DeclaredSecret,
    LaunchSecret,
    EnginePlanEnv,
    RuntimeDescriptor,
    RuntimeInterpreter,
    RuntimePathMutation,
    ProtocolInjection,
    DaemonResume,
    PerSpawnDaemon,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvSourceDetail {
    BaseAllowlist,
    DaemonRoot,
    DeclaredSecret,
    LaunchSecret,
    EnginePlanEnv,
    RuntimeDescriptor,
    RuntimeInterpreter,
    RuntimePathMutation,
    ProtocolInjection { source: EnvInjectionSource },
    DaemonResume,
    PerSpawnDaemon,
}

impl From<EnvSourceKind> for EnvSourceDetail {
    fn from(value: EnvSourceKind) -> Self {
        match value {
            EnvSourceKind::BaseAllowlist => EnvSourceDetail::BaseAllowlist,
            EnvSourceKind::DaemonRoot => EnvSourceDetail::DaemonRoot,
            EnvSourceKind::DeclaredSecret => EnvSourceDetail::DeclaredSecret,
            EnvSourceKind::LaunchSecret => EnvSourceDetail::LaunchSecret,
            EnvSourceKind::EnginePlanEnv => EnvSourceDetail::EnginePlanEnv,
            EnvSourceKind::RuntimeDescriptor => EnvSourceDetail::RuntimeDescriptor,
            EnvSourceKind::RuntimeInterpreter => EnvSourceDetail::RuntimeInterpreter,
            EnvSourceKind::RuntimePathMutation => EnvSourceDetail::RuntimePathMutation,
            EnvSourceKind::ProtocolInjection => {
                panic!("protocol injection env requires EnvSourceDetail::ProtocolInjection")
            }
            EnvSourceKind::DaemonResume => EnvSourceDetail::DaemonResume,
            EnvSourceKind::PerSpawnDaemon => EnvSourceDetail::PerSpawnDaemon,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvBinding {
    pub key: String,
    pub value: String,
    pub source: EnvSourceDetail,
}

impl EnvBinding {
    pub fn new(key: impl Into<String>, value: impl Into<String>, source: EnvSourceDetail) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            source,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DaemonRootEnv {
    pub app_root: Option<String>,
}

impl DaemonRootEnv {
    pub fn from_resolution_roots(
        _roots: &ryeos_engine::item_resolution::ResolutionRoots,
        app_root: &std::path::Path,
    ) -> Self {
        Self {
            app_root: Some(app_root.to_string_lossy().into_owned()),
        }
    }
}

#[derive(Debug, Error)]
pub enum EnvContractError {
    #[error("invalid env name `{key}` from {env_source:?}: {reason}")]
    InvalidName {
        key: String,
        env_source: EnvSourceDetail,
        reason: String,
    },

    #[error(
        "env key `{key}` from {new_source:?} would override protected source {existing_source:?}"
    )]
    ProtectedCollision {
        key: String,
        existing_source: EnvSourceDetail,
        new_source: EnvSourceDetail,
    },

    #[error("duplicate env key `{key}` from {env_source:?}")]
    DuplicateWithinSource {
        key: String,
        env_source: EnvSourceDetail,
    },
}

#[derive(Debug, Default)]
pub struct EnvContractBuilder {
    bindings: BTreeMap<String, EnvBinding>,
}

impl EnvContractBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_base_allowlist<I, K, V>(mut self, host_env: I) -> Result<Self, EnvContractError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let allowlist: HashSet<&str> = BASE_ALLOWLIST_NAMES.iter().copied().collect();
        for (key, value) in host_env {
            let key = key.into();
            if allowlist.contains(key.as_str()) {
                self.insert(EnvBinding::new(
                    key,
                    value.into(),
                    EnvSourceDetail::BaseAllowlist,
                ))?;
            }
        }
        Ok(self)
    }

    pub fn with_daemon_roots(mut self, roots: DaemonRootEnv) -> Result<Self, EnvContractError> {
        if let Some(app_root) = roots.app_root {
            self.insert(EnvBinding::new(
                "RYEOS_APP_ROOT",
                app_root,
                EnvSourceDetail::DaemonRoot,
            ))?;
        }
        Ok(self)
    }

    pub fn with_bindings<I>(
        self,
        source: EnvSourceKind,
        bindings: I,
    ) -> Result<Self, EnvContractError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        if matches!(source, EnvSourceKind::ProtocolInjection) {
            panic!("protocol injection env requires with_typed_bindings")
        }
        let detail = EnvSourceDetail::from(source);
        self.with_typed_bindings(
            bindings
                .into_iter()
                .map(|(key, value)| EnvBinding::new(key, value, detail.clone())),
        )
    }

    pub fn with_typed_bindings<I>(mut self, bindings: I) -> Result<Self, EnvContractError>
    where
        I: IntoIterator<Item = EnvBinding>,
    {
        for binding in bindings {
            self.insert(binding)?;
        }
        Ok(self)
    }

    pub fn build(self) -> Vec<(String, String)> {
        self.bindings
            .into_iter()
            .map(|(key, binding)| (key, binding.value))
            .collect()
    }

    fn insert(&mut self, binding: EnvBinding) -> Result<(), EnvContractError> {
        validate_binding_name(&binding)?;
        if let Some(existing) = self.bindings.get(&binding.key) {
            if can_replace(existing, &binding) {
                self.bindings.insert(binding.key.clone(), binding);
                return Ok(());
            }
            if existing.source == binding.source {
                return Err(EnvContractError::DuplicateWithinSource {
                    key: binding.key,
                    env_source: binding.source,
                });
            }
            return Err(EnvContractError::ProtectedCollision {
                key: binding.key,
                existing_source: existing.source.clone(),
                new_source: binding.source,
            });
        }
        self.bindings.insert(binding.key.clone(), binding);
        Ok(())
    }
}

pub fn validate_secret_name(name: &str) -> Result<(), EnvContractError> {
    validate_binding_name(&EnvBinding::new(name, "", EnvSourceDetail::DeclaredSecret))
}

fn validate_binding_name(binding: &EnvBinding) -> Result<(), EnvContractError> {
    validate_basic_env_name(&binding.key, &binding.source)?;
    match &binding.source {
        EnvSourceDetail::BaseAllowlist => require_name_in(
            &binding.key,
            &binding.source,
            BASE_ALLOWLIST_NAMES,
            "not in base allowlist",
        ),
        EnvSourceDetail::DaemonRoot => require_name_in(
            &binding.key,
            &binding.source,
            DAEMON_ROOT_NAMES,
            "not a daemon root env name",
        ),
        EnvSourceDetail::DeclaredSecret | EnvSourceDetail::LaunchSecret => {
            validate_application_controlled_name(binding)
        }
        EnvSourceDetail::EnginePlanEnv => require_name_in(
            &binding.key,
            &binding.source,
            ENGINE_PLAN_NAMES,
            "not an engine plan env name",
        ),
        EnvSourceDetail::RuntimeDescriptor => validate_application_controlled_name(binding),
        EnvSourceDetail::RuntimeInterpreter => {
            if binding.key == "RYEOS_PYTHON" {
                Ok(())
            } else {
                validate_application_controlled_name(binding)
            }
        }
        EnvSourceDetail::RuntimePathMutation => {
            if binding.key == "PATH" {
                Ok(())
            } else {
                validate_application_controlled_name(binding)
            }
        }
        EnvSourceDetail::ProtocolInjection { source } => {
            validate_protocol_injection_name(&binding.key, *source, &binding.source)
        }
        EnvSourceDetail::PerSpawnDaemon => {
            if DAEMON_CALLBACK_NAMES.contains(&binding.key.as_str()) {
                Ok(())
            } else {
                invalid(
                    &binding.key,
                    &binding.source,
                    "not an allowed daemon per-spawn env name",
                )
            }
        }
        EnvSourceDetail::DaemonResume => require_name_in(
            &binding.key,
            &binding.source,
            DAEMON_RESUME_NAMES,
            "not an allowed daemon resume env name",
        ),
    }
}

fn validate_basic_env_name(key: &str, source: &EnvSourceDetail) -> Result<(), EnvContractError> {
    if key.is_empty() {
        return invalid(key, source, "empty env name");
    }
    if key.contains('=') {
        return invalid(key, source, "env name contains '='");
    }
    if key.contains('\0') {
        return invalid(key, source, "env name contains NUL");
    }
    Ok(())
}

fn validate_application_controlled_name(binding: &EnvBinding) -> Result<(), EnvContractError> {
    ryeos_vault::policy::validate_key_name(&binding.key).map_err(|e| {
        EnvContractError::InvalidName {
            key: binding.key.clone(),
            env_source: binding.source.clone(),
            reason: format!("{e:#}"),
        }
    })?;

    if binding.key.starts_with("RYEOS_") || binding.key.starts_with("RYEOSD_") {
        return invalid(&binding.key, &binding.source, "reserved RyeOS env prefix");
    }
    if BASE_ALLOWLIST_NAMES.contains(&binding.key.as_str())
        || DAEMON_ROOT_NAMES.contains(&binding.key.as_str())
        || PROXY_AND_CA_NAMES.contains(&binding.key.as_str())
        || ryeos_vault::policy::is_blocked_name(&binding.key)
    {
        return invalid(
            &binding.key,
            &binding.source,
            "would override protected subprocess env",
        );
    }
    Ok(())
}

fn validate_protocol_injection_name(
    key: &str,
    source: EnvInjectionSource,
    detail: &EnvSourceDetail,
) -> Result<(), EnvContractError> {
    let allowed = matches!(
        (source, key),
        (EnvInjectionSource::CallbackSocketPath, "RYEOSD_SOCKET_PATH")
            | (EnvInjectionSource::CallbackToken, "RYEOSD_CALLBACK_TOKEN")
            | (EnvInjectionSource::ThreadId, "RYE_THREAD_ID")
            | (EnvInjectionSource::ThreadId, "RYEOSD_THREAD_ID")
            | (
                EnvInjectionSource::CallbackProjectPath,
                "RYEOSD_PROJECT_PATH"
            )
            | (EnvInjectionSource::ProjectPath, "RYE_PROJECT_PATH")
            | (EnvInjectionSource::ProjectPath, "RYEOS_PROJECT_PATH")
            | (
                EnvInjectionSource::ThreadAuthToken,
                "RYEOSD_THREAD_AUTH_TOKEN"
            )
    );
    if allowed {
        return Ok(());
    }

    if key.starts_with("RYE_") || key.starts_with("RYEOS_") || key.starts_with("RYEOSD_") {
        return invalid(key, detail, "protected protocol env name/source mismatch");
    }
    if BASE_ALLOWLIST_NAMES.contains(&key)
        || DAEMON_ROOT_NAMES.contains(&key)
        || PROXY_AND_CA_NAMES.contains(&key)
    {
        return invalid(key, detail, "would override protected subprocess env");
    }
    Ok(())
}

fn require_name_in(
    key: &str,
    source: &EnvSourceDetail,
    allowed: &[&str],
    reason: &'static str,
) -> Result<(), EnvContractError> {
    if allowed.contains(&key) {
        Ok(())
    } else {
        invalid(key, source, reason)
    }
}

fn invalid<T>(key: &str, source: &EnvSourceDetail, reason: &str) -> Result<T, EnvContractError> {
    Err(EnvContractError::InvalidName {
        key: key.to_string(),
        env_source: source.clone(),
        reason: reason.to_string(),
    })
}

fn can_replace(existing: &EnvBinding, new: &EnvBinding) -> bool {
    matches!(
        (&existing.source, &new.source),
        (EnvSourceDetail::BaseAllowlist, EnvSourceDetail::DaemonRoot)
            | (
                EnvSourceDetail::BaseAllowlist,
                EnvSourceDetail::RuntimePathMutation
            )
            | (
                EnvSourceDetail::BaseAllowlist,
                EnvSourceDetail::PerSpawnDaemon
            )
    ) && (new.key == "PATH"
        || DAEMON_ROOT_NAMES.contains(&new.key.as_str())
        || DAEMON_CALLBACK_NAMES.contains(&new.key.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_env(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn declared_secret_policy_allows_application_names() {
        validate_secret_name("SUPABASE_SERVICE_KEY").unwrap();
        validate_secret_name("OXYLABS_PASSWORD").unwrap();
    }

    #[test]
    fn declared_secret_policy_rejects_protected_names() {
        for name in [
            "",
            "BAD=NAME",
            "BAD\0NAME",
            "RYEOS_APP_ROOT",
            "RYEOSD_THREAD_AUTH_TOKEN",
            "RYEOS_PROJECT_SECRET",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "SSL_CERT_FILE",
            "PATH",
            "HOME",
            "PYTHONHOME",
            "LD_AUDIT",
            "LD_DEBUG",
            "DYLD_PRINT_LIBRARIES",
        ] {
            assert!(
                validate_secret_name(name).is_err(),
                "{name:?} should reject"
            );
        }
    }

    #[test]
    fn base_allowlist_copies_only_infrastructure() {
        let env = EnvContractBuilder::new()
            .with_base_allowlist(host_env(&[
                ("PATH", "/bin"),
                ("OPENAI_API_KEY", "secret"),
                ("https_proxy", "http://proxy"),
                ("RYEOS_BROWSER_INTEGRATION", "1"),
                ("RYEOSD_THREAD_AUTH_TOKEN", "bad"),
            ]))
            .unwrap()
            .build();
        let map: BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(map.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(
            map.get("https_proxy").map(String::as_str),
            Some("http://proxy")
        );
        assert!(!map.contains_key("RYEOS_BROWSER_INTEGRATION"));
        assert!(!map.contains_key("OPENAI_API_KEY"));
        assert!(!map.contains_key("RYEOSD_THREAD_AUTH_TOKEN"));
    }

    #[test]
    fn daemon_roots_override_host_roots() {
        let env = EnvContractBuilder::new()
            .with_base_allowlist(host_env(&[("RYEOS_APP_ROOT", "/evil-system")]))
            .unwrap()
            .with_daemon_roots(DaemonRootEnv {
                app_root: Some("/real-system".to_string()),
            })
            .unwrap()
            .build();
        let map: BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(
            map.get("RYEOS_APP_ROOT").map(String::as_str),
            Some("/real-system")
        );
    }

    #[test]
    fn application_secret_cannot_override_base_or_roots() {
        let err = EnvContractBuilder::new()
            .with_base_allowlist(host_env(&[("PATH", "/bin")]))
            .unwrap()
            .with_bindings(
                EnvSourceKind::DeclaredSecret,
                vec![("PATH".to_string(), "secret".to_string())],
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("PATH"));

        let err = EnvContractBuilder::new()
            .with_daemon_roots(DaemonRootEnv {
                app_root: Some("/real-system".to_string()),
            })
            .unwrap()
            .with_bindings(
                EnvSourceKind::DeclaredSecret,
                vec![("RYEOS_APP_ROOT".to_string(), "secret".to_string())],
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("RYEOS_APP_ROOT"));
    }

    #[test]
    fn application_controlled_sources_reject_dynamic_loader_env() {
        for source in [
            EnvSourceKind::DeclaredSecret,
            EnvSourceKind::RuntimeDescriptor,
        ] {
            for key in ["LD_AUDIT", "LD_DEBUG", "DYLD_PRINT_LIBRARIES"] {
                let err = EnvContractBuilder::new()
                    .with_bindings(source, vec![(key.to_string(), "x".to_string())])
                    .unwrap_err();
                assert!(format!("{err:#}").contains(key), "got: {err:#}");
            }
        }
    }

    #[test]
    fn engine_plan_env_allows_known_ryeos_keys_only() {
        EnvContractBuilder::new()
            .with_bindings(
                EnvSourceKind::EnginePlanEnv,
                vec![
                    ("RYEOS_ITEM_REF".to_string(), "tool:x".to_string()),
                    ("RYEOS_THREAD_ID".to_string(), "thread:x".to_string()),
                    ("RYEOS_CHAIN_ROOT_ID".to_string(), "chain:x".to_string()),
                ],
            )
            .unwrap();

        let err = EnvContractBuilder::new()
            .with_bindings(
                EnvSourceKind::EnginePlanEnv,
                vec![("RYEOS_UNKNOWN".to_string(), "x".to_string())],
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("RYEOS_UNKNOWN"));
    }

    #[test]
    fn per_spawn_daemon_cannot_override_daemon_roots() {
        let err = EnvContractBuilder::new()
            .with_daemon_roots(DaemonRootEnv {
                app_root: Some("/real-system".to_string()),
            })
            .unwrap()
            .with_bindings(
                EnvSourceKind::PerSpawnDaemon,
                vec![("RYEOS_APP_ROOT".to_string(), "/evil".to_string())],
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("RYEOS_APP_ROOT"));
    }

    #[test]
    fn runtime_interpreter_and_path_mutation_are_narrow() {
        EnvContractBuilder::new()
            .with_bindings(
                EnvSourceKind::RuntimeInterpreter,
                vec![("RYEOS_PYTHON".to_string(), "/venv/bin/python".to_string())],
            )
            .unwrap();
        EnvContractBuilder::new()
            .with_bindings(
                EnvSourceKind::RuntimePathMutation,
                vec![("PATH".to_string(), "/tool:/bin".to_string())],
            )
            .unwrap();
        let pythonpath_err = EnvContractBuilder::new()
            .with_bindings(
                EnvSourceKind::RuntimePathMutation,
                vec![("PYTHONPATH".to_string(), "/tool".to_string())],
            )
            .unwrap_err();
        assert!(format!("{pythonpath_err:#}").contains("PYTHONPATH"));
        assert!(EnvContractBuilder::new()
            .with_bindings(
                EnvSourceKind::RuntimeDescriptor,
                vec![("RYEOS_PYTHON".to_string(), "bad".to_string())],
            )
            .is_err());
        assert!(EnvContractBuilder::new()
            .with_bindings(
                EnvSourceKind::RuntimeDescriptor,
                vec![("PATH".to_string(), "bad".to_string())],
            )
            .is_err());
    }

    #[test]
    fn protocol_protected_names_require_matching_source() {
        EnvContractBuilder::new()
            .with_typed_bindings(vec![EnvBinding::new(
                "RYEOSD_THREAD_AUTH_TOKEN",
                "tat",
                EnvSourceDetail::ProtocolInjection {
                    source: EnvInjectionSource::ThreadAuthToken,
                },
            )])
            .unwrap();

        let err = EnvContractBuilder::new()
            .with_typed_bindings(vec![EnvBinding::new(
                "RYEOSD_THREAD_AUTH_TOKEN",
                "callback-token",
                EnvSourceDetail::ProtocolInjection {
                    source: EnvInjectionSource::CallbackToken,
                },
            )])
            .unwrap_err();
        assert!(format!("{err:#}").contains("RYEOSD_THREAD_AUTH_TOKEN"));
    }

    #[test]
    fn error_messages_do_not_include_values() {
        let err = EnvContractBuilder::new()
            .with_bindings(
                EnvSourceKind::RuntimeDescriptor,
                vec![("RYEOS_SECRET".to_string(), "super-secret-value".to_string())],
            )
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("RYEOS_SECRET"));
        assert!(!msg.contains("super-secret-value"));
    }
}
