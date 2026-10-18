//! Startup take of provider-delivered environment values.
//!
//! The verified node policy generation declares which names are provider
//! delivered. The daemon takes exactly those names out of its own process
//! environment while it is still single-threaded, so the values never reach
//! a descendant by inheritance and never stay readable through the process
//! environment. Bootstrap later refuses to start unless it loads the same
//! generation the names were taken under.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use ryeos_app::vault::ProviderEnvironment;
use ryeos_engine::trust::TrustStore;
use ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy;
use ryeos_node_runtime::node_policy::sections::provider_environment::ProviderEnvironmentPolicy;
#[cfg(test)]
use ryeos_node_runtime::node_policy::sections::provider_environment::{
    PROVIDER_ENVIRONMENT_POLICY_SCHEMA, ProviderEnvironmentName, ProviderEnvironmentRecipients,
};

/// Provider environment taken at startup, bound to the app root and the
/// policy generation that declared its names. Its debug form carries names
/// only.
#[derive(Debug)]
pub struct StartupProviderEnvironment {
    app_root: PathBuf,
    generation_digest: String,
    environment: Arc<ProviderEnvironment>,
}

impl StartupProviderEnvironment {
    pub fn environment(&self) -> &Arc<ProviderEnvironment> {
        &self.environment
    }

    /// Refuse unless bootstrap loaded the same app root and exact policy
    /// generation the names were taken under.
    pub fn require_generation(&self, app_root: &Path, generation_digest: &str) -> Result<()> {
        if self.app_root != app_root {
            bail!(
                "provider environment was taken for app root {}, not {}",
                self.app_root.display(),
                app_root.display()
            );
        }
        if self.generation_digest != generation_digest {
            bail!(
                "provider environment was taken under node policy generation {}, but bootstrap loaded {}; restart the node",
                self.generation_digest,
                generation_digest
            );
        }
        Ok(())
    }
}

/// Load the verified node policy generation and take its declared provider
/// environment names out of the process environment.
///
/// # Safety
/// Call only during single-threaded daemon startup, before any async
/// runtime, application or foreign library thread exists.
pub unsafe fn take_at_startup(app_root: &Path) -> Result<StartupProviderEnvironment> {
    let trust = TrustStore::load(None, &app_root.join(ryeos_engine::AI_DIR).join("config"))
        .context("load node trust for the provider environment")?;
    let snapshot = ryeos_node_runtime::node_policy::load_snapshot(
        app_root,
        &trust,
        &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
    )
    .context("load the node policy generation that declares the provider environment")?;
    let declaration = snapshot.require::<ProviderEnvironmentPolicy>()?;
    validate_declaration(
        declaration,
        snapshot.require::<NodeExecutionAdmissionPolicy>()?,
    )?;
    // SAFETY: forwarded from this function's caller.
    let values = unsafe { lillux::take_environment_values(&declaration.declared_names()) }
        .map_err(anyhow::Error::msg)
        .context("take declared provider environment values")?;
    let environment = ProviderEnvironment::captured(declaration.names.clone(), values)?;
    Ok(StartupProviderEnvironment {
        app_root: app_root.to_path_buf(),
        generation_digest: snapshot.generation_digest().to_owned(),
        environment: Arc::new(environment),
    })
}

