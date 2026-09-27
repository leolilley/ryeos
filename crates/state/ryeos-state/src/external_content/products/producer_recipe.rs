//! Signed Config data describing a finite producer invocation. This module
//! does not resolve, admit, or launch the selected verifier executable.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::objects::canonical_value_digest;

pub const PRODUCER_RECIPE_SCHEMA: &str = "ryeos.product_producer_recipe.v5";
pub const MAX_PRODUCER_RECIPE_BYTES: usize = 16 * 1024;
pub const MAX_PRODUCER_ARGV: usize = 32;
pub const MAX_PRODUCER_ARG_BYTES: usize = 1024;
// Structural parser limits, not the node's per-generation admission budget.
// The node policy may (and the contained-workflow profile does) impose much
// tighter ceilings. These maxima keep a signed producer recipe finite even
// before a particular node policy has been selected, and prevent arithmetic
// overflow or effectively unbounded output retention/CAS publication.
pub const MAX_PRODUCER_WALL_TIME_MS: u64 = 60 * 60 * 1_000;
pub const MAX_PRODUCER_STREAM_BYTES: u64 = 8 * 1024 * 1024;
// The current qualification-child observation returns both streams in one
// bounded control envelope. A future chunked CAS result transport may
// supersede this cap, but signed recipes cannot promise an undeliverable
// response under the present protocol.
pub const MAX_PRODUCER_COMBINED_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_PRODUCER_MEMORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const MAX_PRODUCER_PROCESSES: u32 = 256;
pub const MAX_PRODUCER_INTERACTIVE_FRAME_BYTES: u32 = 64 * 1024;
pub const MAX_PRODUCER_INTERACTIVE_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_PRODUCER_INTERACTIVE_FRAMES: u32 = 512;
pub const MAX_PRODUCER_ENVIRONMENT_BINDINGS: usize = 16;
/// A recipe can reference one additional prepared CWD not named by any
/// environment binding.
pub const MAX_PRODUCER_PREPARED_DIRECTORIES: usize = MAX_PRODUCER_ENVIRONMENT_BINDINGS + 1;
pub const MAX_PRODUCER_PREPARED_IMMUTABLE_FILES: usize = 8;
pub const MAX_PRODUCER_PREPARED_IMMUTABLE_FILE_BYTES: u64 = 1024 * 1024;

/// A signed executable selector, never a caller-supplied host path. The
/// realization member still requires the root's retained admitted mount and
/// the isolation engine's descriptor promotion before launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProducerExecutableSource {
    AdmittedVerifierExecutable,
    AdmittedRealizationMember {
        realization_id: String,
        manifest_hash: String,
        relative_path: String,
        executable_sha256: String,
    },
}

/// Input shape is selected by the signed recipe, not inferred from a callback
/// or from the presence of a live channel. Both variants have finite budgets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProducerStdinSource {
    SignedVerifierParameters,
    InteractiveVerifierChannel {
        maximum_frame_bytes: u32,
        maximum_total_bytes: u64,
        maximum_frames: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProducerCwdSource {
    VerifierPrivateWorkspace,
    PreparedDirectory { id: String },
}

/// Path-free signed process environment. Prepared directories must be
/// resolved from retained pinned launch authority; a recipe alone never
/// authorizes the daemon to open a host path or synthesize a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProducerEnvironmentBinding {
    Literal { value: String },
    VerifierPrivateWorkspace,
    PreparedDirectory { id: String },
}

fn validate_prepared_directory_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        bail!("producer prepared directory id is not canonical");
    }
    Ok(())
}

/// Fixed isolated-namespace coordinate for one signed logical directory ID.
/// This is not a host path or preparation authority: the launch must still
/// supply a retained directory descriptor and prove its exact mount.
pub fn prepared_directory_mount_destination(id: &str) -> anyhow::Result<std::path::PathBuf> {
    validate_prepared_directory_id(id)?;
    Ok(std::path::Path::new("/ryeos/producer-prepared").join(id))
}

