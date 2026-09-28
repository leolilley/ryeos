//! Exact signed serializer for Codex's command-backed environment contract.
//!
//! The controller supplies one sealed occurrence-private request selecting
//! either an admitted mount or an inherited connector descriptor. This adapter owns
//! only Codex syntax; it cannot choose a command, destination, endpoint, or
//! local fallback policy.

use std::collections::BTreeMap;
use std::io::Write as _;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::{
    MAX_PROVIDER_CONFIGURATION_BYTES, MAX_PROVIDER_CONFIGURATION_REQUEST_BYTES,
    ProviderConfigurationRequest, ProviderConnectorExecutable, from_json_slice_strict,
};
use serde::Serialize;

const ENVIRONMENT_ID: &str = "ryeos-external-candidate";

#[derive(Serialize)]
struct CodexEnvironmentsToml<'a> {
    default: &'a str,
    include_local: bool,
    environments: Vec<CodexCommandEnvironment<'a>>,
}

#[derive(Serialize)]
struct CodexCommandEnvironment<'a> {
    id: &'a str,
    program: String,
    env: BTreeMap<&'static str, &'a str>,
}

fn main() -> Result<()> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let operation = arguments
        .next()
        .context("Codex configuration adapter requires an operation")?;
    ensure!(
        operation == "render" && arguments.next().is_none(),
        "Codex configuration adapter accepts only `render`"
    );
    // SAFETY: the controller installs this one sealed request descriptor
    // exactly once for the dedicated adapter process.
    let bytes = unsafe {
        lillux::read_sealed_inherited_descriptor_from_env(
            "RYEOS_PROVIDER_CONFIGURATION_REQUEST_FD",
            MAX_PROVIDER_CONFIGURATION_REQUEST_BYTES,
        )
    }
    .map_err(anyhow::Error::msg)
    .context("read sealed provider configuration request")?;
    let request: ProviderConfigurationRequest =
        from_json_slice_strict(&bytes, MAX_PROVIDER_CONFIGURATION_REQUEST_BYTES)
            .context("decode strict provider configuration request")?;
    let configuration = render(&request)?;
    std::io::stdout()
        .lock()
        .write_all(configuration.as_bytes())
        .context("write Codex provider configuration")
}

fn render(request: &ProviderConfigurationRequest) -> Result<String> {
    request.validate()?;
    let connector_program = match &request.connector_executable {
        ProviderConnectorExecutable::InheritedDescriptor { descriptor } => {
            lillux::inherited_executable_path(*descriptor).map_err(anyhow::Error::msg)?
        }
        ProviderConnectorExecutable::NamespacePath { path } => path.into(),
    };
    let connector_program = connector_program
        .to_str()
        .context("selected connector executable path is not UTF-8")?;
    ensure!(
        connector_program.starts_with('/') && !connector_program.chars().any(char::is_control),
        "selected connector executable path is invalid"
    );

    let mut env = BTreeMap::new();
    env.insert(
        "RYEOS_EXTERNAL_CONNECTOR_ENDPOINT",
        request.connector_endpoint.as_str(),
    );
    env.insert(
        "RYEOS_EXTERNAL_CONNECTOR_PLACEMENT",
        request.placement_thread_id.as_str(),
    );
    env.insert(
        "RYEOS_EXTERNAL_CONNECTOR_BINDING",
        request.execution_binding_hash.as_str(),
    );
    env.insert(
        "RYEOS_EXTERNAL_CONNECTOR_CAPABILITY",
        request.connector_capability.as_str(),
    );
    let configuration = toml::to_string(&CodexEnvironmentsToml {
        default: ENVIRONMENT_ID,
        include_local: false,
        environments: vec![CodexCommandEnvironment {
            id: ENVIRONMENT_ID,
            program: connector_program.to_owned(),
            env,
        }],
    })
    .context("serialize closed Codex provider configuration")?;
    ensure!(
        !configuration.is_empty() && configuration.len() <= MAX_PROVIDER_CONFIGURATION_BYTES,
        "Codex provider configuration exceeds its byte bound"
    );
    Ok(configuration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::PROVIDER_CONFIGURATION_PROTOCOL;

    #[test]
    fn configuration_is_command_only_and_disables_local_fallback() {
        let mut request = ProviderConfigurationRequest {
            schema: 1,
            protocol: PROVIDER_CONFIGURATION_PROTOCOL.into(),
            provider_declaration_id: "codex-hosted".into(),
            connector_executable: ProviderConnectorExecutable::NamespacePath {
                path: "/ryeos/realizations/external-connector/connector".into(),
            },
            connector_endpoint: "/controller/connector.sock".into(),
            placement_thread_id: "T-fixture".into(),
            execution_binding_hash: "a".repeat(64),
            connector_capability: "private-capability".into(),
        };
        let encoded = render(&request).unwrap();
        let value: toml::Value = toml::from_str(&encoded).unwrap();
        assert_eq!(value["default"].as_str(), Some(ENVIRONMENT_ID));
        assert_eq!(value["include_local"].as_bool(), Some(false));
        let environments = value["environments"].as_array().unwrap();
        assert_eq!(environments.len(), 1);
        let environment = &environments[0];
        assert_eq!(environment["id"].as_str(), Some(ENVIRONMENT_ID));
        assert_eq!(
            environment["program"].as_str(),
            Some("/ryeos/realizations/external-connector/connector")
        );
        assert!(environment.get("url").is_none());
        assert!(environment.get("cwd").is_none());
        assert!(environment.get("args").is_none());
        let env = environment["env"].as_table().unwrap();
        assert_eq!(env.len(), 4);
        assert_eq!(
            env["RYEOS_EXTERNAL_CONNECTOR_CAPABILITY"].as_str(),
            Some("private-capability")
        );
        request.connector_executable = ProviderConnectorExecutable::NamespacePath {
            path: "relative/program".into(),
        };
        assert!(render(&request).is_err());
        request.connector_executable =
            ProviderConnectorExecutable::InheritedDescriptor { descriptor: 1 };
        assert!(render(&request).is_err());
        let executable =
            lillux::sealed_executable_memfd(c"configuration-test", b"fixture image").unwrap();
        request.connector_executable = ProviderConnectorExecutable::InheritedDescriptor {
            descriptor: executable.inherited_descriptor().unwrap(),
        };
        let descriptor_value: toml::Value = toml::from_str(&render(&request).unwrap()).unwrap();
        assert_eq!(
            descriptor_value["environments"][0]["program"].as_str(),
            executable.path().to_str()
        );
    }
}
