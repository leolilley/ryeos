//! Closed, data-only schema for this assignment's V3 GPU startup recipe.
//!
//! Values returned by this module are parsed data. They do not prove source
//! membership, signature trust, capsule identity, descriptor ownership,
//! funding, or permission to launch.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

pub const STARTUP_CONTRACT_V3_SCHEMA: &str = "ryeos.local_tinygrad_current_assignment_startup.v3";
pub const STARTUP_CONTRACT_V3_ROLE: &str = "current_assignment_gpu_occurrence_owner_v3";
pub const STARTUP_CONTRACT_V3_MAX_BYTES: usize = 16 * 1024;

const OWNER_BINARY: &str = "bin:core/ryeos-local-gpu-occurrence-owner";
const OWNER_ROLE: &str = "occurrence-owner";
const INNER_HOST_ROLE: &str = "inner-session-host";
const INNER_HOST_IMAGE: &str = "same-owner-binary";
const TARGET_KIND: &str = "sealed-private-source-v1";
const SOURCE_ROOT: &str = "source";
const PROJECT_ROOT: &str = "/ryeos/admitted-project";
const SOURCE_CONSUMER: &str = "bootstrap_support.py";
const GNU_LOADER: &str = "python-gnu/python/lib/ld-linux-x86-64.so.2";
const SESSION_ENV: &str = "RYEOS_SESSION_FD";
const RESOURCE_CLEANUP_ENV: &str = "RYEOS_RESOURCE_CLEANUP_FD";
const ORIGINAL_WINDOW_ENV: &str = "RYEOS_ORIGINAL_WINDOW_FD";
const ORIGINAL_CLOCK_NAMESPACE_ENV: &str = "RYEOS_ORIGINAL_CLOCK_NAMESPACE_FD";
const POLICY_SCHEMA: &str = "ryeos.local_tinygrad_nvml_assignment_policy.v1";
const GNU_PROVIDER_CHECKS: &str = "trusted-gnu-loader-provider-and-function-origin-v1";
const GNU_MANIFEST_DESTINATION: &str = "/opt/ryeos-original-manifests/gnu.json";
const DRIVER_MANIFEST_DESTINATION: &str = "/opt/ryeos-original-manifests/driver.json";

const INNER_ROOT_ROLES: [&str; 7] = [
    "python-gnu",
    "driver",
    "runtime",
    "tinygrad",
    "toolchain",
    "model",
    "worker-source",
];

