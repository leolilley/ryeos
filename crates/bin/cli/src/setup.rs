//! Typed setup-domain operations used by onboarding and `ryeos setup`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use ryeos_directive_core::{ProviderConfig, ProviderSetupProjection};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::error::{CliError, CliTransportError};

const SUPPORTED_PROVIDER_VALIDATION_REF: &str = "service:model-providers/validate";

#[derive(Debug)]
pub(crate) struct ProviderCatalog {
    pub providers: Vec<ProviderSetupProjection>,
    pub warnings: Vec<String>,
}

pub(crate) fn discover_verified_providers(app_root: &Path) -> Result<ProviderCatalog, CliError> {
    let snapshot = crate::node_descriptors::load_verified_snapshot(app_root).map_err(|error| {
        CliError::Local {
            detail: format!("load verified provider registrations: {error:#}"),
        }
    })?;
    let bundle_roots = crate::effective_metadata::snapshot_bundle_roots(&snapshot);
    let engine = crate::effective_metadata::build_effective_item_engine(
        app_root,
        None,
        &bundle_roots,
    )
    .map_err(|error| CliError::Local {
        detail: format!("build verified provider projection: {error:#}"),
    })?;
    let mut ids = BTreeSet::new();
    let mut warnings = Vec::new();
    for root in &bundle_roots {
        let directory = root
            .join(ryeos_engine::AI_DIR)
            .join("config/ryeos-runtime/model-providers");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(format!("read provider catalog {}: {error}", directory.display()));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => metadata,
                _ => continue,
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if !matches!(path.extension().and_then(|value| value.to_str()), Some("yaml" | "yml")) {
                continue;
            }
            if let Some(id) = path.file_stem().and_then(|value| value.to_str()) {
                ids.insert(id.to_string());
            }
        }
    }
    let mut providers = Vec::new();
    for id in ids {
        let item_ref = format!("config:ryeos-runtime/model-providers/{id}");
        let value = match crate::effective_metadata::resolve_effective_composed_value(
            &engine,
            &item_ref,
            None,
        ) {
            Ok(Some(value)) => value,
            Ok(None) => {
                warnings.push(format!("verified provider '{id}' did not resolve"));
                continue;
            }
            Err(error) => {
                warnings.push(format!("verified provider '{id}' failed resolution: {error:#}"));
                continue;
            }
        };
        let provider: ProviderConfig = match serde_json::from_value(value) {
            Ok(provider) => provider,
            Err(error) => {
                warnings.push(format!("verified provider '{id}' has invalid schema: {error}"));
                continue;
            }
        };
        if let Err(error) = provider.validate(&format!(" for '{id}'")) {
            warnings.push(error.to_string());
            continue;
        }
        match provider.setup_projection(&id) {
            Ok(projection) => providers.push(projection),
            Err(error) => warnings.push(error.to_string()),
        }
    }
    providers.sort_by(|left, right| {
        right
            .recommended
            .cmp(&left.recommended)
            .then_with(|| left.priority.cmp(&right.priority))
            .then_with(|| left.display_name.cmp(&right.display_name))
    });
    Ok(ProviderCatalog {
        providers,
        warnings,
    })
}

pub(crate) struct LocalSetupClient {
    base_url: String,
    audience: String,
    signer: crate::transport::signing::Signer,
}

impl LocalSetupClient {
    pub(crate) async fn connect(app_root: &Path) -> Result<Self, CliError> {
        crate::daemon_preflight::lifecycle_preflight(app_root).await?;
        let daemon_url = crate::transport::http::resolve_daemon_url(app_root).await?;
        let discovered = crate::transport::discovery::discover_audience(&daemon_url).await?;
        Ok(Self {
            base_url: discovered.effective_base_url,
            audience: discovered.principal_id,
            signer: crate::transport::signing::Signer::resolve(app_root)?,
        })
    }

    pub(crate) async fn vault_keys(&self) -> Result<Vec<String>, CliError> {
        let value = self.get("/vault/list").await?;
        let mut keys = value
            .get("secrets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        keys.sort();
        Ok(keys)
    }

    pub(crate) async fn store_credential(
        &self,
        secret_name: &str,
        secret: &str,
    ) -> Result<(), CliError> {
        #[derive(serde::Serialize)]
        struct StoreCredential<'a> {
            name: &'a str,
            value: &'a str,
        }
        self.post(
            "/vault/set",
            &StoreCredential {
                name: secret_name,
                value: secret,
            },
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn validate_provider(
        &self,
        projection: &ProviderSetupProjection,
        model: Option<&str>,
    ) -> Result<Value, CliError> {
        let validation = projection.validation.as_ref().ok_or_else(|| CliError::Local {
            detail: format!(
                "provider '{}' does not declare a validation operation",
                projection.display_name
            ),
        })?;
        let validation_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(&validation.r#ref).map_err(|error| {
            CliError::Local {
                detail: format!(
                    "provider '{}' declares invalid validation ref '{}': {error}",
                    projection.display_name, validation.r#ref
                ),
            }
        })?;
        if validation_ref.to_string() != SUPPORTED_PROVIDER_VALIDATION_REF {
            return Err(CliError::Local {
                detail: format!(
                    "provider '{}' declares unsupported validation operation '{}'",
                    projection.display_name, validation.r#ref
                ),
            });
        }
        self.post(
            "/execute",
            &serde_json::json!({
                "item_ref": validation.r#ref.as_str(),
                "ref_bindings": {},
                "parameters": {
                    "provider_id": projection.provider_id.as_str(),
                    "model": model,
                }
            }),
        )
        .await
    }

    async fn get(&self, path: &str) -> Result<Value, CliError> {
        let headers = self.signer.sign("GET", path, &[], &self.audience)?;
        crate::transport::http::get_json(&format!("{}{}", self.base_url, path), &headers)
            .await
            .map_err(CliError::from)
    }

    async fn post<T: serde::Serialize + ?Sized>(
        &self,
        path: &str,
        value: &T,
    ) -> Result<Value, CliError> {
        let body = Zeroizing::new(serde_json::to_vec(value).map_err(|error| {
            CliError::Transport(CliTransportError::BodyDecode {
                detail: error.to_string(),
            })
        })?);
        let headers = self
            .signer
            .sign("POST", path, body.as_slice(), &self.audience)?;
        crate::transport::http::post_json(
            &format!("{}{}", self.base_url, path),
            &headers,
            body.as_slice(),
        )
        .await
        .map_err(CliError::from)
    }
}