/// One sealed file placed above a writable prepared directory. The signed
/// recipe names only a direct leaf, never a caller-selected namespace path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerPreparedImmutableFile {
    pub prepared_directory_id: String,
    pub leaf_name: String,
    pub maximum_bytes: u64,
    pub expected_sha256: String,
}

impl ProducerPreparedImmutableFile {
    pub fn destination(&self) -> anyhow::Result<std::path::PathBuf> {
        self.validate()?;
        Ok(
            prepared_directory_mount_destination(&self.prepared_directory_id)?
                .join(&self.leaf_name),
        )
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_prepared_directory_id(&self.prepared_directory_id)?;
        if self.leaf_name.is_empty()
            || self.leaf_name.len() > 128
            || self.leaf_name == "."
            || self.leaf_name == ".."
            || !self
                .leaf_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            bail!("producer immutable file leaf is not canonical");
        }
        if !(1..=MAX_PRODUCER_PREPARED_IMMUTABLE_FILE_BYTES).contains(&self.maximum_bytes) {
            bail!("producer immutable file byte bound is invalid");
        }
        if self.expected_sha256.len() != 64
            || !self
                .expected_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("producer immutable file expected content hash is not canonical");
        }
        Ok(())
    }
}

fn validate_producer_environment_name(name: &str) -> anyhow::Result<()> {
    let mut bytes = name.bytes();
    if name.is_empty()
        || name.len() > 64
        || !bytes.next().is_some_and(|byte| byte.is_ascii_uppercase())
        || !bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        || name == "RYEOS_EXTERNAL_REALIZATIONS"
    {
        bail!("producer environment name is not canonical or is reserved");
    }
    Ok(())
}

/// No host environment, caller value, or path lookup is an environment source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerEnvironmentSource {
    AdmittedRealizations,
}

/// One exact ingress origin for an interactive producer in its otherwise
/// isolated network namespace. This is not host-network or egress authority.
/// The daemon owns the target launch and authenticates a one-shot listener
/// handoff; the independent verifier owns the scripted provider relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerLoopbackIngress {
    pub address: String,
}

impl ProducerLoopbackIngress {
    pub fn validate(&self) -> anyhow::Result<()> {
        let address: std::net::SocketAddr = self
            .address
            .parse()
            .context("parse producer loopback address")?;
        if address.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            || address.port() == 0
            || self.address != address.to_string()
        {
            bail!(
                "producer loopback ingress requires canonical 127.0.0.1 and a fixed nonzero port"
            );
        }
        Ok(())
    }
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerResourceBounds {
    pub maximum_wall_time_ms: u64,
    pub maximum_stdout_bytes: u64,
    pub maximum_stderr_bytes: u64,
    pub maximum_memory_bytes: u64,
    pub maximum_processes: u32,
}

impl ProducerResourceBounds {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.maximum_wall_time_ms == 0
            || self.maximum_stdout_bytes == 0
            || self.maximum_stderr_bytes == 0
            || self.maximum_memory_bytes == 0
            || self.maximum_processes == 0
        {
            bail!("producer resource bounds must be positive");
        }
        if self.maximum_wall_time_ms > MAX_PRODUCER_WALL_TIME_MS
            || self.maximum_stdout_bytes > MAX_PRODUCER_STREAM_BYTES
            || self.maximum_stderr_bytes > MAX_PRODUCER_STREAM_BYTES
            || self
                .maximum_stdout_bytes
                .checked_add(self.maximum_stderr_bytes)
                .is_none_or(|total| total > MAX_PRODUCER_COMBINED_OUTPUT_BYTES)
            || self.maximum_memory_bytes > MAX_PRODUCER_MEMORY_BYTES
            || self.maximum_processes > MAX_PRODUCER_PROCESSES
        {
            bail!("producer resource bounds exceed structural maximum");
        }
        Ok(())
    }
}

