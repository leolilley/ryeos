use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const JOURNAL_SCHEMA: &str = "ryeos/onboarding-journal/v1";
const JOURNAL_MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    WelcomeSeen,
    OperatorCreated,
    CoreInitialized,
    NodeStarted,
    ProviderSelected,
    CredentialStored,
    ProviderValidated,
    ModelSelected,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Journal {
    schema: String,
    #[serde(default)]
    completed: BTreeSet<Phase>,
    #[serde(default)]
    pub operator_fingerprint: Option<String>,
    #[serde(default)]
    pub node_fingerprint: Option<String>,
    #[serde(default)]
    pub vault_fingerprint: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub bundles_verified: Option<usize>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            schema: JOURNAL_SCHEMA.to_string(),
            completed: BTreeSet::new(),
            operator_fingerprint: None,
            node_fingerprint: None,
            vault_fingerprint: None,
            provider_id: None,
            model_name: None,
            bundles_verified: None,
        }
    }
}

impl Journal {
    pub(crate) fn load(app_root: &Path) -> Result<Self> {
        let path = journal_path(app_root);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_file()
            || metadata.len() > JOURNAL_MAX_BYTES
        {
            anyhow::bail!(
                "unsafe or oversized onboarding journal at {}",
                path.display()
            );
        }
        let source = String::from_utf8(lillux::read_regular_file_bounded_no_follow(
            &path,
            JOURNAL_MAX_BYTES,
        )?)
        .with_context(|| format!("read onboarding journal {}", path.display()))?;
        let journal: Self = serde_json::from_str(&source)
            .with_context(|| format!("parse onboarding journal {}", path.display()))?;
        if journal.schema != JOURNAL_SCHEMA {
            anyhow::bail!(
                "unsupported onboarding journal schema '{}'; expected {JOURNAL_SCHEMA}",
                journal.schema
            );
        }
        Ok(journal)
    }

    pub(crate) fn reconcile(&mut self, app_root: &Path) -> Result<Vec<String>> {
        let mut diagnostics = Vec::new();
        let operator_path = app_root
            .join(ryeos_engine::AI_DIR)
            .join("config/keys/signing/private_key.pem");
        match read_signing_fingerprint(&operator_path) {
            Ok(Some(fingerprint)) => {
                self.operator_fingerprint = Some(fingerprint);
                self.completed.insert(Phase::OperatorCreated);
            }
            Ok(None) => {
                if self.completed.remove(&Phase::OperatorCreated) {
                    diagnostics.push(
                        "journal claimed operator creation, but the authoritative key is absent; initialization will safely repeat that phase"
                            .to_string(),
                    );
                }
                self.operator_fingerprint = None;
            }
            Err(error) => return Err(error),
        }
        let node_path = app_root
            .join(ryeos_engine::AI_DIR)
            .join("node/identity/private_key.pem");
        self.node_fingerprint = read_signing_fingerprint(&node_path)?;
        let vault_path = app_root
            .join(ryeos_engine::AI_DIR)
            .join("node/vault/public_key.pem");
        self.vault_fingerprint = if vault_path.exists() {
            Some(
                lillux::vault::read_public_key(&vault_path)
                    .with_context(|| format!("read vault identity {}", vault_path.display()))?
                    .fingerprint(),
            )
        } else {
            None
        };
        match ryeos_node::verify_init_completion(app_root) {
            Ok(Some(completion)) => {
                self.operator_fingerprint = Some(completion.operator_fingerprint);
                self.node_fingerprint = Some(completion.node_fingerprint);
                self.vault_fingerprint = Some(completion.vault_fingerprint);
                self.bundles_verified = Some(completion.bundles_verified);
                self.completed.insert(Phase::CoreInitialized);
            }
            Ok(None) => {
                if self.completed.remove(&Phase::CoreInitialized) {
                    diagnostics.push(
                        "the advisory journal has no signed init-completion record; the idempotent core transaction will repeat"
                            .to_string(),
                    );
                }
            }
            Err(error) => {
                self.completed.remove(&Phase::CoreInitialized);
                diagnostics.push(format!(
                    "signed init-completion verification failed ({error:#}); the idempotent core transaction will repair state"
                ));
            }
        }
        Ok(diagnostics)
    }

    pub(crate) fn contains(&self, phase: Phase) -> bool {
        self.completed.contains(&phase)
    }

    pub(crate) fn mark(&mut self, phase: Phase) {
        self.completed.insert(phase);
    }

    pub(crate) fn unmark(&mut self, phase: Phase) {
        self.completed.remove(&phase);
    }

    pub(crate) fn save(&self, app_root: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() as u64 > JOURNAL_MAX_BYTES {
            anyhow::bail!("onboarding journal exceeds size limit");
        }
        let path = journal_path(app_root);
        lillux::atomic_write(&path, &bytes).map_err(|error| {
            anyhow::anyhow!("write onboarding journal {}: {error}", path.display())
        })?;
        Ok(())
    }
}

fn read_signing_fingerprint(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let key = lillux::crypto::load_signing_key(path)
        .with_context(|| format!("load signing identity {}", path.display()))?;
    Ok(Some(lillux::crypto::fingerprint(&key.verifying_key())))
}

fn journal_path(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("onboarding")
        .join("journal-v1.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_json_cannot_contain_secret_fields() {
        let source = serde_json::to_string(&Journal::default()).unwrap();
        assert!(!source.contains("credential"));
        assert!(!source.contains("entropy"));
        assert!(!source.contains("private"));
    }
}
