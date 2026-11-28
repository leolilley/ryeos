use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum InitState {
    Initialized,
    NotInitialized { diagnostics: InitDiagnostics },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InitDiagnostics {
    pub app_root: PathBuf,
    pub bundles_dir: PathBuf,
    pub code: InitDiagnosticCode,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InitDiagnosticCode {
    SystemSpaceMissing,
    BundleRegistrationsMissing,
    BundleRegistrationsEmpty,
}

impl InitDiagnosticCode {
    /// The stable code. The two registration codes are the bundle
    /// registration validator's own, so a node refuses a missing or empty
    /// registration directory with one code whichever check reaches it first.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SystemSpaceMissing => "system_space_missing",
            Self::BundleRegistrationsMissing => {
                ryeos_node_runtime::bundle_bootstrap::BUNDLE_REGISTRATIONS_MISSING
            }
            Self::BundleRegistrationsEmpty => {
                ryeos_node_runtime::bundle_bootstrap::BUNDLE_REGISTRATIONS_EMPTY
            }
        }
    }
}

/// Whether a node is here: presence only. It admits nothing: every path that
/// uses the registrations reads them through the bundle registration
/// validator (`ryeos_node_runtime::bundle_bootstrap`) afterwards. It is
/// strictly looser than that validator, so it never refuses a node the
/// validator would load.
pub fn init_state(app_root: &Path) -> Result<InitState> {
    let bundles_dir = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("bundles");

    if !app_root.exists() {
        return Ok(InitState::NotInitialized {
            diagnostics: InitDiagnostics {
                app_root: app_root.to_path_buf(),
                bundles_dir,
                code: InitDiagnosticCode::SystemSpaceMissing,
                message: format!("app root missing at {}", app_root.display()),
            },
        });
    }

    if !bundles_dir.is_dir() {
        return Ok(InitState::NotInitialized {
            diagnostics: InitDiagnostics {
                app_root: app_root.to_path_buf(),
                bundles_dir: bundles_dir.clone(),
                code: InitDiagnosticCode::BundleRegistrationsMissing,
                message: format!(
                    "bundle registration directory missing at {}",
                    bundles_dir.display()
                ),
            },
        });
    }

    let has_registration = std::fs::read_dir(&bundles_dir)
        .with_context(|| format!("read {}", bundles_dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .any(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yaml" | "yml")
            ) && std::fs::read_to_string(&path)
                .map(|s| s.contains("ryeos:signed:"))
                .unwrap_or(false)
        });

    if !has_registration {
        return Ok(InitState::NotInitialized {
            diagnostics: InitDiagnostics {
                app_root: app_root.to_path_buf(),
                bundles_dir: bundles_dir.clone(),
                code: InitDiagnosticCode::BundleRegistrationsEmpty,
                message: format!(
                    "no signed bundle registrations at {}",
                    bundles_dir.display()
                ),
            },
        });
    }

    Ok(InitState::Initialized)
}

pub fn require_initialized(app_root: &Path) -> Result<()> {
    match init_state(app_root)? {
        InitState::Initialized => Ok(()),
        InitState::NotInitialized { diagnostics } => bail!(
            "RyeOS is not initialized ({}): {}\nRun: ryeos init",
            diagnostics.code.as_str(),
            diagnostics.message
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registration_codes_are_the_validators_and_match_their_serialized_names() {
        for code in [
            InitDiagnosticCode::SystemSpaceMissing,
            InitDiagnosticCode::BundleRegistrationsMissing,
            InitDiagnosticCode::BundleRegistrationsEmpty,
        ] {
            assert_eq!(
                serde_json::to_value(&code).unwrap(),
                serde_json::Value::String(code.as_str().to_owned())
            );
        }
    }

    #[test]
    fn a_missing_registration_directory_refuses_with_the_validators_code() {
        let tmp = tempfile::tempdir().unwrap();
        let error = require_initialized(tmp.path()).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains(ryeos_node_runtime::bundle_bootstrap::BUNDLE_REGISTRATIONS_MISSING),
            "{error:#}"
        );
    }

    #[test]
    fn the_probe_accepts_every_registration_set_the_validator_accepts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".ai/node/identity")).unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(
            &root.join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: identity.fingerprint().to_owned(),
                verifying_key: identity.signing_key().verifying_key(),
                label: None,
            },
        ]);
        let registrations = root.join(".ai/node/bundles");
        std::fs::create_dir_all(&registrations).unwrap();
        // One registration, then two, each accepted by the validator.
        for name in ["core", "extra"] {
            let bundle = root.join(".ai/bundles").join(name);
            std::fs::create_dir_all(&bundle).unwrap();
            let body =
                serde_yaml::to_string(&serde_json::json!({ "kind": "node", "path": bundle }))
                    .unwrap();
            std::fs::write(
                registrations.join(format!("{name}.yaml")),
                lillux::signature::sign_content(&body, identity.signing_key(), "#", None),
            )
            .unwrap();

            let loaded = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
                app_root: root,
                trust_store: &trust,
            }
            .load_registrations()
            .expect("the validator accepts the registrations");
            assert!(!loaded.is_empty());
            assert_eq!(
                init_state(root).unwrap(),
                InitState::Initialized,
                "the probe must accept what the validator accepts"
            );
        }
    }

    #[test]
    fn missing_app_root_is_not_initialized() {
        let tmp = tempfile::tempdir().unwrap();
        let state = init_state(&tmp.path().join("missing")).unwrap();
        assert!(matches!(state, InitState::NotInitialized { .. }));
    }

    #[test]
    fn signed_registration_is_initialized() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles = tmp.path().join(".ai/node/bundles");
        std::fs::create_dir_all(&bundles).unwrap();
        std::fs::write(
            bundles.join("core.yaml"),
            "# ryeos:signed:test\nkind: node\n",
        )
        .unwrap();
        assert_eq!(init_state(tmp.path()).unwrap(), InitState::Initialized);
    }
}
