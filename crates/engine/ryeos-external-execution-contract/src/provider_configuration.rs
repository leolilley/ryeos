//! Finite public routing facts accompany private rendered configuration bytes.
//! These are adapter testimony, not proof that a workload consumed the bytes or
//! that its complete effective tool inventory is confined to this environment.

use std::io::Write as _;

use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use crate::MAX_PROVIDER_CONFIGURATION_BYTES;

pub const PROVIDER_CONFIGURATION_RESPONSE_SCHEMA: u32 = 1;
pub const PROVIDER_CONFIGURATION_RESPONSE_PROTOCOL: &str =
    "ryeos.external-execution.provider-configuration-response.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCommandEnvironmentTransport {
    CommandConnector,
}

/// One environment by construction; this is not an extensible route predicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCommandEnvironmentProjection {
    pub environment_id: String,
    pub default_environment_id: String,
    pub include_local: bool,
    pub transport: ProviderCommandEnvironmentTransport,
}

impl ProviderCommandEnvironmentProjection {
    pub fn validate(&self) -> Result<()> {
        crate::bounded_identifier(&self.environment_id, 128, "command environment id")?;
        ensure!(
            self.default_environment_id == self.environment_id && !self.include_local,
            "command environment must be the sole default without local fallback"
        );
        Ok(())
    }
}

/// Public controller-observed launch input. This value alone conveys no process
/// authority and no claim that the workload consumed its configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfigurationLaunchInput {
    pub schema: u32,
    pub configuration_sha256: String,
    pub configuration_bytes: u64,
    pub command_environment: ProviderCommandEnvironmentProjection,
    pub provider_declaration_id: String,
    pub configuration_destination: String,
    pub configuration_adapter_artifact_hash: String,
    pub configuration_adapter_artifact_bytes: u64,
}

impl ProviderConfigurationLaunchInput {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported provider configuration launch input"
        );
        crate::digest(&self.configuration_sha256, "provider configuration")?;
        crate::digest(
            &self.configuration_adapter_artifact_hash,
            "provider configuration adapter",
        )?;
        ensure!(
            (1..=MAX_PROVIDER_CONFIGURATION_BYTES as u64).contains(&self.configuration_bytes),
            "provider configuration launch input size is invalid"
        );
        ensure!(
            self.configuration_adapter_artifact_bytes > 0,
            "provider configuration adapter is empty"
        );
        crate::bounded_identifier(
            &self.provider_declaration_id,
            128,
            "provider declaration id",
        )?;
        crate::validate_single_file_name(
            &self.configuration_destination,
            "external provider configuration destination",
        )?;
        self.command_environment.validate()
    }
}

/// No Debug or public secret field. Private storage and encoded output are
/// zeroized on drop, including errors after the secret field is deserialized.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfigurationResponse {
    schema: u32,
    protocol: String,
    #[serde(
        serialize_with = "serialize_secret",
        deserialize_with = "deserialize_secret"
    )]
    configuration: Zeroizing<String>,
    command_environment: ProviderCommandEnvironmentProjection,
}

fn serialize_secret<S: Serializer>(
    value: &Zeroizing<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(value.as_str())
}

fn deserialize_secret<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Zeroizing<String>, D::Error> {
    String::deserialize(deserializer).map(Zeroizing::new)
}

impl ProviderConfigurationResponse {
    pub fn new(
        configuration: String,
        command_environment: ProviderCommandEnvironmentProjection,
    ) -> Result<Self> {
        let response = Self {
            schema: PROVIDER_CONFIGURATION_RESPONSE_SCHEMA,
            protocol: PROVIDER_CONFIGURATION_RESPONSE_PROTOCOL.into(),
            configuration: Zeroizing::new(configuration),
            command_environment,
        };
        response.validate()?;
        // The encoded envelope, including JSON escaping, shares the existing
        // stdout ceiling. A configuration that only fits unescaped is refused.
        response.canonical_bytes()?;
        Ok(response)
    }

    pub fn configuration(&self) -> &str {
        self.configuration.as_str()
    }