/// Checks that need the daemon's subprocess environment contract and the
/// execution section: a declared name must be an application-controlled
/// secret name and must not also be a host-environment passthrough or base
/// allowlist name, which would hand it to children outside delivery.
pub fn validate_declaration(
    declaration: &ProviderEnvironmentPolicy,
    execution: &NodeExecutionAdmissionPolicy,
) -> Result<()> {
    for entry in &declaration.names {
        let name = &entry.name;
        ryeos_app::env_contract::validate_secret_name(name)
            .with_context(|| format!("provider environment name `{name}` is refused"))?;
        if execution.host_env_passthrough.binary_search(name).is_ok() {
            bail!("provider environment name `{name}` is also a host-environment passthrough");
        }
        if ryeos_app::process::subprocess_base_allowlist_names().contains(&name.as_str()) {
            bail!("provider environment name `{name}` is a base allowlist name");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(names: &[&str]) -> ProviderEnvironmentPolicy {
        ProviderEnvironmentPolicy {
            schema: PROVIDER_ENVIRONMENT_POLICY_SCHEMA,
            names: names
                .iter()
                .map(|name| ProviderEnvironmentName {
                    name: (*name).to_owned(),
                    recipients: ProviderEnvironmentRecipients {
                        principals: vec!["fp:operator".to_owned()],
                        capabilities: Vec::new(),
                    },
                })
                .collect(),
        }
    }

    fn execution(passthrough: &[&str]) -> NodeExecutionAdmissionPolicy {
        NodeExecutionAdmissionPolicy {
            schema: 4,
            max_live_fanout: 8,
            max_private_materialization_copy_bytes: 1,
            host_env_passthrough: passthrough.iter().map(|name| (*name).to_owned()).collect(),
            workload_client: None,
            resource_authority: None,
            producer_resource_ceiling: None,
            external_lease_budget: None,
        }
    }

    #[test]
    fn declared_names_cannot_reach_children_outside_delivery() {
        validate_declaration(&declaration(&["OPENAI_API_KEY"]), &execution(&[])).unwrap();
        assert!(
            validate_declaration(
                &declaration(&["SERVICE_TOKEN"]),
                &execution(&["SERVICE_TOKEN"])
            )
            .is_err()
        );
        for protected in ["TZ", "HTTPS_PROXY", "RUST_LOG", "RYEOS_TOKEN"] {
            assert!(
                validate_declaration(&declaration(&[protected]), &execution(&[])).is_err(),
                "{protected}"
            );
        }
    }

    #[test]
    fn bootstrap_refuses_a_different_generation_or_root() {
        let taken = StartupProviderEnvironment {
            app_root: PathBuf::from("/node"),
            generation_digest: "a".repeat(64),
            environment: Arc::new(ProviderEnvironment::none()),
        };
        taken
            .require_generation(Path::new("/node"), &"a".repeat(64))
            .unwrap();
        let error = taken
            .require_generation(Path::new("/node"), &"b".repeat(64))
            .unwrap_err();
        assert!(error.to_string().contains("restart the node"), "{error:#}");
        assert!(
            taken
                .require_generation(Path::new("/other"), &"a".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn authored_profiles_declare_provider_environment_as_data() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let profile_dir = repository.join("bundles/.ai/node/init/profiles");
        let mut profiles = std::fs::read_dir(&profile_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        profiles.sort();
        assert_eq!(profiles.len(), 16, "update provider-environment profile expectations");
        let table = ryeos_node_runtime::node_policy::NodePolicyTable::new();
        for path in profiles {
            let profile_name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap()
                .to_owned();
            let raw = std::fs::read_to_string(&path).unwrap();
            let body = lillux::signature::strip_signature_lines(&raw);
            let profile: ryeos_node_runtime::node_policy::generation::NodeInitProfile =
                serde_yaml::from_str(&body).unwrap();
            let provider: ProviderEnvironmentPolicy =
                profile.typed_policy(&table, &path).unwrap();
            let execution: NodeExecutionAdmissionPolicy =
                profile.typed_policy(&table, &path).unwrap();
            validate_declaration(&provider, &execution)
                .unwrap_or_else(|error| panic!("{profile_name}: {error:#}"));
            let expected: &[&str] = match profile_name.as_str() {
                "f5r-controller" | "hosted-workflow" => &["OPENAI_API_KEY"],
                // Authority-class profiles never declare a provider source.
                "release-authority" | "central-host" | "standard-central-auth"
                | "bundle-source" => &[],
                "contained-workflow" | "core" | "core-knowledge" | "core-scheduling"
                | "core-workflow" | "development" | "full" | "hosted-node" | "local-inference"
                | "standard" => &[],
                _ => panic!("add an explicit provider-environment expectation for {profile_name}"),
            };
            assert_eq!(provider.declared_names(), expected, "{profile_name}");
            if matches!(profile_name.as_str(), "f5r-controller" | "hosted-workflow") {
                // The hosted workflow delivers its key only to holders of
                // the dedicated receive capability.
                let recipients = &provider.names[0].recipients;
                assert!(recipients.principals.is_empty(), "{profile_name}");
                assert_eq!(
                    recipients.capabilities,
                    ["ryeos.receive.provider_env.openai_api_key"],
                    "{profile_name}"
                );
            }
        }
    }
}
