//! Strict, path-authority-free wire contracts shared by RyeOS isolation
//! engines, signed bundle manifests, and isolation adapters.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{DeserializeOwned, DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub const ISOLATION_ADAPTER_PROTOCOL: &str = "ryeos.isolation-adapter/v1";
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const MAX_AUTHORITIES: usize = 4096;
pub const MAX_MOUNTS: usize = 4096;
pub const MAX_ENVIRONMENT_ENTRIES: usize = 4096;
pub const MAX_ARGUMENTS: usize = 4096;
pub const MAX_STRING_BYTES: usize = 64 * 1024;
pub const MAX_DIAGNOSTIC_DETAILS: usize = 128;
pub const MAX_JSON_DEPTH: usize = 64;

/// Decode an isolation protocol document while rejecting duplicate object
/// keys at every nesting level and bounding recursive JSON structure.
pub fn from_json_slice_strict<T>(input: &[u8]) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let value = StrictJsonValue { depth: 0 }.deserialize(&mut deserializer)?;
    deserializer.end()?;
    serde_json::from_value(value)
}

pub fn from_json_str_strict<T>(input: &str) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    from_json_slice_strict(input.as_bytes())
}

struct StrictJsonValue {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for StrictJsonValue {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonValueVisitor { depth: self.depth })
    }
}