/// Decode original bounded member bytes directly into the one closed V3 type.
/// The returned value is ordinary data and carries no source or launch authority.
pub fn decode_and_validate_v3(bytes: &[u8]) -> anyhow::Result<StartupContractV3> {
    ensure!(
        (1..=STARTUP_CONTRACT_V3_MAX_BYTES).contains(&bytes.len()),
        "startup V3 member is empty or exceeds its byte bound"
    );
    let contract: StartupContractV3 =
        serde_json::from_slice(bytes).context("decode closed startup V3 document")?;
    contract.validate()?;
    Ok(contract)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupContractV3 {
    pub schema: String,
    pub role: String,
    pub owner: StartupV3Owner,
    pub inner_host: StartupV3InnerHost,
    pub entry: StartupV3SourceMember,
    pub bootstrap_members: Vec<StartupV3SourceMember>,
    pub launch: StartupV3Launch,
    pub products: StartupV3Products,
    pub policy: StartupV3Policy,
    pub wire: StartupV3Wire,
    pub manifest_delivery: StartupV3ManifestDelivery,
}

impl StartupContractV3 {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.schema == STARTUP_CONTRACT_V3_SCHEMA,
            "startup V3 schema is unsupported"
        );
        ensure!(
            self.role == STARTUP_CONTRACT_V3_ROLE,
            "startup V3 role is unsupported"
        );
        self.owner.validate()?;
        self.inner_host.validate(&self.entry)?;
        self.validate_members()?;
        self.launch.validate()?;
        self.products.validate()?;
        self.policy.validate()?;
        self.wire.validate()?;
        self.manifest_delivery.validate(&self.bootstrap_members)?;
        Ok(())
    }

    fn validate_members(&self) -> anyhow::Result<()> {
        self.entry.validate()?;
        ensure!(
            (1..=64).contains(&self.bootstrap_members.len()),
            "startup V3 bootstrap member count is outside its bound"
        );
        let mut prior: Option<(&str, &str)> = None;
        let mut seen = BTreeSet::new();
        let mut entry_count = 0usize;
        for member in &self.bootstrap_members {
            member.validate()?;
            let key = (member.root.as_str(), member.path.as_str());
            ensure!(
                prior.is_none_or(|previous| previous < key),
                "startup V3 bootstrap members are not strictly ordered"
            );
            ensure!(
                seen.insert(key),
                "startup V3 bootstrap member is duplicated"
            );
            prior = Some(key);
            if member == &self.entry {
                entry_count += 1;
            }
        }
        ensure!(
            entry_count == 1,
            "startup V3 bootstrap list must contain the exact entry once"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3Owner {
    pub binary: String,
    pub role: String,
    pub executable_sha256: String,
}

impl StartupV3Owner {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.binary == OWNER_BINARY && self.role == OWNER_ROLE,
            "startup V3 owner selector is not fixed"
        );
        require_hash(
            &self.executable_sha256,
            "startup V3 owner executable digest",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3InnerHost {
    pub role: String,
    pub image: String,
    pub target: StartupV3Target,
}

impl StartupV3InnerHost {
    fn validate(&self, entry: &StartupV3SourceMember) -> anyhow::Result<()> {
        ensure!(
            self.role == INNER_HOST_ROLE && self.image == INNER_HOST_IMAGE,
            "startup V3 inner-host selector is not fixed"
        );
        self.target.validate(entry)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3Target {
    pub kind: String,
    pub source_root: String,
    pub worker_entry: String,
    pub project_root: String,
    pub working_directory: String,
    pub private_tmp: bool,
    pub roots: Vec<String>,
}

impl StartupV3Target {
    fn validate(&self, entry: &StartupV3SourceMember) -> anyhow::Result<()> {
        ensure!(
            self.kind == TARGET_KIND
                && self.source_root == SOURCE_ROOT
                && self.worker_entry == entry.path
                && entry.root == self.source_root
                && self.project_root == PROJECT_ROOT
                && self.working_directory == PROJECT_ROOT
                && self.private_tmp
                && self.roots.iter().map(String::as_str).eq(INNER_ROOT_ROLES),
            "startup V3 target selector or source-root join is not fixed"
        );
        ensure!(
            self.roots.len() == INNER_ROOT_ROLES.len(),
            "startup V3 target root list has an invalid length"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3SourceMember {
    pub root: String,
    pub path: String,
    pub sha256: String,
}

impl StartupV3SourceMember {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.root.len() <= 64 && self.root == SOURCE_ROOT,
            "startup V3 source member root is not the fixed logical root"
        );
        ensure!(
            self.path.len() <= 1_024,
            "startup V3 source member path exceeds its bound"
        );
        ryeos_state::objects::validate_canonical_project_relative_path(&self.path)
            .context("startup V3 source member path is not canonical")?;
        require_hash(&self.sha256, "startup V3 source member digest")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3Launch {
    pub command_sha256: String,
    pub args: Vec<StartupV3Argument>,
    #[serde(deserialize_with = "deserialize_bounded_environment")]
    pub env: BTreeMap<String, String>,
    pub process_environment_sha256: String,
    pub dynamic_channels: Vec<StartupV3DynamicChannel>,
}

impl StartupV3Launch {
    fn validate(&self) -> anyhow::Result<()> {
        require_hash(&self.command_sha256, "startup V3 loader digest")?;
        require_hash(
            &self.process_environment_sha256,
            "startup V3 process-environment digest",
        )?;
        self.validate_args()?;
        self.validate_env()?;
        self.validate_channels()?;
        Ok(())
    }

    fn validate_args(&self) -> anyhow::Result<()> {
        ensure!(
            (1..=32).contains(&self.args.len()),
            "startup V3 argument count is outside its bound"
        );
        let expected = [
            GNU_LOADER,
            "--inhibit-cache",
            "--library-path",
            "python-gnu/python/lib",
            "python-gnu/python/bin/python3.14",
            "-P",
            "-S",
            "-B",
            "-X",
            "utf8",
        ];
        ensure!(
            self.args.len() == expected.len() + 1,
            "startup V3 GNU/Python recipe has an invalid argument count"
        );
        for (argument, expected) in self.args.iter().zip(expected) {
            match argument {
                StartupV3Argument::Literal { value } => {
                    ensure!(
                        value.len() <= 1_024 && !value.chars().any(char::is_control),
                        "startup V3 literal argument is unbounded or contains controls"
                    );
                    ensure!(value == expected, "startup V3 GNU/Python argv differs");
                }
                StartupV3Argument::AdmittedSourceEntry => {
                    anyhow::bail!("startup V3 source-entry substitution is misplaced")
                }
            }
        }
        ensure!(
            matches!(
                self.args.last(),
                Some(StartupV3Argument::AdmittedSourceEntry)
            ),
            "startup V3 GNU/Python recipe lacks its final source-entry substitution"
        );
        Ok(())
    }

    fn validate_env(&self) -> anyhow::Result<()> {
        ensure!(
            self.env.len() <= 32,
            "startup V3 environment has too many entries"
        );
        ensure!(
            lillux::canonical_json(&serde_json::to_value(&self.env)?)
                .map_err(anyhow::Error::msg)?
                .len()
                <= 4_096,
            "startup V3 canonical environment exceeds its byte bound"
        );
        for (key, value) in &self.env {
            ensure!(
                !key.is_empty()
                    && key.len() <= 128
                    && value.len() <= 4_096
                    && !key.chars().any(char::is_control)
                    && !value.chars().any(char::is_control),
                "startup V3 environment has an unbounded or non-canonical entry"
            );
            let allowed_python_key = matches!(
                key.as_str(),
                "PYTHONHOME"
                    | "PYTHONHASHSEED"
                    | "PYTHONDONTWRITEBYTECODE"
                    | "PYTHONNOUSERSITE"
                    | "PYTHONSAFEPATH"
                    | "PYTHONUNBUFFERED"
            );
            ensure!(
                !environment_override_name(key) || allowed_python_key,
                "startup V3 interpreter/loader override is refused: {key}"
            );
            ensure!(
                !matches!(
                    key.as_str(),
                    SESSION_ENV
                        | RESOURCE_CLEANUP_ENV
                        | ORIGINAL_WINDOW_ENV
                        | ORIGINAL_CLOCK_NAMESPACE_ENV
                ),
                "startup V3 static environment shadows a registered descriptor role"
            );
        }
        for (key, value) in [
            ("PYTHONHOME", "python-gnu/python"),
            ("PYTHONHASHSEED", "0"),
            ("PYTHONDONTWRITEBYTECODE", "1"),
            ("PYTHONNOUSERSITE", "1"),
            ("PYTHONSAFEPATH", "1"),
            ("PYTHONUNBUFFERED", "1"),
            ("PATH", ""),
        ] {
            ensure!(
                self.env.get(key).is_some_and(|actual| actual == value),
                "startup V3 fixed GNU/Python environment differs: {key}"
            );
        }
        Ok(())
    }

    fn validate_channels(&self) -> anyhow::Result<()> {
        ensure!(
            self.dynamic_channels.len() == 2
                && self.dynamic_channels[0].role == StartupV3ChannelRole::Session
                && self.dynamic_channels[0].env == SESSION_ENV
                && self.dynamic_channels[1].role == StartupV3ChannelRole::ResourceCleanup
                && self.dynamic_channels[1].env == RESOURCE_CLEANUP_ENV,
            "startup V3 semantic channels are not the fixed ordered pair"
        );
        ensure!(
            self.dynamic_channels
                .iter()
                .all(|channel| !self.env.contains_key(&channel.env)),
            "startup V3 environment shadows a semantic channel"
        );
        Ok(())
    }
}

fn deserialize_bounded_environment<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedEnvironment;

    impl<'de> serde::de::Visitor<'de> for BoundedEnvironment {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded environment map without duplicate names")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if result.len() >= 32
                    || key.is_empty()
                    || key.len() > 128
                    || value.len() > 4_096
                    || result.insert(key, value).is_some()
                {
                    return Err(serde::de::Error::custom(
                        "startup V3 environment duplicate or unbounded entry",
                    ));
                }
            }
            Ok(result)
        }
    }

    deserializer.deserialize_map(BoundedEnvironment)
}

fn environment_override_name(key: &str) -> bool {
    key.starts_with("LD_")
        || (key.starts_with("PYTHON")
            && !matches!(
                key,
                "PYTHONHOME"
                    | "PYTHONHASHSEED"
                    | "PYTHONDONTWRITEBYTECODE"
                    | "PYTHONNOUSERSITE"
                    | "PYTHONSAFEPATH"
                    | "PYTHONUNBUFFERED"
            ))
        || matches!(
            key,
            "GLIBC_TUNABLES" | "GCONV_PATH" | "LOCPATH" | "__PYVENV_LAUNCHER__"
        )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StartupV3Argument {
    Literal { value: String },
    AdmittedSourceEntry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub enum StartupV3ChannelRole {
    #[serde(rename = "session")]
    Session,
    #[serde(rename = "resource_cleanup")]
    ResourceCleanup,
}

impl TryFrom<String> for StartupV3ChannelRole {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "session" => Ok(Self::Session),
            "resource_cleanup" => Ok(Self::ResourceCleanup),
            _ => Err("startup V3 channel role is not recognized"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3DynamicChannel {
    pub role: StartupV3ChannelRole,
    pub env: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3Products {
    pub gnu_slot: String,
    pub driver_slot: String,
    pub nvml_member: String,
    pub provider_checks: String,
    pub gnu_manifest_sha256: String,
    pub driver_manifest_sha256: String,
    pub tinygrad_manifest_sha256: String,
}

impl StartupV3Products {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.gnu_slot == "python-gnu"
                && self.driver_slot == "driver"
                && self.nvml_member == "lib/libnvidia-ml.so.1"
                && self.provider_checks == GNU_PROVIDER_CHECKS,
            "startup V3 product selector is not fixed"
        );
        require_hash(&self.gnu_manifest_sha256, "startup V3 GNU manifest digest")?;
        require_hash(
            &self.driver_manifest_sha256,
            "startup V3 driver manifest digest",
        )?;
        require_hash(
            &self.tinygrad_manifest_sha256,
            "startup V3 tinygrad manifest digest",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3Policy {
    pub schema: String,
    pub primary_device_role: String,
    #[serde(deserialize_with = "deserialize_required_l4_policy")]
    pub l4_query_policy: Option<StartupV3L4Policy>,
}

impl StartupV3Policy {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.schema == POLICY_SCHEMA,
            "startup V3 policy schema is unsupported"
        );
        ensure!(
            !self.primary_device_role.is_empty()
                && self.primary_device_role.len() <= 64
                && self
                    .primary_device_role
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte)),
            "startup V3 primary device role is not canonical"
        );
        Ok(())
    }
}

fn deserialize_required_l4_policy<'de, D>(
    deserializer: D,
) -> Result<Option<StartupV3L4Policy>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<StartupV3L4Policy>::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub enum StartupV3L4Policy {
    #[serde(rename = "l4-is-mig-success-zero-mode-not-supported-v1")]
    SuccessZero,
    #[serde(rename = "l4-is-mig-not-supported-mode-not-supported-v1")]
    NotSupported,
}

impl TryFrom<String> for StartupV3L4Policy {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "l4-is-mig-success-zero-mode-not-supported-v1" => Ok(Self::SuccessZero),
            "l4-is-mig-not-supported-mode-not-supported-v1" => Ok(Self::NotSupported),
            _ => Err("startup V3 L4 policy is not recognized"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3Wire {
    pub target_fd: u32,
    pub preflight_version: u32,
    pub result_version: u32,
    pub init_version: u32,
    pub maximum_frame_bytes: u32,
}

impl StartupV3Wire {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.target_fd == 4
                && self.preflight_version == 3
                && self.result_version == 3
                && self.init_version == 2
                && self.maximum_frame_bytes == 4_096,
            "startup V3 worker wire contract is not fixed"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3ManifestDelivery {
    pub abi: StartupV3ManifestAbi,
    pub gnu: StartupV3ManifestDataRole,
    pub driver: StartupV3ManifestDataRole,
    pub consumer: StartupV3ManifestConsumer,
}

impl StartupV3ManifestDelivery {
    fn validate(&self, bootstrap_members: &[StartupV3SourceMember]) -> anyhow::Result<()> {
        ensure!(
            self.gnu.access == StartupV3ManifestAccess::ReadOnly
                && self.gnu.destination == GNU_MANIFEST_DESTINATION
                && self.driver.access == StartupV3ManifestAccess::ReadOnly
                && self.driver.destination == DRIVER_MANIFEST_DESTINATION,
            "startup V3 manifest delivery destinations/access are not fixed"
        );
        ensure!(
            self.consumer.root == SOURCE_ROOT && self.consumer.path == SOURCE_CONSUMER,
            "startup V3 manifest consumer is not the fixed source member"
        );
        ensure!(
            bootstrap_members
                .iter()
                .filter(|member| {
                    member.root == self.consumer.root && member.path == self.consumer.path
                })
                .count()
                == 1,
            "startup V3 manifest consumer is not unique in bootstrap members"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3ManifestConsumer {
    pub root: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub enum StartupV3ManifestAbi {
    #[serde(rename = "fixed_namespace_v1")]
    FixedNamespaceV1,
}

impl TryFrom<String> for StartupV3ManifestAbi {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "fixed_namespace_v1" => Ok(Self::FixedNamespaceV1),
            _ => Err("startup V3 manifest ABI is not recognized"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub enum StartupV3ManifestAccess {
    #[serde(rename = "read_only")]
    ReadOnly,
}

impl TryFrom<String> for StartupV3ManifestAccess {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "read_only" => Ok(Self::ReadOnly),
            _ => Err("startup V3 manifest access is not recognized"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupV3ManifestDataRole {
    pub destination: String,
    pub access: StartupV3ManifestAccess,
}

fn require_hash(value: &str, label: &str) -> anyhow::Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is not a canonical SHA-256 digest"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_document() -> serde_json::Value {
        serde_json::json!({
            "schema": STARTUP_CONTRACT_V3_SCHEMA,
            "role": STARTUP_CONTRACT_V3_ROLE,
            "owner": {
                "binary": OWNER_BINARY,
                "role": OWNER_ROLE,
                "executable_sha256": "a".repeat(64)
            },
            "inner_host": {
                "role": INNER_HOST_ROLE,
                "image": INNER_HOST_IMAGE,
                "target": {
                    "kind": TARGET_KIND,
                    "source_root": SOURCE_ROOT,
                    "worker_entry": "worker.py",
                    "project_root": PROJECT_ROOT,
                    "working_directory": PROJECT_ROOT,
                    "private_tmp": true,
                    "roots": INNER_ROOT_ROLES
                }
            },
            "entry": {"root":"source", "path":"worker.py", "sha256":"b".repeat(64)},
            "bootstrap_members": [
                {"root":"source", "path":"bootstrap_support.py", "sha256":"c".repeat(64)},
                {"root":"source", "path":"worker.py", "sha256":"b".repeat(64)}
            ],
            "launch": {
                "command_sha256":"d".repeat(64),
                "args":[
                    {"kind":"literal", "value":GNU_LOADER},
                    {"kind":"literal", "value":"--inhibit-cache"},
                    {"kind":"literal", "value":"--library-path"},
                    {"kind":"literal", "value":"python-gnu/python/lib"},
                    {"kind":"literal", "value":"python-gnu/python/bin/python3.14"},
                    {"kind":"literal", "value":"-P"},
                    {"kind":"literal", "value":"-S"},
                    {"kind":"literal", "value":"-B"},
                    {"kind":"literal", "value":"-X"},
                    {"kind":"literal", "value":"utf8"},
                    {"kind":"admitted_source_entry"}
                ],
                "env": {
                    "PATH":"",
                    "PYTHONHOME":"python-gnu/python",
                    "PYTHONHASHSEED":"0",
                    "PYTHONDONTWRITEBYTECODE":"1",
                    "PYTHONNOUSERSITE":"1",
                    "PYTHONSAFEPATH":"1",
                    "PYTHONUNBUFFERED":"1"
                },
                "process_environment_sha256":"e".repeat(64),
                "dynamic_channels":[
                    {"role":"session", "env":SESSION_ENV},
                    {"role":"resource_cleanup", "env":RESOURCE_CLEANUP_ENV}
                ]
            },
            "products": {
                "gnu_slot":"python-gnu",
                "driver_slot":"driver",
                "nvml_member":"lib/libnvidia-ml.so.1",
                "provider_checks":GNU_PROVIDER_CHECKS,
                "gnu_manifest_sha256":"f".repeat(64),
                "driver_manifest_sha256":"1".repeat(64),
                "tinygrad_manifest_sha256":"2".repeat(64)
            },
            "policy": {
                "schema":POLICY_SCHEMA,
                "primary_device_role":"gpu-primary",
                "l4_query_policy":null
            },
            "wire": {
                "target_fd":4,
                "preflight_version":3,
                "result_version":3,
                "init_version":2,
                "maximum_frame_bytes":4096
            },
            "manifest_delivery": {
                "abi":"fixed_namespace_v1",
                "gnu":{"destination":GNU_MANIFEST_DESTINATION,"access":"read_only"},
                "driver":{"destination":DRIVER_MANIFEST_DESTINATION,"access":"read_only"},
                "consumer":{"root":"source","path":SOURCE_CONSUMER}
            }
        })
    }

    fn decode_value(value: &serde_json::Value) -> anyhow::Result<StartupContractV3> {
        decode_and_validate_v3(&serde_json::to_vec(value)?)
    }

    fn valid_baseline() -> serde_json::Value {
        let value = valid_document();
        assert!(
            decode_value(&value).is_ok(),
            "negative control baseline is valid"
        );
        value
    }

    #[test]
    fn accepts_one_complete_fixed_v3_recipe() {
        assert!(decode_value(&valid_document()).is_ok());
    }

    #[test]
    fn rejects_unsupported_or_open_schema_shapes() {
        let mut unknown = valid_baseline();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(decode_value(&unknown).is_err());

        let mut wrong_schema = valid_baseline();
        wrong_schema["schema"] =
            serde_json::json!("ryeos.local_tinygrad_current_assignment_startup.v2");
        assert!(decode_value(&wrong_schema).is_err());

        let mut missing_policy = valid_baseline();
        missing_policy["policy"]
            .as_object_mut()
            .unwrap()
            .remove("l4_query_policy");
        assert!(decode_value(&missing_policy).is_err());
    }

    #[test]
    fn rejects_duplicate_fields_and_duplicate_environment_names() {
        let bytes = serde_json::to_vec(&valid_baseline()).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let duplicate_field = text.replacen(
            "\"schema\":\"ryeos.local_tinygrad_current_assignment_startup.v3\"",
            "\"schema\":\"ryeos.local_tinygrad_current_assignment_startup.v3\",\"schema\":\"ryeos.local_tinygrad_current_assignment_startup.v3\"",
            1,
        );
        assert!(decode_and_validate_v3(duplicate_field.as_bytes()).is_err());

        let duplicate_env = text.replace("\"PATH\":\"\"", "\"PATH\":\"\",\"PATH\":\"\"");
        assert!(decode_and_validate_v3(duplicate_env.as_bytes()).is_err());
    }

    #[test]
    fn rejects_wrong_target_entry_root_and_noncanonical_member_order() {
        let mut wrong_entry = valid_baseline();
        wrong_entry["inner_host"]["target"]["worker_entry"] = serde_json::json!("other.py");
        assert!(decode_value(&wrong_entry).is_err());

        let mut wrong_root = valid_baseline();
        wrong_root["entry"]["root"] = serde_json::json!("worker-source");
        assert!(decode_value(&wrong_root).is_err());

        let mut wrong_target_root = valid_baseline();
        wrong_target_root["inner_host"]["target"]["source_root"] =
            serde_json::json!("worker-source");
        assert!(decode_value(&wrong_target_root).is_err());

        let mut wrong_target_order = valid_baseline();
        wrong_target_order["inner_host"]["target"]["roots"]
            .as_array_mut()
            .unwrap()
            .swap(0, 1);
        assert!(decode_value(&wrong_target_order).is_err());

        let mut reversed = valid_baseline();
        reversed["bootstrap_members"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert!(decode_value(&reversed).is_err());
    }

    #[test]
    fn rejects_recipe_shadowing_and_wrong_arguments() {
        let mut shadowed = valid_baseline();
        shadowed["launch"]["env"][SESSION_ENV] = serde_json::json!("5");
        assert!(decode_value(&shadowed).is_err());

        let mut loader_override = valid_baseline();
        loader_override["launch"]["env"]["LD_LIBRARY_PATH"] = serde_json::json!("x");
        assert!(decode_value(&loader_override).is_err());

        let mut wrong_arg = valid_baseline();
        wrong_arg["launch"]["args"][1]["value"] = serde_json::json!("--library-path");
        assert!(decode_value(&wrong_arg).is_err());

        let mut wrong_variant = valid_baseline();
        wrong_variant["launch"]["args"][0]["kind"] = serde_json::json!("arbitrary");
        assert!(decode_value(&wrong_variant).is_err());

        for (role, env) in [
            ("resource_cleanup", RESOURCE_CLEANUP_ENV),
            ("session", ORIGINAL_WINDOW_ENV),
            ("session", ORIGINAL_CLOCK_NAMESPACE_ENV),
        ] {
            let mut shadow = valid_baseline();
            shadow["launch"]["env"][env] = serde_json::json!("16");
            assert!(decode_value(&shadow).is_err(), "descriptor role {role}");
        }

        let mut wrong_channel = valid_baseline();
        wrong_channel["launch"]["dynamic_channels"][0]["role"] =
            serde_json::json!("resource_cleanup");
        assert!(decode_value(&wrong_channel).is_err());

        let mut wrong_channel_env = valid_baseline();
        wrong_channel_env["launch"]["dynamic_channels"][0]["env"] =
            serde_json::json!(RESOURCE_CLEANUP_ENV);
        assert!(decode_value(&wrong_channel_env).is_err());
    }

    #[test]
    fn enforces_member_and_environment_bounds_with_canonical_unicode_paths() {
        let mut unicode_path = valid_baseline();
        unicode_path["entry"]["path"] = serde_json::json!("λ/worker.py");
        unicode_path["inner_host"]["target"]["worker_entry"] = serde_json::json!("λ/worker.py");
        unicode_path["bootstrap_members"][1]["path"] = serde_json::json!("λ/worker.py");
        assert!(decode_value(&unicode_path).is_ok());

        let too_long_path = "a".repeat(1_025);
        let mut long_path = valid_baseline();
        long_path["entry"]["path"] = serde_json::json!(too_long_path);
        long_path["inner_host"]["target"]["worker_entry"] = long_path["entry"]["path"].clone();
        long_path["bootstrap_members"][1]["path"] = long_path["entry"]["path"].clone();
        assert!(decode_value(&long_path).is_err());

        let mut at_entry_limit = valid_baseline();
        for index in 0..25 {
            at_entry_limit["launch"]["env"][format!("SAFE_{index:02}")] = serde_json::json!("x");
        }
        assert!(decode_value(&at_entry_limit).is_ok());
        at_entry_limit["launch"]["env"]["SAFE_25"] = serde_json::json!("x");
        assert!(decode_value(&at_entry_limit).is_err());

        let mut oversized_key = valid_baseline();
        oversized_key["launch"]["env"]["K".repeat(129)] = serde_json::json!("x");
        assert!(decode_value(&oversized_key).is_err());

        let mut oversized_value = valid_baseline();
        oversized_value["launch"]["env"]["SAFE_VALUE"] = serde_json::json!("x".repeat(4_097));
        assert!(decode_value(&oversized_value).is_err());

        let mut oversized_canonical_map = valid_baseline();
        oversized_canonical_map["launch"]["env"]["SAFE_VALUE"] =
            serde_json::json!("x".repeat(4_000));
        assert!(decode_value(&oversized_canonical_map).is_err());
    }

    #[test]
    fn rejects_fixed_recipe_and_manifest_policy_drift() {
        let mut wrong_product = valid_baseline();
        wrong_product["products"]["gnu_slot"] = serde_json::json!("gnu");
        assert!(decode_value(&wrong_product).is_err());

        let mut wrong_consumer = valid_baseline();
        wrong_consumer["manifest_delivery"]["consumer"]["path"] =
            serde_json::json!("not-bootstrap.py");
        assert!(decode_value(&wrong_consumer).is_err());

        let mut invalid_nullable_enum = valid_baseline();
        invalid_nullable_enum["policy"]["l4_query_policy"] = serde_json::json!("l4-policy-alias");
        assert!(decode_value(&invalid_nullable_enum).is_err());

        let mut exact_nullable_enum = valid_baseline();
        exact_nullable_enum["policy"]["l4_query_policy"] =
            serde_json::json!("l4-is-mig-success-zero-mode-not-supported-v1");
        assert!(decode_value(&exact_nullable_enum).is_ok());

        let mut wrapped_nullable_enum = valid_baseline();
        wrapped_nullable_enum["policy"]["l4_query_policy"] =
            serde_json::json!({"l4-is-mig-success-zero-mode-not-supported-v1": null});
        assert!(decode_value(&wrapped_nullable_enum).is_err());

        let mut wrapped_channel_role = valid_baseline();
        wrapped_channel_role["launch"]["dynamic_channels"][0]["role"] =
            serde_json::json!({"session": null});
        assert!(decode_value(&wrapped_channel_role).is_err());

        let mut wrapped_manifest_abi = valid_baseline();
        wrapped_manifest_abi["manifest_delivery"]["abi"] =
            serde_json::json!({"fixed_namespace_v1": null});
        assert!(decode_value(&wrapped_manifest_abi).is_err());

        let mut wrapped_manifest_access = valid_baseline();
        wrapped_manifest_access["manifest_delivery"]["gnu"]["access"] =
            serde_json::json!({"read_only": null});
        assert!(decode_value(&wrapped_manifest_access).is_err());
    }

    #[test]
    fn enforces_original_byte_bound_and_canonical_digest_syntax() {
        assert!(decode_and_validate_v3(&[]).is_err());
        assert!(decode_and_validate_v3(&vec![b' '; STARTUP_CONTRACT_V3_MAX_BYTES + 1]).is_err());

        let mut bad_digest = valid_baseline();
        bad_digest["owner"]["executable_sha256"] = serde_json::json!("A".repeat(64));
        assert!(decode_value(&bad_digest).is_err());
    }
}
