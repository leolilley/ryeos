use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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
    NoBundleRegistrations,
}

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
                code: InitDiagnosticCode::NoBundleRegistrations,
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
            "RyeOS is not initialized: {}\nRun: ryeos init",
            diagnostics.message
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