struct StrictJsonValueVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for StrictJsonValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(A::Error::custom(format!(
                "JSON nesting exceeds {MAX_JSON_DEPTH} levels"
            )));
        }
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(value) = sequence.next_element_seed(StrictJsonValue {
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut mapping: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(A::Error::custom(format!(
                "JSON nesting exceeds {MAX_JSON_DEPTH} levels"
            )));
        }
        let mut values = serde_json::Map::new();
        while let Some(key) = mapping.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom(format!(
                    "duplicate JSON object key `{key}`"
                )));
            }
            let value = mapping.next_value_seed(StrictJsonValue {
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IsolationAdapterProtocolVersion {
    #[serde(rename = "ryeos.isolation-adapter/v1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IsolationTargetTriple {
    #[serde(rename = "x86_64-unknown-linux-gnu")]
    X86_64UnknownLinuxGnu,
    #[serde(rename = "aarch64-unknown-linux-gnu")]
    Aarch64UnknownLinuxGnu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IsolationArtifactRole {
    #[serde(rename = "launcher")]
    Launcher,
    #[serde(rename = "loader")]
    Loader,
    #[serde(rename = "runtime_library")]
    RuntimeLibrary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationBackendSelection {
    pub bundle: String,
    pub implementation: String,
}

impl IsolationBackendSelection {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_identifier("isolation bundle", &self.bundle)?;
        validate_identifier("isolation implementation", &self.implementation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IsolationCapability {
    #[serde(rename = "filesystem.private_root")]
    FilesystemPrivateRoot,
    #[serde(rename = "filesystem.fd_read_only")]
    FilesystemFdReadOnly,
    #[serde(rename = "filesystem.fd_writable")]
    FilesystemFdWritable,
    #[serde(rename = "filesystem.ordered_overlays")]
    FilesystemOrderedOverlays,
    #[serde(rename = "filesystem.private_tmp")]
    FilesystemPrivateTmp,
    #[serde(rename = "devices.minimal")]
    DevicesMinimal,
    #[serde(rename = "environment.exact")]
    EnvironmentExact,
    #[serde(rename = "network.host")]
    NetworkHost,
    #[serde(rename = "network.isolated")]
    NetworkIsolated,
    #[serde(rename = "process.host_pid_namespace")]
    ProcessHostPidNamespace,
    #[serde(rename = "process.target_pid_reporting")]
    ProcessTargetPidReporting,
    #[serde(rename = "lifecycle.shared_process_group")]
    LifecycleSharedProcessGroup,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct IsolationAuthorityId(String);

impl IsolationAuthorityId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolValidationError> {
        let value = value.into();
        validate_identifier("authority id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for IsolationAuthorityId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct IsolationPath(String);

impl IsolationPath {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolValidationError> {
        let value = value.into();
        validate_string("isolation path", &value)?;
        if !value.starts_with('/') {
            return Err(ProtocolValidationError::new(
                "isolation path must be absolute",
            ));
        }
        if value.split('/').any(|part| part == "." || part == "..") {
            return Err(ProtocolValidationError::new(
                "isolation path cannot contain dot components",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for IsolationPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationBackendDeclaration {
    pub id: String,
    pub protocol: IsolationAdapterProtocolVersion,
    pub targets: Vec<IsolationTargetTriple>,
    pub adapter: String,
    pub artifacts: BTreeMap<IsolationArtifactRole, String>,
    pub capabilities: BTreeSet<IsolationCapability>,
}

impl IsolationBackendDeclaration {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_identifier("backend id", &self.id)?;
        validate_executable_name("adapter", &self.adapter)?;
        if self.targets.is_empty() {
            return Err(ProtocolValidationError::new(
                "isolation backend must declare at least one target",
            ));
        }
        if self.targets.iter().collect::<BTreeSet<_>>().len() != self.targets.len() {
            return Err(ProtocolValidationError::new(
                "isolation backend contains a duplicate target",
            ));
        }
        if self.capabilities.is_empty() {
            return Err(ProtocolValidationError::new(
                "isolation backend must declare capabilities",
            ));
        }
        if !self
            .artifacts
            .contains_key(&IsolationArtifactRole::Launcher)
        {
            return Err(ProtocolValidationError::new(
                "isolation backend must declare a launcher artifact",
            ));
        }
        let mut names = BTreeSet::new();
        names.insert(self.adapter.as_str());
        for name in self.artifacts.values() {
            validate_executable_name("artifact", name)?;
            if !names.insert(name.as_str()) {
                return Err(ProtocolValidationError::new(
                    "adapter and artifact executable names must be distinct",
                ));
            }
        }
        Ok(())
    }

    /// Narrow live adapter claims to the maximum authority granted by the
    /// signed bundle declaration. Inspection can remove authority but cannot
    /// add authority that the manifest signer did not grant.
    pub fn effective_capabilities(
        &self,
        inspected: &BTreeSet<IsolationCapability>,
    ) -> BTreeSet<IsolationCapability> {
        self.capabilities.intersection(inspected).copied().collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationAuthorityPurpose {
    ReadOnlyMount,
    WritableMount,
    Executable,
    RuntimeLibraryDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationAuthority {
    pub id: IsolationAuthorityId,
    pub inherited_fd: u32,
    pub purpose: IsolationAuthorityPurpose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMountAccess {
    ReadOnly,
    Writable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationMount {
    pub source: IsolationAuthorityId,
    pub destination: IsolationPath,
    pub access: IsolationMountAccess,
    pub layer: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationNetwork {
    Host,
    Isolated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationDeviceSurface {
    Minimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationEnvironment {
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationTarget {
    pub executable: IsolationAuthorityId,
    pub argv0: String,
    pub arguments: Vec<String>,
    pub cwd: IsolationPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationPlan {
    pub target: IsolationTarget,
    pub mounts: Vec<IsolationMount>,
    pub environment: IsolationEnvironment,
    pub network: IsolationNetwork,
    pub devices: IsolationDeviceSurface,
    pub private_tmp: bool,
    pub host_pid_namespace: bool,
    pub shared_process_group: bool,
}

impl IsolationPlan {
    pub fn validate(
        &self,
        authorities: &[IsolationAuthority],
    ) -> Result<BTreeSet<IsolationCapability>, ProtocolValidationError> {
        if authorities.len() > MAX_AUTHORITIES {
            return Err(ProtocolValidationError::new("too many authorities"));
        }
        if self.mounts.len() > MAX_MOUNTS {
            return Err(ProtocolValidationError::new("too many mounts"));
        }
        if self.environment.values.len() > MAX_ENVIRONMENT_ENTRIES {
            return Err(ProtocolValidationError::new("too many environment entries"));
        }
        if self.target.arguments.len() > MAX_ARGUMENTS {
            return Err(ProtocolValidationError::new("too many target arguments"));
        }
        validate_string("argv0", &self.target.argv0)?;
        for argument in &self.target.arguments {
            validate_string("target argument", argument)?;
        }
        for (name, value) in &self.environment.values {
            validate_environment_name(name)?;
            validate_string("environment value", value)?;
        }

        let mut authority_ids = BTreeMap::new();
        let mut descriptors = BTreeSet::new();
        for authority in authorities {
            if authority.inherited_fd <= 2 {
                return Err(ProtocolValidationError::new(
                    "authority descriptor overlaps stdio",
                ));
            }
            if authority_ids
                .insert(authority.id.clone(), authority.purpose)
                .is_some()
            {
                return Err(ProtocolValidationError::new("duplicate authority id"));
            }
            if !descriptors.insert(authority.inherited_fd) {
                return Err(ProtocolValidationError::new(
                    "duplicate authority descriptor",
                ));
            }
        }
        match authority_ids.get(&self.target.executable) {
            Some(IsolationAuthorityPurpose::Executable) => {}
            _ => {
                return Err(ProtocolValidationError::new(
                    "target executable authority is missing or has the wrong purpose",
                ))
            }
        }

        let mut used_authorities = BTreeSet::from([self.target.executable.clone()]);
        let mut target_mounts = 0usize;
        let mut previous_layer = None;
        for mount in &self.mounts {
            let Some(purpose) = authority_ids.get(&mount.source) else {
                return Err(ProtocolValidationError::new(
                    "mount references an unknown authority",
                ));
            };
            let purpose_matches = match mount.access {
                IsolationMountAccess::ReadOnly => matches!(
                    purpose,
                    IsolationAuthorityPurpose::ReadOnlyMount
                        | IsolationAuthorityPurpose::Executable
                        | IsolationAuthorityPurpose::RuntimeLibraryDirectory
                ),
                IsolationMountAccess::Writable => {
                    *purpose == IsolationAuthorityPurpose::WritableMount
                }
            };
            if !purpose_matches {
                return Err(ProtocolValidationError::new(
                    "mount authority purpose does not match requested access",
                ));
            }
            used_authorities.insert(mount.source.clone());
            if mount.source == self.target.executable {
                if mount.access != IsolationMountAccess::ReadOnly {
                    return Err(ProtocolValidationError::new(
                        "target executable must use a read-only mount",
                    ));
                }
                target_mounts += 1;
            }
            if previous_layer.is_some_and(|layer| mount.layer < layer) {
                return Err(ProtocolValidationError::new(
                    "mount layers must be deterministically ordered",
                ));
            }
            previous_layer = Some(mount.layer);
        }
        if target_mounts != 1 {
            return Err(ProtocolValidationError::new(
                "target executable authority must have exactly one mount",
            ));
        }
        if used_authorities.len() != authority_ids.len() {
            return Err(ProtocolValidationError::new(
                "every inherited authority must be used by the isolation plan",
            ));
        }

        Ok(self.required_capabilities())
    }

    pub fn required_capabilities(&self) -> BTreeSet<IsolationCapability> {
        let mut capabilities = BTreeSet::from([
            IsolationCapability::FilesystemPrivateRoot,
            IsolationCapability::FilesystemOrderedOverlays,
            IsolationCapability::DevicesMinimal,
            IsolationCapability::EnvironmentExact,
            IsolationCapability::ProcessTargetPidReporting,
        ]);
        if self
            .mounts
            .iter()
            .any(|mount| mount.access == IsolationMountAccess::ReadOnly)
        {
            capabilities.insert(IsolationCapability::FilesystemFdReadOnly);
        }
        if self
            .mounts
            .iter()
            .any(|mount| mount.access == IsolationMountAccess::Writable)
        {
            capabilities.insert(IsolationCapability::FilesystemFdWritable);
        }
        if self.private_tmp {
            capabilities.insert(IsolationCapability::FilesystemPrivateTmp);
        }
        capabilities.insert(match self.network {
            IsolationNetwork::Host => IsolationCapability::NetworkHost,
            IsolationNetwork::Isolated => IsolationCapability::NetworkIsolated,
        });
        if self.host_pid_namespace {
            capabilities.insert(IsolationCapability::ProcessHostPidNamespace);
        }
        if self.shared_process_group {
            capabilities.insert(IsolationCapability::LifecycleSharedProcessGroup);
        }
        capabilities
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterInspectionRequest {
    pub protocol: IsolationAdapterProtocolVersion,
    pub target: IsolationTargetTriple,
    pub backend_id: String,
    pub artifacts: BTreeMap<IsolationArtifactRole, u32>,
}

impl AdapterInspectionRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_identifier("backend id", &self.backend_id)?;
        validate_artifact_descriptors(&self.artifacts, None).map(|_| ())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterInspectionResponse {
    pub protocol: IsolationAdapterProtocolVersion,
    pub adapter_build: String,
    pub effective_capabilities: BTreeSet<IsolationCapability>,
    pub artifacts: BTreeMap<IsolationArtifactRole, InspectedArtifact>,
}

impl AdapterInspectionResponse {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_string("adapter build", &self.adapter_build)?;
        if self.effective_capabilities.is_empty() {
            return Err(ProtocolValidationError::new(
                "adapter inspection must report capabilities",
            ));
        }
        if !self
            .artifacts
            .contains_key(&IsolationArtifactRole::Launcher)
        {
            return Err(ProtocolValidationError::new(
                "adapter inspection must report the launcher artifact",
            ));
        }
        for artifact in self.artifacts.values() {
            artifact.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedArtifact {
    pub version: String,
    pub digest: String,
}

impl InspectedArtifact {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_string("artifact version", &self.version)?;
        if self.digest.len() != 64
            || !self
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ProtocolValidationError::new(
                "artifact digest must be a lowercase SHA-256 hex digest",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterLaunchRequest {
    pub protocol: IsolationAdapterProtocolVersion,
    pub plan: IsolationPlan,
    pub authorities: Vec<IsolationAuthority>,
    pub artifacts: BTreeMap<IsolationArtifactRole, u32>,
    pub status_fd: u32,
}

impl AdapterLaunchRequest {
    pub fn validate(&self) -> Result<BTreeSet<IsolationCapability>, ProtocolValidationError> {
        let required = self.plan.validate(&self.authorities)?;
        if self.status_fd <= 2 {
            return Err(ProtocolValidationError::new(
                "status descriptor overlaps stdio",
            ));
        }
        let mut descriptors = validate_artifact_descriptors(&self.artifacts, Some(self.status_fd))?;
        for authority in &self.authorities {
            if !descriptors.insert(authority.inherited_fd) {
                return Err(ProtocolValidationError::new(
                    "descriptor is reused across isolation protocol roles",
                ));
            }
        }
        Ok(required)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationDiagnosticCode {
    InvalidRequest,
    UnsupportedProtocol,
    MissingCapability,
    InvalidDescriptor,
    IncompatibleArtifact,
    PlatformUnavailable,
    LaunchRefused,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationDiagnostic {
    pub code: IsolationDiagnosticCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

impl IsolationDiagnostic {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_string("diagnostic message", &self.message)?;
        if self.details.len() > MAX_DIAGNOSTIC_DETAILS {
            return Err(ProtocolValidationError::new(
                "too many diagnostic detail entries",
            ));
        }
        for (name, value) in &self.details {
            validate_identifier("diagnostic detail name", name)?;
            validate_string("diagnostic detail value", value)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LauncherRefusalDocument {
    pub refused: IsolationDiagnostic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolValidationError {
    message: String,
}

impl ProtocolValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ProtocolValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProtocolValidationError {}

fn validate_string(kind: &str, value: &str) -> Result<(), ProtocolValidationError> {
    if value.is_empty() {
        return Err(ProtocolValidationError::new(format!(
            "{kind} cannot be empty"
        )));
    }
    if value.len() > MAX_STRING_BYTES {
        return Err(ProtocolValidationError::new(format!(
            "{kind} exceeds {MAX_STRING_BYTES} bytes"
        )));
    }
    if value.as_bytes().contains(&0) {
        return Err(ProtocolValidationError::new(format!(
            "{kind} contains an interior NUL"
        )));
    }
    Ok(())
}

fn validate_identifier(kind: &str, value: &str) -> Result<(), ProtocolValidationError> {
    validate_string(kind, value)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ProtocolValidationError::new(format!(
            "{kind} contains an unsupported character"
        )));
    }
    Ok(())
}

fn validate_executable_name(kind: &str, value: &str) -> Result<(), ProtocolValidationError> {
    validate_identifier(kind, value)?;
    if value == "." || value == ".." {
        return Err(ProtocolValidationError::new(format!(
            "{kind} must be a safe single-component executable name"
        )));
    }
    Ok(())
}

fn validate_environment_name(value: &str) -> Result<(), ProtocolValidationError> {
    validate_string("environment name", value)?;
    if value.as_bytes().contains(&b'=') {
        return Err(ProtocolValidationError::new(
            "environment name contains '='",
        ));
    }
    Ok(())
}

fn validate_artifact_descriptors(
    artifacts: &BTreeMap<IsolationArtifactRole, u32>,
    reserved: Option<u32>,
) -> Result<BTreeSet<u32>, ProtocolValidationError> {
    if !artifacts.contains_key(&IsolationArtifactRole::Launcher) {
        return Err(ProtocolValidationError::new(
            "isolation request is missing the launcher artifact",
        ));
    }
    let mut descriptors = reserved.into_iter().collect::<BTreeSet<_>>();
    for descriptor in artifacts.values().copied() {
        if descriptor <= 2 {
            return Err(ProtocolValidationError::new(
                "artifact descriptor overlaps stdio",
            ));
        }
        if !descriptors.insert(descriptor) {
            return Err(ProtocolValidationError::new(
                "descriptor is reused across isolation protocol roles",
            ));
        }
    }
    Ok(descriptors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority(
        id: &str,
        inherited_fd: u32,
        purpose: IsolationAuthorityPurpose,
    ) -> IsolationAuthority {
        IsolationAuthority {
            id: IsolationAuthorityId::new(id).unwrap(),
            inherited_fd,
            purpose,
        }
    }

    fn complete_plan() -> (IsolationPlan, Vec<IsolationAuthority>) {
        let executable = IsolationAuthorityId::new("target").unwrap();
        let readable = IsolationAuthorityId::new("project").unwrap();
        let writable = IsolationAuthorityId::new("workspace").unwrap();
        (
            IsolationPlan {
                target: IsolationTarget {
                    executable: executable.clone(),
                    argv0: "python".to_string(),
                    arguments: vec!["script.py".to_string()],
                    cwd: IsolationPath::new("/workspace").unwrap(),
                },
                mounts: vec![
                    IsolationMount {
                        source: executable,
                        destination: IsolationPath::new("/bin/python").unwrap(),
                        access: IsolationMountAccess::ReadOnly,
                        layer: 0,
                    },
                    IsolationMount {
                        source: readable,
                        destination: IsolationPath::new("/project").unwrap(),
                        access: IsolationMountAccess::ReadOnly,
                        layer: 1,
                    },
                    IsolationMount {
                        source: writable,
                        destination: IsolationPath::new("/workspace").unwrap(),
                        access: IsolationMountAccess::Writable,
                        layer: 2,
                    },
                ],
                environment: IsolationEnvironment {
                    values: BTreeMap::from([("PATH".to_string(), "/bin".to_string())]),
                },
                network: IsolationNetwork::Isolated,
                devices: IsolationDeviceSurface::Minimal,
                private_tmp: true,
                host_pid_namespace: true,
                shared_process_group: true,
            },
            vec![
                authority("target", 3, IsolationAuthorityPurpose::Executable),
                authority("project", 4, IsolationAuthorityPurpose::ReadOnlyMount),
                authority("workspace", 5, IsolationAuthorityPurpose::WritableMount),
            ],
        )
    }

    #[test]
    fn paths_are_absolute_and_normalized() {
        assert!(IsolationPath::new("/workspace/item").is_ok());
        assert!(IsolationPath::new("workspace/item").is_err());
        assert!(IsolationPath::new("/workspace/../secret").is_err());
    }

    #[test]
    fn declaration_requires_distinct_launcher() {
        let declaration = IsolationBackendDeclaration {
            id: "example".to_string(),
            protocol: IsolationAdapterProtocolVersion::V1,
            targets: vec![IsolationTargetTriple::X86_64UnknownLinuxGnu],
            adapter: "adapter".to_string(),
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, "adapter".to_string())]),
            capabilities: BTreeSet::from([IsolationCapability::FilesystemPrivateRoot]),
        };
        assert!(declaration.validate().is_err());
    }

    #[test]
    fn declaration_rejects_duplicate_targets_and_unsafe_executable_names() {
        let mut declaration = IsolationBackendDeclaration {
            id: "example".to_string(),
            protocol: IsolationAdapterProtocolVersion::V1,
            targets: vec![
                IsolationTargetTriple::X86_64UnknownLinuxGnu,
                IsolationTargetTriple::X86_64UnknownLinuxGnu,
            ],
            adapter: "adapter".to_string(),
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, "launcher".to_string())]),
            capabilities: BTreeSet::from([IsolationCapability::FilesystemPrivateRoot]),
        };
        assert!(declaration
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate target"));

        declaration.targets.truncate(1);
        declaration.adapter = "../adapter".to_string();
        assert!(declaration.validate().is_err());
    }

    #[test]
    fn signed_declaration_is_the_capability_upper_bound() {
        let declaration = IsolationBackendDeclaration {
            id: "linux".to_string(),
            protocol: IsolationAdapterProtocolVersion::V1,
            targets: vec![IsolationTargetTriple::X86_64UnknownLinuxGnu],
            adapter: "adapter".to_string(),
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, "launcher".to_string())]),
            capabilities: BTreeSet::from([
                IsolationCapability::NetworkIsolated,
                IsolationCapability::EnvironmentExact,
            ]),
        };
        let inspected = BTreeSet::from([
            IsolationCapability::NetworkHost,
            IsolationCapability::NetworkIsolated,
        ]);
        assert_eq!(
            declaration.effective_capabilities(&inspected),
            BTreeSet::from([IsolationCapability::NetworkIsolated])
        );
    }

    #[test]
    fn refusal_wire_preserves_the_exact_top_level_field() {
        let document = LauncherRefusalDocument {
            refused: IsolationDiagnostic {
                code: IsolationDiagnosticCode::LaunchRefused,
                message: "refused".to_string(),
                details: BTreeMap::new(),
            },
        };
        let value = serde_json::to_value(document).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 1);
        assert!(value.get("refused").is_some());
    }

    #[test]
    fn strict_json_rejects_duplicate_keys_at_every_depth() {
        let top_level = r#"{"protocol":"ryeos.isolation-adapter/v1","protocol":"ryeos.isolation-adapter/v1","target":"x86_64-unknown-linux-gnu","backend_id":"example","artifacts":{"launcher":3}}"#;
        let nested = r#"{"protocol":"ryeos.isolation-adapter/v1","target":"x86_64-unknown-linux-gnu","backend_id":"example","artifacts":{"launcher":3,"launcher":4}}"#;
        for document in [top_level, nested] {
            let error = from_json_str_strict::<AdapterInspectionRequest>(document).unwrap_err();
            assert!(error.to_string().contains("duplicate JSON object key"));
        }
    }

    #[test]
    fn strict_json_rejects_unknown_fields_trailing_data_and_excessive_depth() {
        let unknown = r#"{"protocol":"ryeos.isolation-adapter/v1","target":"x86_64-unknown-linux-gnu","backend_id":"example","artifacts":{"launcher":3},"extra":true}"#;
        assert!(from_json_str_strict::<AdapterInspectionRequest>(unknown)
            .unwrap_err()
            .to_string()
            .contains("unknown field"));

        let valid = r#"{"protocol":"ryeos.isolation-adapter/v1","target":"x86_64-unknown-linux-gnu","backend_id":"example","artifacts":{"launcher":3}}"#;
        assert!(
            from_json_str_strict::<AdapterInspectionRequest>(&format!("{valid} true"))
                .unwrap_err()
                .to_string()
                .contains("trailing")
        );

        let deeply_nested = format!(
            "{}null{}",
            "[".repeat(MAX_JSON_DEPTH + 1),
            "]".repeat(MAX_JSON_DEPTH + 1)
        );
        assert!(from_json_str_strict::<Value>(&deeply_nested)
            .unwrap_err()
            .to_string()
            .contains("nesting exceeds"));
    }

    #[test]
    fn plan_validation_derives_the_exact_capability_set() {
        let (plan, authorities) = complete_plan();
        assert_eq!(
            plan.validate(&authorities).unwrap(),
            BTreeSet::from([
                IsolationCapability::FilesystemPrivateRoot,
                IsolationCapability::FilesystemFdReadOnly,
                IsolationCapability::FilesystemFdWritable,
                IsolationCapability::FilesystemOrderedOverlays,
                IsolationCapability::FilesystemPrivateTmp,
                IsolationCapability::DevicesMinimal,
                IsolationCapability::EnvironmentExact,
                IsolationCapability::NetworkIsolated,
                IsolationCapability::ProcessHostPidNamespace,
                IsolationCapability::ProcessTargetPidReporting,
                IsolationCapability::LifecycleSharedProcessGroup,
            ])
        );
    }

    #[test]
    fn plan_validation_rejects_unused_wrong_purpose_and_unordered_authorities() {
        let (plan, mut authorities) = complete_plan();
        authorities.push(authority(
            "unused",
            6,
            IsolationAuthorityPurpose::ReadOnlyMount,
        ));
        assert!(plan
            .validate(&authorities)
            .unwrap_err()
            .to_string()
            .contains("every inherited authority"));

        let (mut plan, authorities) = complete_plan();
        plan.mounts[1].access = IsolationMountAccess::Writable;
        assert!(plan
            .validate(&authorities)
            .unwrap_err()
            .to_string()
            .contains("purpose"));

        let (mut plan, authorities) = complete_plan();
        plan.mounts[2].layer = 0;
        assert!(plan
            .validate(&authorities)
            .unwrap_err()
            .to_string()
            .contains("deterministically ordered"));
    }

    #[test]
    fn plan_validation_requires_one_read_only_target_mount() {
        let (mut plan, authorities) = complete_plan();
        plan.mounts.remove(0);
        assert!(plan
            .validate(&authorities)
            .unwrap_err()
            .to_string()
            .contains("exactly one mount"));

        let (mut plan, mut authorities) = complete_plan();
        plan.mounts[0].access = IsolationMountAccess::Writable;
        authorities[0].purpose = IsolationAuthorityPurpose::WritableMount;
        assert!(plan
            .validate(&authorities)
            .unwrap_err()
            .to_string()
            .contains("target executable authority"));
    }

    #[test]
    fn launch_request_rejects_descriptor_reuse_across_roles() {
        let (plan, authorities) = complete_plan();
        let request = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::V1,
            plan,
            authorities,
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, 5)]),
            status_fd: 6,
        };
        assert!(request
            .validate()
            .unwrap_err()
            .to_string()
            .contains("reused across"));
    }

    #[test]
    fn protocol_collection_and_string_limits_are_independent() {
        let (plan, authorities) = complete_plan();

        let mut too_many_authorities = authorities.clone();
        too_many_authorities.resize(
            MAX_AUTHORITIES + 1,
            authority("overflow", 99, IsolationAuthorityPurpose::ReadOnlyMount),
        );
        assert!(plan
            .validate(&too_many_authorities)
            .unwrap_err()
            .to_string()
            .contains("too many authorities"));

        let (mut too_many_mounts, authorities) = complete_plan();
        too_many_mounts
            .mounts
            .resize(MAX_MOUNTS + 1, too_many_mounts.mounts[1].clone());
        assert!(too_many_mounts
            .validate(&authorities)
            .unwrap_err()
            .to_string()
            .contains("too many mounts"));

        let (mut too_many_arguments, authorities) = complete_plan();
        too_many_arguments
            .target
            .arguments
            .resize(MAX_ARGUMENTS + 1, "argument".to_string());
        assert!(too_many_arguments
            .validate(&authorities)
            .unwrap_err()
            .to_string()
            .contains("too many target arguments"));

        let (mut too_many_environment, authorities) = complete_plan();
        too_many_environment.environment.values = (0..=MAX_ENVIRONMENT_ENTRIES)
            .map(|index| (format!("KEY_{index}"), "value".to_string()))
            .collect();
        assert!(too_many_environment
            .validate(&authorities)
            .unwrap_err()
            .to_string()
            .contains("too many environment entries"));

        let (mut oversized_string, authorities) = complete_plan();
        oversized_string.target.argv0 = "x".repeat(MAX_STRING_BYTES + 1);
        assert!(oversized_string
            .validate(&authorities)
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
    }

    #[test]
    fn diagnostic_details_are_strict_and_bounded() {
        let diagnostic = IsolationDiagnostic {
            code: IsolationDiagnosticCode::InvalidRequest,
            message: "invalid".to_string(),
            details: (0..=MAX_DIAGNOSTIC_DETAILS)
                .map(|index| (format!("detail_{index}"), "value".to_string()))
                .collect(),
        };
        assert!(diagnostic
            .validate()
            .unwrap_err()
            .to_string()
            .contains("too many diagnostic"));
    }

    #[test]
    fn inspection_contract_validates_identity_descriptors_and_digests() {
        let request = AdapterInspectionRequest {
            protocol: IsolationAdapterProtocolVersion::V1,
            target: IsolationTargetTriple::X86_64UnknownLinuxGnu,
            backend_id: "invalid/example".to_string(),
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, 3)]),
        };
        assert!(request.validate().is_err());

        let response = AdapterInspectionResponse {
            protocol: IsolationAdapterProtocolVersion::V1,
            adapter_build: "0.1.0".to_string(),
            effective_capabilities: BTreeSet::from([IsolationCapability::FilesystemPrivateRoot]),
            artifacts: BTreeMap::from([(
                IsolationArtifactRole::Launcher,
                InspectedArtifact {
                    version: "example 1.0.0".to_string(),
                    digest: "A".repeat(64),
                },
            )]),
        };
        assert!(response
            .validate()
            .unwrap_err()
            .to_string()
            .contains("lowercase SHA-256"));
    }
}