    pub fn command_environment(&self) -> &ProviderCommandEnvironmentProjection {
        &self.command_environment
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == PROVIDER_CONFIGURATION_RESPONSE_SCHEMA
                && self.protocol == PROVIDER_CONFIGURATION_RESPONSE_PROTOCOL,
            "unsupported provider configuration response"
        );
        ensure!(
            !self.configuration.is_empty()
                && self.configuration.len() <= MAX_PROVIDER_CONFIGURATION_BYTES,
            "provider configuration exceeds its byte bound"
        );
        self.command_environment.validate()
    }

    /// Closed typed decoding rejects duplicate and unknown fields without
    /// constructing an intermediate JSON Value containing private strings.
    /// Errors deliberately exclude input fragments and parser diagnostics.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_PROVIDER_CONFIGURATION_BYTES,
            "provider configuration response exceeds its byte bound"
        );
        let response: Self = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("invalid provider configuration response"))?;
        response.validate()?;
        ensure!(
            response.canonical_bytes()?.as_slice() == bytes,
            "provider configuration response is not canonical"
        );
        Ok(response)
    }

    pub fn canonical_bytes(&self) -> Result<Zeroizing<Vec<u8>>> {
        self.validate()?;
        struct BoundedOutput(Zeroizing<Vec<u8>>);
        impl std::io::Write for BoundedOutput {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > MAX_PROVIDER_CONFIGURATION_BYTES.saturating_sub(self.0.len()) {
                    return Err(std::io::Error::other(
                        "provider configuration envelope exceeds bound",
                    ));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // Reserve once so reallocations cannot abandon earlier secret buffers.
        let mut output = BoundedOutput(Zeroizing::new(Vec::with_capacity(
            MAX_PROVIDER_CONFIGURATION_BYTES,
        )));
        serde_json::to_writer(&mut output, self).map_err(|_| {
            anyhow::anyhow!("provider configuration envelope exceeds its byte bound")
        })?;
        output.flush()?;
        Ok(output.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> ProviderCommandEnvironmentProjection {
        ProviderCommandEnvironmentProjection {
            environment_id: "generic-command".into(),
            default_environment_id: "generic-command".into(),
            include_local: false,
            transport: ProviderCommandEnvironmentTransport::CommandConnector,
        }
    }

    #[test]
    fn exact_secret_bytes_and_finite_projection_round_trip() {
        let secret = "token = \"private\\value\"\n# λ\n";
        let response = ProviderConfigurationResponse::new(secret.into(), projection()).unwrap();
        let encoded = response.canonical_bytes().unwrap();
        let decoded = ProviderConfigurationResponse::decode(&encoded).unwrap();
        assert_eq!(decoded.configuration(), secret);
        assert_eq!(decoded.command_environment(), &projection());
    }

    #[test]
    fn closed_current_envelope_refuses_substitution_and_private_diagnostics() {
        let response =
            ProviderConfigurationResponse::new("private-marker".into(), projection()).unwrap();
        let encoded = response.canonical_bytes().unwrap();
        let original: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        for (key, value) in [
            ("schema", serde_json::json!(0)),
            ("protocol", serde_json::json!("old")),
            ("configuration", serde_json::json!("")),
            ("configuration_hash", serde_json::json!("invented")),
        ] {
            let mut changed = original.clone();
            changed[key] = value;
            assert!(
                ProviderConfigurationResponse::decode(&serde_json::to_vec(&changed).unwrap())
                    .is_err()
            );
        }
        for (key, value) in [
            ("include_local", serde_json::json!(true)),
            ("default_environment_id", serde_json::json!("other")),
            ("transport", serde_json::json!("private-marker")),
            ("extra", serde_json::json!(true)),
        ] {
            let mut changed = original.clone();
            changed["command_environment"][key] = value;
            let error =
                ProviderConfigurationResponse::decode(&serde_json::to_vec(&changed).unwrap())
                    .err()
                    .unwrap();
            assert!(!error.to_string().contains("private-marker"));
        }
        let duplicate =
            String::from_utf8(encoded.to_vec())
                .unwrap()
                .replacen("{", "{\"schema\":1,", 1);
        assert!(ProviderConfigurationResponse::decode(duplicate.as_bytes()).is_err());
        assert!(ProviderConfigurationResponse::decode(b"old plain TOML").is_err());
        assert!(
            ProviderConfigurationResponse::decode(&vec![
                b'x';
                MAX_PROVIDER_CONFIGURATION_BYTES + 1
            ])
            .is_err()
        );
    }

    #[test]
    fn launch_input_requires_exact_bounded_public_identities() {
        let input = ProviderConfigurationLaunchInput {
            schema: 1,
            configuration_sha256: "a".repeat(64),
            configuration_bytes: 10,
            command_environment: projection(),
            provider_declaration_id: "generic-provider".into(),
            configuration_destination: "environments.toml".into(),
            configuration_adapter_artifact_hash: "b".repeat(64),
            configuration_adapter_artifact_bytes: 100,
        };
        input.validate().unwrap();
        let mut changed = input.clone();
        changed.configuration_destination = "../environments.toml".into();
        assert!(changed.validate().is_err());
        changed = input.clone();
        changed.configuration_bytes = MAX_PROVIDER_CONFIGURATION_BYTES as u64 + 1;
        assert!(changed.validate().is_err());
        changed = input.clone();
        changed.configuration_adapter_artifact_hash = "B".repeat(64);
        assert!(changed.validate().is_err());
        changed = input.clone();
        changed.configuration_adapter_artifact_bytes = 0;
        assert!(changed.validate().is_err());
        changed = input;
        changed.command_environment.include_local = true;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn escaped_envelope_must_fit_the_existing_stdout_ceiling() {
        assert!(
            ProviderConfigurationResponse::new(
                "\\".repeat(MAX_PROVIDER_CONFIGURATION_BYTES / 2),
                projection()
            )
            .is_err()
        );
        let response = ProviderConfigurationResponse::new(
            "x".repeat(MAX_PROVIDER_CONFIGURATION_BYTES - 1024),
            projection(),
        )
        .unwrap();
        assert!(response.canonical_bytes().unwrap().len() <= MAX_PROVIDER_CONFIGURATION_BYTES);
    }
}