/// A fixed, literal argv vector. No string is interpreted as a shell command,
/// template, variable, or executable path. `--scenario-driver` is ordinary data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductProducerRecipe {
    pub schema: String,
    pub executable_source: ProducerExecutableSource,
    pub argv: Vec<String>,
    pub stdin_source: ProducerStdinSource,
    pub cwd_source: ProducerCwdSource,
    pub environment_sources: Vec<ProducerEnvironmentSource>,
    pub environment_bindings: BTreeMap<String, ProducerEnvironmentBinding>,
    pub prepared_immutable_files: Vec<ProducerPreparedImmutableFile>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub loopback_ingress: Option<ProducerLoopbackIngress>,
    pub bounds: ProducerResourceBounds,
}

impl ProductProducerRecipe {
    pub fn from_value(value: Value) -> anyhow::Result<Self> {
        if lillux::canonical_json(&value)?.len() > MAX_PRODUCER_RECIPE_BYTES {
            bail!("producer recipe exceeds byte bound");
        }
        let recipe: Self = serde_json::from_value(value).context("decode producer recipe")?;
        recipe.validate()?;
        Ok(recipe)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCER_RECIPE_SCHEMA {
            bail!("unsupported producer recipe schema");
        }
        if self.argv.len() > MAX_PRODUCER_ARGV {
            bail!("producer argv exceeds count bound");
        }
        for arg in &self.argv {
            if arg.len() > MAX_PRODUCER_ARG_BYTES || arg.contains('\0') {
                bail!("producer argv contains an invalid literal");
            }
        }
        if self.environment_sources.len() > 1
            || self
                .environment_sources
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != self.environment_sources.len()
        {
            bail!("producer environment sources must be finite and unique");
        }
        if self.environment_bindings.len() > MAX_PRODUCER_ENVIRONMENT_BINDINGS {
            bail!("producer environment bindings exceed count bound");
        }
        if self.prepared_immutable_files.len() > MAX_PRODUCER_PREPARED_IMMUTABLE_FILES {
            bail!("producer immutable file count exceeds bound");
        }
        if let ProducerCwdSource::PreparedDirectory { id } = &self.cwd_source {
            validate_prepared_directory_id(id)?;
        }
        for (name, binding) in &self.environment_bindings {
            validate_producer_environment_name(name)?;
            match binding {
                ProducerEnvironmentBinding::Literal { value } => {
                    if value.len() > 4096
                        || value.chars().any(char::is_control)
                        || value.contains('/')
                        || value.contains('\\')
                    {
                        bail!("producer environment literal is not bounded or path-free");
                    }
                }
                ProducerEnvironmentBinding::VerifierPrivateWorkspace => {}
                ProducerEnvironmentBinding::PreparedDirectory { id } => {
                    validate_prepared_directory_id(id)?;
                }
            }
        }
        let mut prepared_ids = BTreeSet::new();
        if let ProducerCwdSource::PreparedDirectory { id } = &self.cwd_source {
            prepared_ids.insert(id.as_str());
        }
        for binding in self.environment_bindings.values() {
            if let ProducerEnvironmentBinding::PreparedDirectory { id } = binding {
                prepared_ids.insert(id.as_str());
            }
        }
        let mut immutable_destinations = BTreeSet::new();
        for file in &self.prepared_immutable_files {
            file.validate()?;
            if !prepared_ids.contains(file.prepared_directory_id.as_str()) {
                bail!("producer immutable file requires a used prepared directory");
            }
            if !immutable_destinations
                .insert((file.prepared_directory_id.as_str(), file.leaf_name.as_str()))
            {
                bail!("producer immutable file destination is duplicated");
            }
        }
        if let ProducerExecutableSource::AdmittedRealizationMember {
            realization_id,
            manifest_hash,
            relative_path,
            executable_sha256,
        } = &self.executable_source
        {
            if realization_id.is_empty()
                || realization_id.len() > 64
                || !realization_id.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
            {
                bail!("producer realization id is not canonical");
            }
            crate::objects::validate_canonical_project_relative_path(relative_path)?;
            if relative_path.len() > 1024
                || ![manifest_hash, executable_sha256].into_iter().all(|hash| {
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
            {
                bail!("producer realization member identity is not canonical");
            }
        }
        if let ProducerStdinSource::InteractiveVerifierChannel {
            maximum_frame_bytes,
            maximum_total_bytes,
            maximum_frames,
        } = &self.stdin_source
        {
            if !(1..=MAX_PRODUCER_INTERACTIVE_FRAME_BYTES).contains(maximum_frame_bytes)
                || !(1..=MAX_PRODUCER_INTERACTIVE_TOTAL_BYTES).contains(maximum_total_bytes)
                || !(1..=MAX_PRODUCER_INTERACTIVE_FRAMES).contains(maximum_frames)
                || u64::from(*maximum_frame_bytes) > *maximum_total_bytes
            {
                bail!("producer interactive input exceeds signed structural bounds");
            }
        }
        if let Some(ingress) = &self.loopback_ingress {
            ingress.validate()?;
            if !matches!(
                self.executable_source,
                ProducerExecutableSource::AdmittedRealizationMember { .. }
            ) || !matches!(
                self.stdin_source,
                ProducerStdinSource::InteractiveVerifierChannel { .. }
            ) {
                bail!(
                    "producer loopback ingress requires an interactive admitted realization member"
                );
            }
        }
        self.bounds.validate()?;
        if lillux::canonical_json(&serde_json::to_value(self)?)?.len() > MAX_PRODUCER_RECIPE_BYTES {
            bail!("producer recipe exceeds byte bound");
        }
        Ok(())
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::to_value(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid() -> Value {
        json!({"schema": PRODUCER_RECIPE_SCHEMA,
            "executable_source": {"kind":"admitted_verifier_executable"},
            "argv": ["--scenario-driver", "check"],
            "stdin_source": {"kind":"signed_verifier_parameters"},
            "cwd_source": {"kind":"verifier_private_workspace"},
            "environment_sources": ["admitted_realizations"],
            "environment_bindings": {},
            "prepared_immutable_files": [],
            "loopback_ingress": null,
            "bounds": {"maximum_wall_time_ms": 1000, "maximum_stdout_bytes": 1024,
                "maximum_stderr_bytes": 1024, "maximum_memory_bytes": 1048576,
                "maximum_processes": 1}})
    }

    #[test]
    fn literal_recipe_is_canonical() {
        let recipe = ProductProducerRecipe::from_value(valid()).unwrap();
        assert_eq!(
            recipe.digest().unwrap(),
            canonical_value_digest(&valid()).unwrap()
        );
    }

    #[test]
    fn prepared_environment_bindings_are_path_free_and_exact() {
        let mut value = valid();
        value["cwd_source"] = json!({"kind":"prepared_directory","id":"guest-cwd"});
        value["environment_bindings"] = json!({
            "CODEX_HOME":{"kind":"prepared_directory","id":"codex-home"},
            "HOME":{"kind":"prepared_directory","id":"codex-home"},
            "PATH":{"kind":"literal","value":""},
            "LANG":{"kind":"literal","value":"C"}
        });
        let recipe = ProductProducerRecipe::from_value(value.clone()).unwrap();
        assert_eq!(
            recipe.digest().unwrap(),
            canonical_value_digest(&value).unwrap()
        );

        let mut invalid = value.clone();
        invalid["environment_bindings"]["HOME"] = json!({"kind":"literal","value":"/home/ambient"});
        assert!(ProductProducerRecipe::from_value(invalid).is_err());
        let mut invalid = value.clone();
        invalid["environment_bindings"]["RYEOS_EXTERNAL_REALIZATIONS"] =
            json!({"kind":"literal","value":"override"});
        assert!(ProductProducerRecipe::from_value(invalid).is_err());
        let mut invalid = value.clone();
        invalid["environment_bindings"]["HOME"] =
            json!({"kind":"prepared_directory","id":"../ambient"});
        assert!(ProductProducerRecipe::from_value(invalid).is_err());
        let mut invalid = value;
        invalid["cwd_source"] = json!({"kind":"prepared_directory","id":""});
        assert!(ProductProducerRecipe::from_value(invalid).is_err());
        assert_eq!(
            prepared_directory_mount_destination("codex-home")
                .unwrap()
                .to_str(),
            Some("/ryeos/producer-prepared/codex-home")
        );
        assert!(prepared_directory_mount_destination("../ambient").is_err());
    }

    #[test]
    fn immutable_prepared_files_are_bounded_direct_leaves_of_used_directories() {
        let mut value = valid();
        value["environment_bindings"] = json!({
            "CODEX_HOME":{"kind":"prepared_directory","id":"codex-home"}
        });
        value["prepared_immutable_files"] = json!([
            {"prepared_directory_id":"codex-home","leaf_name":"config.toml","maximum_bytes":65536,"expected_sha256":"a".repeat(64)},
            {"prepared_directory_id":"codex-home","leaf_name":"environments.toml","maximum_bytes":65536,"expected_sha256":"b".repeat(64)}
        ]);
        let recipe = ProductProducerRecipe::from_value(value.clone()).unwrap();
        assert_eq!(
            recipe.prepared_immutable_files[0]
                .destination()
                .unwrap()
                .to_str(),
            Some("/ryeos/producer-prepared/codex-home/config.toml")
        );
        for (field, bad) in [
            ("prepared_directory_id", json!("unbound")),
            ("leaf_name", json!("../config.toml")),
            ("leaf_name", json!(".config/config.toml")),
            ("leaf_name", json!("..")),
            ("maximum_bytes", json!(0)),
            (
                "maximum_bytes",
                json!(MAX_PRODUCER_PREPARED_IMMUTABLE_FILE_BYTES + 1),
            ),
            ("expected_sha256", json!("A".repeat(64))),
        ] {
            let mut invalid = value.clone();
            invalid["prepared_immutable_files"][0][field] = bad;
            assert!(
                ProductProducerRecipe::from_value(invalid).is_err(),
                "{field}"
            );
        }
        let mut duplicate = value.clone();
        duplicate["prepared_immutable_files"][1] = duplicate["prepared_immutable_files"][0].clone();
        assert!(ProductProducerRecipe::from_value(duplicate).is_err());
        let mut too_many = value;
        too_many["prepared_immutable_files"] = json!(
            (0..=MAX_PRODUCER_PREPARED_IMMUTABLE_FILES)
                .map(|index| json!({"prepared_directory_id":"codex-home",
                "leaf_name":format!("file-{index}"),"maximum_bytes":1,
                "expected_sha256":"a".repeat(64)}))
                .collect::<Vec<_>>()
        );
        assert!(ProductProducerRecipe::from_value(too_many).is_err());
    }

    #[test]
    fn authority_fields_are_required_and_closed() {
        for field in [
            "executable_source",
            "argv",
            "stdin_source",
            "cwd_source",
            "environment_sources",
            "environment_bindings",
            "prepared_immutable_files",
            "loopback_ingress",
            "bounds",
        ] {
            let mut value = valid();
            value.as_object_mut().unwrap().remove(field);
            assert!(ProductProducerRecipe::from_value(value).is_err(), "{field}");
        }
        for (path, value) in [
            (
                "executable_source",
                json!({"kind":"host_path","path":"/bin/sh"}),
            ),
            ("stdin_source", json!({"kind":"caller_input"})),
            ("cwd_source", json!({"kind":"host_workspace"})),
            ("environment_sources", json!(["host_environment"])),
        ] {
            let mut candidate = valid();
            candidate[path] = value;
            assert!(
                ProductProducerRecipe::from_value(candidate).is_err(),
                "{path}"
            );
        }
        let mut candidate = valid();
        candidate["callback"] = json!("override");
        assert!(ProductProducerRecipe::from_value(candidate).is_err());
    }

    #[test]
    fn resource_bounds_are_rejected_but_arguments_remain_literal() {
        let mut candidate = valid();
        candidate["argv"] = json!(["${HOME}"]);
        let recipe = ProductProducerRecipe::from_value(candidate).unwrap();
        assert_eq!(recipe.argv, ["${HOME}"]);
        let mut candidate = valid();
        candidate["bounds"]["maximum_wall_time_ms"] = json!(0);
        assert!(ProductProducerRecipe::from_value(candidate).is_err());
    }

    #[test]
    fn structural_resource_caps_reject_effectively_unbounded_signed_recipes() {
        for (field, over) in [
            ("maximum_wall_time_ms", MAX_PRODUCER_WALL_TIME_MS + 1),
            ("maximum_stdout_bytes", MAX_PRODUCER_STREAM_BYTES + 1),
            ("maximum_stderr_bytes", MAX_PRODUCER_STREAM_BYTES + 1),
            ("maximum_memory_bytes", MAX_PRODUCER_MEMORY_BYTES + 1),
            ("maximum_processes", u64::from(MAX_PRODUCER_PROCESSES) + 1),
        ] {
            let mut candidate = valid();
            candidate["bounds"][field] = json!(over);
            assert!(
                ProductProducerRecipe::from_value(candidate).is_err(),
                "{field}"
            );
        }
        let mut combined = valid();
        combined["bounds"]["maximum_stdout_bytes"] = json!(MAX_PRODUCER_STREAM_BYTES);
        combined["bounds"]["maximum_stderr_bytes"] = json!(1);
        assert!(ProductProducerRecipe::from_value(combined).is_err());
    }

    #[test]
    fn admitted_interactive_member_is_exact_and_bounded() {
        let mut value = valid();
        value["executable_source"] = json!({
            "kind":"admitted_realization_member",
            "realization_id":"subject",
            "manifest_hash":"a".repeat(64),
            "relative_path":"bin/codex",
            "executable_sha256":"b".repeat(64)
        });
        value["stdin_source"] = json!({
            "kind":"interactive_verifier_channel",
            "maximum_frame_bytes":4096,
            "maximum_total_bytes":65536,
            "maximum_frames":16
        });
        let admitted = ProductProducerRecipe::from_value(value.clone()).unwrap();
        assert_eq!(
            admitted.digest().unwrap(),
            canonical_value_digest(&value).unwrap()
        );
        value["loopback_ingress"] = json!({"address":"127.0.0.1:18765"});
        assert!(ProductProducerRecipe::from_value(value.clone()).is_ok());
        for address in [
            "localhost:18765",
            "127.0.0.1:0",
            "127.0.0.2:18765",
            "0.0.0.0:18765",
            "[::1]:18765",
        ] {
            let mut changed = value.clone();
            changed["loopback_ingress"]["address"] = json!(address);
            assert!(
                ProductProducerRecipe::from_value(changed).is_err(),
                "{address}"
            );
        }
        let mut wrong_mode = valid();
        wrong_mode["loopback_ingress"] = json!({"address":"127.0.0.1:18765"});
        assert!(ProductProducerRecipe::from_value(wrong_mode).is_err());
        for (field, bad) in [
            ("realization_id", json!("../subject")),
            ("manifest_hash", json!("A".repeat(64))),
            ("relative_path", json!("../bin/codex")),
            ("executable_sha256", json!("0")),
        ] {
            let mut changed = value.clone();
            changed["executable_source"][field] = bad;
            assert!(
                ProductProducerRecipe::from_value(changed).is_err(),
                "{field}"
            );
        }
        for (field, bad) in [
            ("maximum_frame_bytes", json!(0)),
            (
                "maximum_frame_bytes",
                json!(MAX_PRODUCER_INTERACTIVE_FRAME_BYTES + 1),
            ),
            ("maximum_total_bytes", json!(4095)),
            ("maximum_frames", json!(MAX_PRODUCER_INTERACTIVE_FRAMES + 1)),
        ] {
            let mut changed = value.clone();
            changed["stdin_source"][field] = bad;
            assert!(
                ProductProducerRecipe::from_value(changed).is_err(),
                "{field}"
            );
        }
        let mut old = valid();
        old["schema"] = json!("ryeos.product_producer_recipe.v1");
        assert!(ProductProducerRecipe::from_value(old).is_err());
    }
}
