//! Strict, path-authority-free wire contracts shared by RyeOS isolation
//! engines, signed bundle manifests, and isolation adapters.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{DeserializeOwned, DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub const ISOLATION_ADAPTER_PROTOCOL: &str = "ryeos.isolation-adapter/v10";
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const MAX_WORKSPACE_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// One closed digest receipt fits atomically beside exactly one view descriptor.
/// Full workspace mutation JSON remains on the bounded stdout response channel.
pub const MAX_WORKSPACE_VIEW_RECEIPT_BYTES: usize = 512;
pub const MAX_AUTHORITIES: usize = 4096;
pub const MAX_MOUNTS: usize = 4096;
pub const MAX_ENVIRONMENT_ENTRIES: usize = 4096;
pub const MAX_ARGUMENTS: usize = 4096;
pub const MAX_STRING_BYTES: usize = 64 * 1024;
pub const MAX_DIAGNOSTIC_DETAILS: usize = 128;
pub const MAX_WORKSPACE_MUTATIONS: usize = 100_000;
pub const MAX_WORKSPACE_SYMLINK_TARGET_BYTES: usize = 4096;
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
    #[serde(rename = "ryeos.isolation-adapter/v10")]
    Current,
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
    #[serde(rename = "filesystem.fixed_parent_views")]
    FilesystemFixedParentViews,
    #[serde(rename = "filesystem.project_workspace_cow")]
    FilesystemProjectWorkspaceCow,
    #[serde(rename = "filesystem.workspace_delta")]
    FilesystemWorkspaceDelta,
    #[serde(rename = "filesystem.private_tmp")]
    FilesystemPrivateTmp,
    #[serde(rename = "filesystem.pid_namespace_proc")]
    FilesystemPidNamespaceProc,
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
    #[serde(rename = "process.isolated_pid_namespace")]
    ProcessIsolatedPidNamespace,
    #[serde(rename = "process.target_pid_reporting")]
    ProcessTargetPidReporting,
    #[serde(rename = "lifecycle.shared_process_group")]
    LifecycleSharedProcessGroup,
    #[serde(rename = "process.nested_sandbox")]
    ProcessNestedSandbox,
    #[serde(rename = "ipc.target_unix_stream")]
    IpcTargetUnixStream,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationAuthorityPurpose {
    ReadOnlyMount,
    WritableMount,
    Executable,
    RuntimeLibraryDirectory,
    WorkspaceProject,
    WorkspaceBackendState,
    WorkspaceView,
    WorkspaceViewDescendant,
    TargetDuplexChannel,
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

/// Mechanical bounds selected by node policy. The wire has no fallback values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedParentViewLimits {
    pub max_entries: usize,
    pub max_depth: usize,
}

impl FixedParentViewLimits {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.max_entries == 0
            || self.max_entries > MAX_MOUNTS
            || self.max_depth == 0
            || self.max_depth > MAX_JSON_DEPTH
        {
            return Err(ProtocolValidationError::new(
                "fixed-parent view bounds exceed the wire contract",
            ));
        }
        Ok(())
    }
}

/// Restriction on an existing exact directory mount. Connector entry membership
/// is fixed; allowed child mounts retain that directory's admitted RO/RW access.
/// Denied paths are authority input, not paths discovered by an adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationFixedParentView {
    pub destination: IsolationPath,
    pub denied_paths: Vec<String>,
    pub limits: FixedParentViewLimits,
}

impl IsolationFixedParentView {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        self.limits.validate()?;
        if self.denied_paths.is_empty() || self.denied_paths.len() > self.limits.max_entries {
            return Err(ProtocolValidationError::new(
                "fixed-parent view requires a bounded denial set",
            ));
        }
        let mut previous: Option<&str> = None;
        let mut denied = BTreeSet::new();
        let mut prefixes = BTreeSet::new();
        for path in &self.denied_paths {
            validate_string("fixed-parent denied path", path)?;
            let count = path.split('/').count();
            if count < 2
                || count > self.limits.max_depth
                || path.split('/').any(|part| matches!(part, "" | "." | ".."))
                || previous.is_some_and(|prev| prev >= path.as_str())
            {
                return Err(ProtocolValidationError::new(
                    "fixed-parent denied paths must be normalized, nonoverlapping and sorted beneath an ancestor",
                ));
            }
            for (offset, _) in path.match_indices('/') {
                let prefix = &path[..offset];
                if denied.contains(prefix) {
                    return Err(ProtocolValidationError::new(
                        "fixed-parent denied paths overlap",
                    ));
                }
                prefixes.insert(prefix);
            }
            denied.insert(path.as_str());
            prefixes.insert(path.as_str());
            if prefixes.len() > self.limits.max_entries {
                return Err(ProtocolValidationError::new(
                    "fixed-parent path tree exceeds entry bound",
                ));
            }
            previous = Some(path);
        }
        Ok(())
    }
}

/// One retained writable project view. Construction already happened once
/// under its workspace owner. A borrower clones this exact descriptor into
/// its fresh confinement namespace; it must not construct another filesystem
/// from lower/state paths. Ordinary mounts may not target the same path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationProjectWorkspace {
    pub workspace_id: String,
    pub view: IsolationAuthorityId,
    /// Digest of the exact descriptor's existing opaque directory identity.
    pub view_descriptor_identity: String,
    pub destination: IsolationPath,
    /// Exact writable aliases of directories belonging to this retained view.
    /// They are not host-located ordinary mount sources.
    pub writable_descendant_mounts: Vec<IsolationWorkspaceDescendantMount>,
}

/// One descriptor-proven member of the enclosing workspace. Access is always
/// writable at layer 10; callers cannot turn this relationship into another
/// mount class or select a different overlay ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationWorkspaceDescendantMount {
    pub source: IsolationAuthorityId,
    pub relative_path: String,
    pub destination: IsolationPath,
}

/// One daemon-owned connected duplex channel delivered at a fixed target
/// descriptor. The source authority is operational; the target descriptor and
/// environment name are the complete admitted target-side contract. A plan's
/// collection is target-fd sorted: fd 0 may carry a primary control stream,
/// while auxiliary channels use descriptors above stderr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationTargetChannel {
    pub source: IsolationAuthorityId,
    pub target_fd: u32,
    pub env_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationNetwork {
    Host,
    Isolated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationPidNamespace {
    Host,
    Isolated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationDeviceSurface {
    Minimal,
}

/// Finite node-owned process-filesystem surface, never an arbitrary host
/// mount. PID-only procfs requires an isolated PID namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationProcFilesystem {
    Empty,
    PidNamespace,
    /// Explicit broader kernel metadata and writable namespace maps. Host
    /// tasks remain invisible; requires the scoped nested-sandbox contract.
    PidNamespaceNested,
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
    pub fixed_parent_views: Vec<IsolationFixedParentView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_workspace: Option<IsolationProjectWorkspace>,
    pub target_channels: Vec<IsolationTargetChannel>,
    pub environment: IsolationEnvironment,
    pub network: IsolationNetwork,
    pub devices: IsolationDeviceSurface,
    pub private_tmp: bool,
    pub proc_filesystem: IsolationProcFilesystem,
    pub pid_namespace: IsolationPidNamespace,
    pub shared_process_group: bool,
    /// Explicit child-sandbox authority. Requires external whole-execution
    /// containment and PID-local proc; permits child namespace maps there.
    pub nested_sandbox: bool,
}

impl IsolationPlan {
    pub fn validate(
        &self,
        authorities: &[IsolationAuthority],
    ) -> Result<BTreeSet<IsolationCapability>, ProtocolValidationError> {
        if authorities.len() > MAX_AUTHORITIES {
            return Err(ProtocolValidationError::new("too many authorities"));
        }
        let descendant_count = self
            .project_workspace
            .as_ref()
            .map_or(0, |workspace| workspace.writable_descendant_mounts.len());
        if self.mounts.len().saturating_add(descendant_count) > MAX_MOUNTS {
            return Err(ProtocolValidationError::new("too many mounts"));
        }
        if self.nested_sandbox
            && (self.shared_process_group
                || self.proc_filesystem != IsolationProcFilesystem::PidNamespaceNested
                || self.pid_namespace != IsolationPidNamespace::Isolated)
        {
            return Err(ProtocolValidationError::new(
                "nested sandbox requires external whole-execution containment and isolated PID proc",
            ));
        }
        if !self.nested_sandbox
            && self.proc_filesystem == IsolationProcFilesystem::PidNamespaceNested
        {
            return Err(ProtocolValidationError::new(
                "nested proc requires explicit nested sandbox authority",
            ));
        }
        if self.fixed_parent_views.len() > MAX_MOUNTS {
            return Err(ProtocolValidationError::new("too many fixed-parent views"));
        }
        if self.proc_filesystem != IsolationProcFilesystem::Empty
            && self.pid_namespace != IsolationPidNamespace::Isolated
        {
            return Err(ProtocolValidationError::new(
                "PID procfs requires isolated PID namespace",
            ));
        }
        // Empty also means an empty surface, not permission to substitute an
        // arbitrary (possibly host) proc filesystem through ordinary mounts.
        {
            let proc_path = std::path::Path::new("/proc");
            if self.mounts.iter().any(|mount| {
                std::path::Path::new(mount.destination.as_str()).starts_with(proc_path)
            }) || self.project_workspace.as_ref().is_some_and(|workspace| {
                let destination = std::path::Path::new(workspace.destination.as_str());
                destination.starts_with(proc_path) || proc_path.starts_with(destination)
            }) {
                return Err(ProtocolValidationError::new(
                    "mount conflicts with reserved PID procfs",
                ));
            }
        }
        let mut destinations = BTreeSet::new();
        for mount in &self.mounts {
            if mount.destination.as_str() == "/" || !destinations.insert(&mount.destination) {
                return Err(ProtocolValidationError::new(
                    "mount destinations must be unique and cannot replace the private root",
                ));
            }
            for ancestor in &self.mounts {
                if mount.destination != ancestor.destination
                    && std::path::Path::new(mount.destination.as_str())
                        .starts_with(ancestor.destination.as_str())
                    && ancestor.layer > mount.layer
                {
                    return Err(ProtocolValidationError::new(
                        "mount ancestor would hide a child layer",
                    ));
                }
            }
            if self.project_workspace.as_ref().is_some_and(|workspace| {
                std::path::Path::new(workspace.destination.as_str())
                    .starts_with(mount.destination.as_str())
            }) {
                return Err(ProtocolValidationError::new(
                    "ordinary mount would hide the private workspace",
                ));
            }
        }
        let mut view_destinations = BTreeSet::new();
        let mut view_budget = 0usize;
        for view in &self.fixed_parent_views {
            view.validate()?;
            view_budget = view_budget
                .checked_add(view.limits.max_entries)
                .ok_or_else(|| ProtocolValidationError::new("fixed-parent budget overflow"))?;
            if view_budget > MAX_MOUNTS
                || !view_destinations.insert(&view.destination)
                || !self
                    .mounts
                    .iter()
                    .any(|mount| mount.destination == view.destination)
            {
                return Err(ProtocolValidationError::new(
                    "fixed-parent view lacks a unique mount or exceeds aggregate bounds",
                ));
            }
            for mount in &self.mounts {
                if mount.destination == view.destination {
                    continue;
                }
                for path in &view.denied_paths {
                    let denied = std::path::Path::new(view.destination.as_str()).join(path);
                    let destination = std::path::Path::new(mount.destination.as_str());
                    if destination.starts_with(&denied)
                        || (denied.starts_with(destination)
                            && destination.starts_with(view.destination.as_str()))
                    {
                        return Err(ProtocolValidationError::new(
                            "positive mount conflicts with fixed-parent restriction",
                        ));
                    }
                }
            }
        }
        if self.environment.values.len() > MAX_ENVIRONMENT_ENTRIES {
            return Err(ProtocolValidationError::new("too many environment entries"));
        }
        if self.target.arguments.len() > MAX_ARGUMENTS {
            return Err(ProtocolValidationError::new("too many target arguments"));
        }
        validate_string("argv0", &self.target.argv0)?;
        for argument in &self.target.arguments {
            validate_process_value("target argument", argument)?;
        }
        for (name, value) in &self.environment.values {
            validate_environment_name(name)?;
            validate_process_value("environment value", value)?;
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
                ));
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
        if let Some(workspace) = &self.project_workspace {
            validate_identifier("workspace id", &workspace.workspace_id)?;
            validate_sha256(
                "workspace descriptor identity",
                &workspace.view_descriptor_identity,
            )?;
            if authority_ids.get(&workspace.view) != Some(&IsolationAuthorityPurpose::WorkspaceView)
            {
                return Err(ProtocolValidationError::new(
                    "workspace view authority is missing or has the wrong purpose",
                ));
            }
            used_authorities.insert(workspace.view.clone());
            let mut previous_destination = None;
            let mut descendant_sources = BTreeSet::new();
            for descendant in &workspace.writable_descendant_mounts {
                validate_relative_path("workspace descendant", &descendant.relative_path)?;
                if authority_ids.get(&descendant.source)
                    != Some(&IsolationAuthorityPurpose::WorkspaceViewDescendant)
                    || !descendant_sources.insert(&descendant.source)
                {
                    return Err(ProtocolValidationError::new(
                        "workspace descendant requires one unique descendant authority",
                    ));
                }
                if previous_destination.is_some_and(|previous| previous >= &descendant.destination)
                {
                    return Err(ProtocolValidationError::new(
                        "workspace descendants must have unique destination-sorted entries",
                    ));
                }
                previous_destination = Some(&descendant.destination);
                let destination = std::path::Path::new(descendant.destination.as_str());
                let overlaps = |other: &IsolationPath| {
                    let other = std::path::Path::new(other.as_str());
                    destination.starts_with(other) || other.starts_with(destination)
                };
                if destination == std::path::Path::new("/")
                    || destination.starts_with("/proc")
                    || overlaps(&workspace.destination)
                    || self.mounts.iter().any(|mount| overlaps(&mount.destination))
                    || workspace.writable_descendant_mounts.iter().any(|other| {
                        other.source != descendant.source && overlaps(&other.destination)
                    })
                {
                    return Err(ProtocolValidationError::new(
                        "workspace descendant destination conflicts with another namespace authority",
                    ));
                }
                used_authorities.insert(descendant.source.clone());
            }
            if self
                .mounts
                .iter()
                .any(|mount| mount.destination == workspace.destination)
            {
                return Err(ProtocolValidationError::new(
                    "project workspace destination conflicts with an ordinary mount",
                ));
            }
        }
        if self.target_channels.len() > MAX_AUTHORITIES {
            return Err(ProtocolValidationError::new("too many target channels"));
        }
        let mut previous_target_fd = None;
        let mut channel_sources = BTreeSet::new();
        let mut channel_environment = BTreeSet::new();
        for channel in &self.target_channels {
            if authority_ids.get(&channel.source)
                != Some(&IsolationAuthorityPurpose::TargetDuplexChannel)
            {
                return Err(ProtocolValidationError::new(
                    "target channel authority is missing or has the wrong purpose",
                ));
            }
            if matches!(channel.target_fd, 1 | 2) {
                return Err(ProtocolValidationError::new(
                    "target channel cannot replace stdout or stderr",
                ));
            }
            if previous_target_fd.is_some_and(|previous| previous >= channel.target_fd) {
                return Err(ProtocolValidationError::new(
                    "target channels must be unique and sorted by target descriptor",
                ));
            }
            previous_target_fd = Some(channel.target_fd);
            if !channel_sources.insert(channel.source.clone()) {
                return Err(ProtocolValidationError::new(
                    "target channel source authority is duplicated",
                ));
            }
            validate_environment_name(&channel.env_name)?;
            if !channel_environment.insert(channel.env_name.as_str()) {
                return Err(ProtocolValidationError::new(
                    "target channel environment name is duplicated",
                ));
            }
            let expected_target_fd = channel.target_fd.to_string();
            if self
                .environment
                .values
                .get(&channel.env_name)
                .map(String::as_str)
                != Some(expected_target_fd.as_str())
            {
                return Err(ProtocolValidationError::new(
                    "target channel environment must name its exact target descriptor",
                ));
            }
            if self
                .mounts
                .iter()
                .any(|mount| mount.source == channel.source)
                || self.target.executable == channel.source
                || self
                    .project_workspace
                    .as_ref()
                    .is_some_and(|workspace| workspace.view == channel.source)
            {
                return Err(ProtocolValidationError::new(
                    "target channel authority cannot supply filesystem or executable authority",
                ));
            }
            used_authorities.insert(channel.source.clone());
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
            || self
                .project_workspace
                .as_ref()
                .is_some_and(|workspace| !workspace.writable_descendant_mounts.is_empty())
        {
            capabilities.insert(IsolationCapability::FilesystemFdWritable);
        }
        if self.project_workspace.is_some() {
            capabilities.insert(IsolationCapability::FilesystemProjectWorkspaceCow);
            capabilities.insert(IsolationCapability::FilesystemWorkspaceDelta);
        }
        if !self.fixed_parent_views.is_empty() {
            capabilities.insert(IsolationCapability::FilesystemFixedParentViews);
        }
        if !self.target_channels.is_empty() {
            capabilities.insert(IsolationCapability::IpcTargetUnixStream);
        }
        if self.private_tmp {
            capabilities.insert(IsolationCapability::FilesystemPrivateTmp);
        }
        if self.proc_filesystem != IsolationProcFilesystem::Empty {
            capabilities.insert(IsolationCapability::FilesystemPidNamespaceProc);
        }
        capabilities.insert(match self.network {
            IsolationNetwork::Host => IsolationCapability::NetworkHost,
            IsolationNetwork::Isolated => IsolationCapability::NetworkIsolated,
        });
        capabilities.insert(match self.pid_namespace {
            IsolationPidNamespace::Host => IsolationCapability::ProcessHostPidNamespace,
            IsolationPidNamespace::Isolated => IsolationCapability::ProcessIsolatedPidNamespace,
        });
        if self.shared_process_group {
            capabilities.insert(IsolationCapability::LifecycleSharedProcessGroup);
        }
        if self.nested_sandbox {
            capabilities.insert(IsolationCapability::ProcessNestedSandbox);
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
    /// Exact signed adapter executable retained by the daemon. The adapter
    /// verifies that this handle identifies its current image before using a
    /// duplicate as the sandbox-side sealed-argv bridge.
    pub adapter_fd: u32,
    pub status_fd: u32,
    pub lifecycle: AdapterLaunchLifecycle,
}

/// Exact target lifecycle requested from an isolation adapter.
///
/// A normal launch carries no attachment descriptors and must let the target
/// run as soon as the backend is ready. An attachment launch reports its target
/// while that target is held at the final backend boundary. The distinct wire
/// variants deliberately make the two contracts impossible to infer from
/// optional fields or descriptor presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdapterLaunchLifecycle {
    Run,
    AwaitAttachment {
        /// Read end of the daemon-owned one-shot release boundary. The backend
        /// must consume it immediately before target exec.
        release_fd: u32,
        /// Child-owned duplicate of the release writer. This prevents parent
        /// death from becoming EOF on a backend whose boundary treats EOF as a
        /// completed wait.
        release_keepalive_fd: u32,
    },
}

/// Adapter-owned lifecycle operations for one durable project workspace.
/// The descriptor authorities are the only filesystem inputs: host paths are
/// deliberately absent from this contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceLifecycleOperation {
    Create,
    FreezeAndDiff,
    Destroy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterWorkspaceRequest {
    pub protocol: IsolationAdapterProtocolVersion,
    pub operation: WorkspaceLifecycleOperation,
    pub workspace_id: String,
    /// Canonical JSON form of the RyeOS LaunchOwner. The adapter treats it as
    /// an opaque fencing identity and echoes it in the response.
    pub launch_owner: String,
    pub base_snapshot: String,
    pub authorities: Vec<IsolationAuthority>,
    /// Only Create may supply the one-shot descriptor-transfer endpoint.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub transfer_fd: Option<u32>,
    /// Existing bound incarnation. Null only for Create or separately proved
    /// cleanup of a construction which never acquired a bound view.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub mount_identity: Option<String>,
}

impl AdapterWorkspaceRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_identifier("workspace id", &self.workspace_id)?;
        validate_string("launch owner", &self.launch_owner)?;
        validate_sha256("base snapshot", &self.base_snapshot)?;
        if self.authorities.len() != 2 {
            return Err(ProtocolValidationError::new(
                "workspace lifecycle requires exactly project and backend-state authorities",
            ));
        }
        let mut purposes = BTreeSet::new();
        let mut descriptors = BTreeSet::new();
        for authority in &self.authorities {
            if authority.inherited_fd <= 2 || !descriptors.insert(authority.inherited_fd) {
                return Err(ProtocolValidationError::new(
                    "workspace authority descriptors must be unique and must not overlap stdio",
                ));
            }
            if !matches!(
                authority.purpose,
                IsolationAuthorityPurpose::WorkspaceProject
                    | IsolationAuthorityPurpose::WorkspaceBackendState
            ) || !purposes.insert(authority.purpose)
            {
                return Err(ProtocolValidationError::new(
                    "workspace lifecycle authority purposes must be exactly project and backend state",
                ));
            }
        }
        if let Some(identity) = &self.mount_identity {
            validate_sha256("workspace mount identity", identity)?;
        }
        match self.operation {
            WorkspaceLifecycleOperation::Create => {
                if self.mount_identity.is_some()
                    || self
                        .transfer_fd
                        .is_none_or(|fd| fd <= 2 || descriptors.contains(&fd))
                {
                    return Err(ProtocolValidationError::new(
                        "workspace Create requires one distinct transfer endpoint and no prior mount identity",
                    ));
                }
            }
            WorkspaceLifecycleOperation::FreezeAndDiff => {
                if self.transfer_fd.is_some() || self.mount_identity.is_none() {
                    return Err(ProtocolValidationError::new(
                        "workspace diff requires a bound mount identity and cannot transfer a new view",
                    ));
                }
            }
            WorkspaceLifecycleOperation::Destroy => {
                if self.transfer_fd.is_some() {
                    return Err(ProtocolValidationError::new(
                        "workspace destruction cannot transfer a new view",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Explicit null is part of the current protocol, not a missing-field predecessor fallback.
fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// The payload in the one-shot Create packet. Both digests are bare lowercase
/// SHA-256 over Lillux canonical JSON of the respective typed protocol value.
/// The packet is also canonical JSON and must carry exactly one descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceViewTransferReceipt {
    pub protocol: IsolationAdapterProtocolVersion,
    pub request_digest: String,
    pub response_digest: String,
}

impl WorkspaceViewTransferReceipt {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_sha256("workspace request digest", &self.request_digest)?;
        validate_sha256("workspace response digest", &self.response_digest)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMutationKind {
    UpsertRegular,
    UpsertSymlink,
    DeletePath,
    EnsureDirectory,
    OpaqueDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMutation {
    pub path: String,
    pub kind: WorkspaceMutationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Exact relative link bytes, never a followed target or a content digest.
    /// Required-null on every non-symlink mutation in the current protocol.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub target: Option<String>,
}

impl WorkspaceMutation {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_relative_path("workspace mutation path", &self.path)?;
        match self.kind {
            WorkspaceMutationKind::UpsertRegular => {
                if !matches!(self.normalized_mode, Some(0o644 | 0o755))
                    || self.target.is_some()
                    || self.size.is_none()
                    || self
                        .content_hash
                        .as_deref()
                        .is_none_or(|hash| validate_sha256("workspace content hash", hash).is_err())
                {
                    return Err(ProtocolValidationError::new(
                        "regular workspace mutation requires mode, size, and content hash",
                    ));
                }
                Ok(())
            }
            WorkspaceMutationKind::UpsertSymlink => {
                let target = self.target.as_deref().ok_or_else(|| {
                    ProtocolValidationError::new("symlink workspace mutation requires target bytes")
                })?;
                // Containment belongs to the declared output root, not the
                // overlay's project-relative path. Preserve '..' for that
                // owner to resolve within the selected product subtree.
                if target.is_empty()
                    || target.len() > MAX_WORKSPACE_SYMLINK_TARGET_BYTES
                    || target.starts_with('/')
                    || target.as_bytes().contains(&0)
                    || self.normalized_mode.is_some()
                    || self.size.is_some()
                    || self.content_hash.is_some()
                {
                    return Err(ProtocolValidationError::new(
                        "invalid symlink workspace mutation",
                    ));
                }
                Ok(())
            }
            _ if self.normalized_mode.is_none()
                && self.size.is_none()
                && self.content_hash.is_none()
                && self.target.is_none() =>
            {
                Ok(())
            }
            _ => Err(ProtocolValidationError::new(
                "non-regular workspace mutation cannot carry file metadata",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterWorkspaceResponse {
    pub protocol: IsolationAdapterProtocolVersion,
    pub operation: WorkspaceLifecycleOperation,
    pub workspace_id: String,
    pub launch_owner: String,
    pub backend_id: String,
    pub backend_version: String,
    pub pinned_root_identities: BTreeMap<String, String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub mount_identity: Option<String>,
    /// Present only when Create delivers an exact newly constructed view.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub view_descriptor_identity: Option<String>,
    /// Adapter-declared, backend-relative root containing the bytes named by
    /// `mutations`. Present only for `freeze_and_diff`; RyeOS resolves and pins
    /// it below the still-open opaque backend-state authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_content_root: Option<String>,
    pub mutations: Vec<WorkspaceMutation>,
    pub destroyed: bool,
}

impl AdapterWorkspaceResponse {
    /// Sole field-level encoding for the created mount incarnation. Callers
    /// hash this value with the existing Lillux canonical JSON/SHA-256 owner.
    /// The mutable directory metadata and the response's own digest are not
    /// identity inputs. Construction ownership prevents a later directory/
    /// kernel-coordinate reuse from masquerading as the accepted incarnation.
    pub fn mount_identity_value(
        &self,
        request: &AdapterWorkspaceRequest,
    ) -> Result<Value, ProtocolValidationError> {
        if self.operation != WorkspaceLifecycleOperation::Create
            || request.operation != WorkspaceLifecycleOperation::Create
        {
            return Err(ProtocolValidationError::new(
                "only Create constructs a mount identity",
            ));
        }
        let identity = self.view_descriptor_identity.as_deref().ok_or_else(|| {
            ProtocolValidationError::new("created view lacks descriptor identity")
        })?;
        validate_sha256("workspace descriptor identity", identity)?;
        Ok(serde_json::json!({
            "protocol": request.protocol,
            "workspace_id": request.workspace_id,
            "launch_owner": request.launch_owner,
            "base_snapshot": request.base_snapshot,
            "backend_id": self.backend_id,
            "backend_version": self.backend_version,
            "pinned_root_identities": self.pinned_root_identities,
            "view_descriptor_identity": identity,
        }))
    }

    pub fn validate_for(
        &self,
        request: &AdapterWorkspaceRequest,
    ) -> Result<(), ProtocolValidationError> {
        if self.protocol != request.protocol
            || self.operation != request.operation
            || self.workspace_id != request.workspace_id
            || self.launch_owner != request.launch_owner
        {
            return Err(ProtocolValidationError::new(
                "workspace lifecycle response does not match its request",
            ));
        }
        validate_identifier("workspace backend id", &self.backend_id)?;
        validate_string("workspace backend version", &self.backend_version)?;
        if let Some(identity) = &self.mount_identity {
            validate_sha256("workspace mount identity", identity)?;
        }
        if self.operation == WorkspaceLifecycleOperation::Create {
            if self.mount_identity.is_none() {
                return Err(ProtocolValidationError::new(
                    "created workspace must identify its mount incarnation",
                ));
            }
            validate_sha256(
                "workspace descriptor identity",
                self.view_descriptor_identity.as_deref().ok_or_else(|| {
                    ProtocolValidationError::new(
                        "created workspace must identify its transferred descriptor",
                    )
                })?,
            )?;
        } else if self.view_descriptor_identity.is_some()
            || self.mount_identity != request.mount_identity
        {
            return Err(ProtocolValidationError::new(
                "workspace operation cannot introduce or replace a view identity",
            ));
        }
        if self.pinned_root_identities.len() != 2
            || !["project", "backend_state"]
                .iter()
                .all(|name| self.pinned_root_identities.contains_key(*name))
        {
            return Err(ProtocolValidationError::new(
                "workspace lifecycle response must identify project and backend-state roots",
            ));
        }
        if let Some(root) = &self.mutation_content_root {
            validate_relative_path("workspace mutation content root", root)?;
        }
        if self.mutations.len() > MAX_WORKSPACE_MUTATIONS {
            return Err(ProtocolValidationError::new(
                "workspace mutation count exceeds protocol limit",
            ));
        }
        let mut previous_path: Option<&str> = None;
        let mut non_directory_paths = BTreeSet::new();
        for mutation in &self.mutations {
            mutation.validate()?;
            if previous_path.is_some_and(|previous| previous >= mutation.path.as_str()) {
                return Err(ProtocolValidationError::new(
                    "workspace mutations must be unique and canonically path-sorted",
                ));
            }
            let components = mutation.path.split('/').collect::<Vec<_>>();
            for end in 1..components.len() {
                let ancestor = components[..end].join("/");
                if non_directory_paths.contains(&ancestor) {
                    return Err(ProtocolValidationError::new(
                        "workspace mutation descends through a non-directory mutation",
                    ));
                }
            }
            if matches!(
                mutation.kind,
                WorkspaceMutationKind::UpsertRegular
                    | WorkspaceMutationKind::UpsertSymlink
                    | WorkspaceMutationKind::DeletePath
            ) {
                non_directory_paths.insert(mutation.path.clone());
            }
            previous_path = Some(&mutation.path);
        }
        match self.operation {
            WorkspaceLifecycleOperation::Create
                if self.destroyed
                    || !self.mutations.is_empty()
                    || self.mutation_content_root.is_some() =>
            {
                Err(ProtocolValidationError::new(
                    "workspace create response has an invalid result shape",
                ))
            }
            WorkspaceLifecycleOperation::FreezeAndDiff
                if self.destroyed || self.mutation_content_root.is_none() =>
            {
                Err(ProtocolValidationError::new(
                    "workspace diff must identify its mutation-content root without reporting destruction",
                ))
            }
            WorkspaceLifecycleOperation::Destroy
                if !self.destroyed
                    || !self.mutations.is_empty()
                    || self.mutation_content_root.is_some() =>
            {
                Err(ProtocolValidationError::new(
                    "workspace destroy response has an invalid result shape",
                ))
            }
            _ => Ok(()),
        }
    }
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
        if self.adapter_fd <= 2 || !descriptors.insert(self.adapter_fd) {
            return Err(ProtocolValidationError::new(
                "adapter descriptor overlaps stdio or another isolation protocol role",
            ));
        }
        if let AdapterLaunchLifecycle::AwaitAttachment {
            release_fd,
            release_keepalive_fd,
        } = self.lifecycle
        {
            if release_fd <= 2 || !descriptors.insert(release_fd) {
                return Err(ProtocolValidationError::new(
                    "attachment release descriptor overlaps stdio or another isolation protocol role",
                ));
            }
            if release_keepalive_fd <= 2 || !descriptors.insert(release_keepalive_fd) {
                return Err(ProtocolValidationError::new(
                    "attachment release keepalive descriptor overlaps stdio or another isolation protocol role",
                ));
            }
        }
        for authority in &self.authorities {
            if !descriptors.insert(authority.inherited_fd) {
                return Err(ProtocolValidationError::new(
                    "descriptor is reused across isolation protocol roles",
                ));
            }
        }
        for channel in &self.plan.target_channels {
            let source_fd = self
                .authorities
                .iter()
                .find(|authority| authority.id == channel.source)
                .map(|authority| authority.inherited_fd)
                .ok_or_else(|| {
                    ProtocolValidationError::new(
                        "target channel source disappeared after plan validation",
                    )
                })?;
            if channel.target_fd > 2
                && channel.target_fd != source_fd
                && descriptors.contains(&channel.target_fd)
            {
                return Err(ProtocolValidationError::new(
                    "target channel destination aliases an inherited protocol descriptor",
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
    validate_process_value(kind, value)
}

// argv entries and environment values are exact process data, not identifiers.
// Empty is meaningful (including an explicitly empty PATH) and must not be
// replaced with absence, an inherited value, or a fabricated sentinel. Names,
// argv0 and authority coordinates still use the nonempty validator above.
fn validate_process_value(kind: &str, value: &str) -> Result<(), ProtocolValidationError> {
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

fn validate_sha256(kind: &str, value: &str) -> Result<(), ProtocolValidationError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ProtocolValidationError::new(format!(
            "{kind} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn validate_relative_path(kind: &str, value: &str) -> Result<(), ProtocolValidationError> {
    validate_string(kind, value)?;
    if value.starts_with('/')
        || value.ends_with('/')
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(ProtocolValidationError::new(format!(
            "{kind} is not a canonical relative path"
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
                fixed_parent_views: Vec::new(),
                project_workspace: None,
                target_channels: Vec::new(),
                environment: IsolationEnvironment {
                    values: BTreeMap::from([("PATH".to_string(), "/bin".to_string())]),
                },
                network: IsolationNetwork::Isolated,
                devices: IsolationDeviceSurface::Minimal,
                private_tmp: true,
                proc_filesystem: IsolationProcFilesystem::Empty,
                pid_namespace: IsolationPidNamespace::Host,
                shared_process_group: true,
                nested_sandbox: false,
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
    fn project_workspace_requires_one_retained_view_role() {
        let (mut plan, mut authorities) = complete_plan();
        let view = IsolationAuthorityId::new("workspace-view").unwrap();
        plan.mounts
            .retain(|mount| mount.destination.as_str() != "/workspace");
        authorities.retain(|authority| authority.id.as_str() != "workspace");
        authorities.push(authority(
            "workspace-view",
            5,
            IsolationAuthorityPurpose::WorkspaceView,
        ));
        plan.project_workspace = Some(IsolationProjectWorkspace {
            workspace_id: "workspace-one".to_string(),
            view,
            view_descriptor_identity: "c".repeat(64),
            destination: IsolationPath::new("/workspace").unwrap(),
            writable_descendant_mounts: Vec::new(),
        });
        plan.validate(&authorities).unwrap();
        authorities.last_mut().unwrap().purpose = IsolationAuthorityPurpose::WorkspaceProject;
        assert!(plan.validate(&authorities).is_err());
        authorities.last_mut().unwrap().purpose = IsolationAuthorityPurpose::WorkspaceView;
        plan.project_workspace
            .as_mut()
            .unwrap()
            .view_descriptor_identity = "not-a-digest".into();
        assert!(plan.validate(&authorities).is_err());
    }

    fn workspace_descendant_plan() -> (IsolationPlan, Vec<IsolationAuthority>) {
        let (mut plan, mut authorities) = complete_plan();
        plan.mounts
            .retain(|mount| mount.destination.as_str() != "/workspace");
        authorities.retain(|authority| authority.id.as_str() != "workspace");
        authorities.push(authority(
            "view",
            5,
            IsolationAuthorityPurpose::WorkspaceView,
        ));
        authorities.push(authority(
            "cache",
            6,
            IsolationAuthorityPurpose::WorkspaceViewDescendant,
        ));
        plan.project_workspace = Some(IsolationProjectWorkspace {
            workspace_id: "workspace-one".into(),
            view: IsolationAuthorityId::new("view").unwrap(),
            view_descriptor_identity: "c".repeat(64),
            destination: IsolationPath::new("/workspace").unwrap(),
            writable_descendant_mounts: vec![IsolationWorkspaceDescendantMount {
                source: IsolationAuthorityId::new("cache").unwrap(),
                relative_path: "cache/one".into(),
                destination: IsolationPath::new("/runtime/cache").unwrap(),
            }],
        });
        (plan, authorities)
    }

    #[test]
    fn workspace_descendants_require_explicit_closed_wire_and_writable_capability() {
        let (plan, authorities) = workspace_descendant_plan();
        assert!(
            plan.mounts
                .iter()
                .all(|mount| mount.access == IsolationMountAccess::ReadOnly)
        );
        let capabilities = plan.validate(&authorities).unwrap();
        assert!(capabilities.contains(&IsolationCapability::FilesystemFdWritable));
        let encoded = serde_json::to_value(&plan).unwrap();
        let decoded: IsolationPlan = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded, plan);
        for field in ["access", "layer"] {
            let mut malformed = encoded.clone();
            malformed["project_workspace"]["writable_descendant_mounts"][0][field] =
                serde_json::json!("not-authorable");
            assert!(serde_json::from_value::<IsolationPlan>(malformed).is_err());
        }
        let mut missing = encoded;
        missing["project_workspace"]
            .as_object_mut()
            .unwrap()
            .remove("writable_descendant_mounts");
        assert!(serde_json::from_value::<IsolationPlan>(missing).is_err());
    }

    #[test]
    fn workspace_descendants_refuse_role_reuse_or_orphan_authority() {
        let (plan, authorities) = workspace_descendant_plan();
        plan.validate(&authorities).unwrap();
        for purpose in [
            IsolationAuthorityPurpose::WritableMount,
            IsolationAuthorityPurpose::ReadOnlyMount,
            IsolationAuthorityPurpose::WorkspaceView,
            IsolationAuthorityPurpose::TargetDuplexChannel,
        ] {
            let mut wrong = authorities.clone();
            wrong.last_mut().unwrap().purpose = purpose;
            assert!(plan.validate(&wrong).is_err());
        }
        let mut wrong = authorities.clone();
        wrong.last_mut().unwrap().inherited_fd = 3;
        assert!(plan.validate(&wrong).is_err());
        let mut orphan = plan.clone();
        orphan.project_workspace = None;
        assert!(orphan.validate(&authorities).is_err());
        let mut unused = plan.clone();
        unused
            .project_workspace
            .as_mut()
            .unwrap()
            .writable_descendant_mounts
            .clear();
        assert!(unused.validate(&authorities).is_err());
        let mut unknown = plan.clone();
        unknown
            .project_workspace
            .as_mut()
            .unwrap()
            .writable_descendant_mounts[0]
            .source = IsolationAuthorityId::new("unknown").unwrap();
        assert!(unknown.validate(&authorities).is_err());
        let mut ordinary = plan;
        ordinary.mounts.push(IsolationMount {
            source: IsolationAuthorityId::new("cache").unwrap(),
            destination: IsolationPath::new("/ordinary-cache").unwrap(),
            access: IsolationMountAccess::Writable,
            layer: 10,
        });
        assert!(ordinary.validate(&authorities).is_err());
    }

    #[test]
    fn workspace_descendants_refuse_unsafe_paths_and_namespace_conflicts() {
        let (plan, authorities) = workspace_descendant_plan();
        for relative in [
            "", ".", "../x", "a/../b", "a/./b", "a//b", "a/", "/a", "a\nb",
        ] {
            let mut invalid = plan.clone();
            invalid
                .project_workspace
                .as_mut()
                .unwrap()
                .writable_descendant_mounts[0]
                .relative_path = relative.into();
            assert!(invalid.validate(&authorities).is_err(), "{relative:?}");
        }
        for destination in [
            "/",
            "/proc",
            "/proc/x",
            "/workspace",
            "/workspace/child",
            "/project",
            "/project/hidden",
            "/bin",
            "/bin/python",
            "/bin/python/child",
        ] {
            let mut invalid = plan.clone();
            invalid
                .project_workspace
                .as_mut()
                .unwrap()
                .writable_descendant_mounts[0]
                .destination = IsolationPath::new(destination).unwrap();
            assert!(invalid.validate(&authorities).is_err(), "{destination}");
        }
        let mut excessive = plan;
        excessive
            .mounts
            .resize(MAX_MOUNTS, excessive.mounts[0].clone());
        assert!(
            excessive
                .validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("too many mounts")
        );
    }

    #[test]
    fn workspace_descendants_require_disjoint_sorted_unique_mounts() {
        let (plan, mut authorities) = workspace_descendant_plan();
        authorities.push(authority(
            "cache-two",
            7,
            IsolationAuthorityPurpose::WorkspaceViewDescendant,
        ));
        for destination in [
            "/runtime/cache",
            "/runtime/cache/child",
            "/runtime",
            "/runtime/aaa",
        ] {
            let mut invalid = plan.clone();
            invalid
                .project_workspace
                .as_mut()
                .unwrap()
                .writable_descendant_mounts
                .push(IsolationWorkspaceDescendantMount {
                    source: IsolationAuthorityId::new("cache-two").unwrap(),
                    relative_path: "cache/two".into(),
                    destination: IsolationPath::new(destination).unwrap(),
                });
            assert!(invalid.validate(&authorities).is_err(), "{destination}");
        }
        let mut valid = plan;
        valid
            .project_workspace
            .as_mut()
            .unwrap()
            .writable_descendant_mounts
            .push(IsolationWorkspaceDescendantMount {
                source: IsolationAuthorityId::new("cache-two").unwrap(),
                relative_path: "cache/two".into(),
                destination: IsolationPath::new("/runtime/zzz").unwrap(),
            });
        valid.validate(&authorities).unwrap();
        valid
            .project_workspace
            .as_mut()
            .unwrap()
            .writable_descendant_mounts[1]
            .source = IsolationAuthorityId::new("cache").unwrap();
        assert!(valid.validate(&authorities).is_err());
    }

    #[test]
    fn exact_process_values_allow_empty_without_relaxing_identity_or_byte_bounds() {
        let (mut plan, authorities) = complete_plan();
        plan.target.arguments.push(String::new());
        plan.environment.values.insert("PATH".into(), String::new());
        plan.environment
            .values
            .insert("PYTHONPATH".into(), String::new());
        plan.validate(&authorities).unwrap();
        let roundtrip: IsolationPlan =
            serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
        assert_eq!(roundtrip.target.arguments.last().unwrap(), "");
        assert_eq!(roundtrip.environment.values.get("PATH").unwrap(), "");
        roundtrip.validate(&authorities).unwrap();

        let mut unnamed = plan.clone();
        unnamed
            .environment
            .values
            .insert(String::new(), "value".into());
        assert!(
            unnamed
                .validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("environment name cannot be empty")
        );
        let mut no_argv0 = plan.clone();
        no_argv0.target.argv0.clear();
        assert!(
            no_argv0
                .validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("argv0 cannot be empty")
        );

        for invalid in ["bad\0value".to_owned(), "x".repeat(MAX_STRING_BYTES + 1)] {
            let mut argument = plan.clone();
            argument.target.arguments.push(invalid.clone());
            assert!(argument.validate(&authorities).is_err());
            let mut environment = plan.clone();
            environment
                .environment
                .values
                .insert("VALUE".into(), invalid);
            assert!(environment.validate(&authorities).is_err());
        }
    }

    #[test]
    fn declaration_allows_a_self_contained_adapter() {
        let declaration = IsolationBackendDeclaration {
            id: "example".to_string(),
            protocol: IsolationAdapterProtocolVersion::Current,
            targets: vec![IsolationTargetTriple::X86_64UnknownLinuxGnu],
            adapter: "adapter".to_string(),
            artifacts: BTreeMap::new(),
            capabilities: BTreeSet::from([IsolationCapability::FilesystemPrivateRoot]),
        };
        declaration.validate().unwrap();
    }

    #[test]
    fn declaration_requires_external_artifacts_to_be_distinct_from_adapter() {
        let declaration = IsolationBackendDeclaration {
            id: "example".to_string(),
            protocol: IsolationAdapterProtocolVersion::Current,
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
            protocol: IsolationAdapterProtocolVersion::Current,
            targets: vec![
                IsolationTargetTriple::X86_64UnknownLinuxGnu,
                IsolationTargetTriple::X86_64UnknownLinuxGnu,
            ],
            adapter: "adapter".to_string(),
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, "launcher".to_string())]),
            capabilities: BTreeSet::from([IsolationCapability::FilesystemPrivateRoot]),
        };
        assert!(
            declaration
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate target")
        );

        declaration.targets.truncate(1);
        declaration.adapter = "../adapter".to_string();
        assert!(declaration.validate().is_err());
    }

    #[test]
    fn signed_declaration_is_the_capability_upper_bound() {
        let declaration = IsolationBackendDeclaration {
            id: "linux".to_string(),
            protocol: IsolationAdapterProtocolVersion::Current,
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
        let top_level = r#"{"protocol":"ryeos.isolation-adapter/v10","protocol":"ryeos.isolation-adapter/v10","target":"x86_64-unknown-linux-gnu","backend_id":"example","artifacts":{"launcher":3}}"#;
        let nested = r#"{"protocol":"ryeos.isolation-adapter/v10","target":"x86_64-unknown-linux-gnu","backend_id":"example","artifacts":{"launcher":3,"launcher":4}}"#;
        for document in [top_level, nested] {
            let error = from_json_str_strict::<AdapterInspectionRequest>(document).unwrap_err();
            assert!(error.to_string().contains("duplicate JSON object key"));
        }
    }

    #[test]
    fn strict_json_rejects_unknown_fields_trailing_data_and_excessive_depth() {
        let unknown = r#"{"protocol":"ryeos.isolation-adapter/v10","target":"x86_64-unknown-linux-gnu","backend_id":"example","artifacts":{"launcher":3},"extra":true}"#;
        assert!(
            from_json_str_strict::<AdapterInspectionRequest>(unknown)
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );

        let valid = r#"{"protocol":"ryeos.isolation-adapter/v10","target":"x86_64-unknown-linux-gnu","backend_id":"example","artifacts":{"launcher":3}}"#;
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
        assert!(
            from_json_str_strict::<Value>(&deeply_nested)
                .unwrap_err()
                .to_string()
                .contains("nesting exceeds")
        );
    }

    #[test]
    fn predecessor_adapter_protocol_is_refused() {
        for version in [8, 9] {
            let predecessor = format!(
                r#"{{"protocol":"ryeos.isolation-adapter/v{version}","target":"x86_64-unknown-linux-gnu","backend_id":"example","artifacts":{{"launcher":3}}}}"#
            );
            let error = from_json_str_strict::<AdapterInspectionRequest>(&predecessor).unwrap_err();
            assert!(error.to_string().contains("unknown variant"));
        }
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

    fn fixed_view() -> IsolationFixedParentView {
        IsolationFixedParentView {
            destination: IsolationPath::new("/project").unwrap(),
            denied_paths: vec!["control/secret".to_string()],
            limits: FixedParentViewLimits {
                max_entries: 32,
                max_depth: 4,
            },
        }
    }

    #[test]
    fn nested_sandbox_requires_explicit_scoped_pid_contract() {
        let (mut plan, authorities) = complete_plan();
        let mut missing = serde_json::to_value(&plan).unwrap();
        missing.as_object_mut().unwrap().remove("nested_sandbox");
        assert!(serde_json::from_value::<IsolationPlan>(missing).is_err());
        plan.nested_sandbox = true;
        assert!(plan.validate(&authorities).is_err());
        plan.pid_namespace = IsolationPidNamespace::Isolated;
        plan.proc_filesystem = IsolationProcFilesystem::PidNamespaceNested;
        assert!(
            plan.validate(&authorities).is_err(),
            "a shared group cannot contain nested execution"
        );
        plan.shared_process_group = false;
        assert!(
            plan.validate(&authorities)
                .unwrap()
                .contains(&IsolationCapability::ProcessNestedSandbox)
        );
        plan.nested_sandbox = false;
        assert!(plan.validate(&authorities).is_err());
        plan.proc_filesystem = IsolationProcFilesystem::PidNamespace;
        assert!(
            !plan
                .validate(&authorities)
                .unwrap()
                .contains(&IsolationCapability::ProcessNestedSandbox),
            "scope containment alone does not permit nested sandboxing"
        );
    }

    #[test]
    fn pid_proc_is_explicit_capability_gated_and_cannot_be_replaced() {
        let (mut plan, authorities) = complete_plan();
        let mut missing = serde_json::to_value(&plan).unwrap();
        missing.as_object_mut().unwrap().remove("proc_filesystem");
        assert!(serde_json::from_value::<IsolationPlan>(missing).is_err());
        plan.proc_filesystem = IsolationProcFilesystem::PidNamespace;
        assert!(
            plan.validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("isolated PID")
        );
        plan.pid_namespace = IsolationPidNamespace::Isolated;
        assert!(
            plan.validate(&authorities)
                .unwrap()
                .contains(&IsolationCapability::FilesystemPidNamespaceProc)
        );
        for path in ["/proc", "/proc/self", "/proc/1/fd"] {
            plan.mounts[1].destination = IsolationPath::new(path).unwrap();
            assert!(
                plan.validate(&authorities)
                    .unwrap_err()
                    .to_string()
                    .contains("reserved PID procfs")
            );
        }
        plan.mounts[1].destination = IsolationPath::new("/process-inputs").unwrap();
        assert!(plan.validate(&authorities).is_ok());
        plan.proc_filesystem = IsolationProcFilesystem::Empty;
        plan.mounts[1].destination = IsolationPath::new("/proc/self").unwrap();
        assert!(
            plan.validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("reserved PID procfs")
        );
    }

    #[test]
    fn fixed_parent_views_are_required_explicit_and_capability_gated() {
        let (mut plan, authorities) = complete_plan();
        let mut encoded = serde_json::to_value(&plan).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("fixed_parent_views");
        assert!(serde_json::from_value::<IsolationPlan>(encoded).is_err());
        assert!(
            !plan
                .validate(&authorities)
                .unwrap()
                .contains(&IsolationCapability::FilesystemFixedParentViews)
        );
        plan.fixed_parent_views.push(fixed_view());
        assert!(
            plan.validate(&authorities)
                .unwrap()
                .contains(&IsolationCapability::FilesystemFixedParentViews)
        );
        let mut encoded = serde_json::to_value(fixed_view()).unwrap();
        encoded.as_object_mut().unwrap().remove("limits");
        assert!(serde_json::from_value::<IsolationFixedParentView>(encoded).is_err());
    }

    #[test]
    fn fixed_parent_plan_rejects_conflicting_topology_and_bounds() {
        let (base, authorities) = complete_plan();
        let mut interleaved = fixed_view();
        interleaved.denied_paths = vec![
            "control/a".into(),
            "control/a-b".into(),
            "control/a/secret".into(),
        ];
        assert!(
            interleaved
                .validate()
                .unwrap_err()
                .to_string()
                .contains("overlap")
        );
        let mut bounded = fixed_view();
        bounded.limits.max_entries = 1;
        assert!(
            bounded
                .validate()
                .unwrap_err()
                .to_string()
                .contains("entry bound")
        );
        for path in [
            "control",
            "control/../secret",
            "control//secret",
            "/control/secret",
        ] {
            let mut plan = base.clone();
            let mut view = fixed_view();
            view.denied_paths = vec![path.to_string()];
            plan.fixed_parent_views = vec![view];
            assert!(plan.validate(&authorities).is_err(), "{path}");
        }
        for bound in [0, MAX_MOUNTS + 1] {
            let mut plan = base.clone();
            let mut view = fixed_view();
            view.limits.max_entries = bound;
            plan.fixed_parent_views = vec![view];
            assert!(plan.validate(&authorities).is_err());
        }
        for destination in [
            "/project/control",
            "/project/control/secret",
            "/project/control/secret/child",
        ] {
            let mut plan = base.clone();
            plan.fixed_parent_views = vec![fixed_view()];
            let mut mount = plan.mounts[1].clone();
            mount.layer = 3;
            mount.destination = IsolationPath::new(destination).unwrap();
            plan.mounts.push(mount);
            assert!(
                plan.validate(&authorities)
                    .unwrap_err()
                    .to_string()
                    .contains("conflicts")
            );
        }
        let mut plan = base.clone();
        plan.fixed_parent_views = vec![fixed_view(), fixed_view()];
        assert!(plan.validate(&authorities).is_err());
        let mut plan = base;
        plan.mounts[1].destination = IsolationPath::new("/workspace/child").unwrap();
        assert!(
            plan.validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("ancestor")
        );
    }

    #[test]
    fn pid_namespace_choice_is_an_explicit_capability() {
        let (mut plan, authorities) = complete_plan();
        plan.pid_namespace = IsolationPidNamespace::Isolated;
        let capabilities = plan.validate(&authorities).unwrap();
        assert!(capabilities.contains(&IsolationCapability::ProcessIsolatedPidNamespace));
        assert!(!capabilities.contains(&IsolationCapability::ProcessHostPidNamespace));
    }

    #[test]
    fn plan_validation_rejects_unused_wrong_purpose_and_unordered_authorities() {
        let (plan, mut authorities) = complete_plan();
        authorities.push(authority(
            "unused",
            6,
            IsolationAuthorityPurpose::ReadOnlyMount,
        ));
        assert!(
            plan.validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("every inherited authority")
        );

        let (mut plan, authorities) = complete_plan();
        plan.mounts[1].access = IsolationMountAccess::Writable;
        assert!(
            plan.validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("purpose")
        );

        let (mut plan, authorities) = complete_plan();
        plan.mounts[2].layer = 0;
        assert!(
            plan.validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("deterministically ordered")
        );
    }

    #[test]
    fn plan_validation_requires_one_read_only_target_mount() {
        let (mut plan, authorities) = complete_plan();
        plan.mounts.remove(0);
        assert!(
            plan.validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("exactly one mount")
        );

        let (mut plan, mut authorities) = complete_plan();
        plan.mounts[0].access = IsolationMountAccess::Writable;
        authorities[0].purpose = IsolationAuthorityPurpose::WritableMount;
        assert!(
            plan.validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("target executable authority")
        );
    }

    #[test]
    fn target_channels_are_required_sorted_and_derive_exact_ipc_capability() {
        let (mut plan, mut authorities) = complete_plan();
        let source = IsolationAuthorityId::new("session-channel").unwrap();
        let auxiliary = IsolationAuthorityId::new("workload-client-channel").unwrap();
        plan.target_channels = vec![
            IsolationTargetChannel {
                source: source.clone(),
                target_fd: 0,
                env_name: "RYEOS_SESSION_FD".to_owned(),
            },
            IsolationTargetChannel {
                source: auxiliary.clone(),
                target_fd: 3,
                env_name: "RYEOS_WORKLOAD_CLIENT_FD".to_owned(),
            },
        ];
        plan.environment
            .values
            .insert("RYEOS_SESSION_FD".to_owned(), "0".to_owned());
        plan.environment
            .values
            .insert("RYEOS_WORKLOAD_CLIENT_FD".to_owned(), "3".to_owned());
        authorities.push(IsolationAuthority {
            id: source,
            inherited_fd: 6,
            purpose: IsolationAuthorityPurpose::TargetDuplexChannel,
        });
        authorities.push(IsolationAuthority {
            id: auxiliary,
            inherited_fd: 7,
            purpose: IsolationAuthorityPurpose::TargetDuplexChannel,
        });
        assert!(
            plan.validate(&authorities)
                .unwrap()
                .contains(&IsolationCapability::IpcTargetUnixStream)
        );

        let mut value = serde_json::to_value(&plan).unwrap();
        assert!(value.get("target_channels").is_some());
        value.as_object_mut().unwrap().remove("target_channels");
        assert!(serde_json::from_value::<IsolationPlan>(value).is_err());

        plan.target_channels.swap(0, 1);
        assert!(plan.validate(&authorities).is_err());
    }

    #[test]
    fn target_channel_rejects_wrong_target_environment_and_authority_use() {
        let (mut plan, mut authorities) = complete_plan();
        let source = IsolationAuthorityId::new("session-channel").unwrap();
        plan.target_channels = vec![IsolationTargetChannel {
            source: source.clone(),
            target_fd: 1,
            env_name: "RYEOS_SESSION_FD".to_owned(),
        }];
        plan.environment
            .values
            .insert("RYEOS_SESSION_FD".to_owned(), "3".to_owned());
        authorities.push(IsolationAuthority {
            id: source.clone(),
            inherited_fd: 6,
            purpose: IsolationAuthorityPurpose::TargetDuplexChannel,
        });
        assert!(plan.validate(&authorities).is_err());

        plan.target_channels[0].target_fd = 0;
        plan.environment
            .values
            .insert("RYEOS_SESSION_FD".to_owned(), "0".to_owned());
        plan.mounts[1].source = source;
        assert!(plan.validate(&authorities).is_err());
    }

    #[test]
    fn launch_request_rejects_descriptor_reuse_across_roles() {
        let (plan, authorities) = complete_plan();
        let request = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            plan,
            authorities,
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, 7)]),
            adapter_fd: 10,
            status_fd: 6,
            lifecycle: AdapterLaunchLifecycle::AwaitAttachment {
                release_fd: 7,
                release_keepalive_fd: 8,
            },
        };
        assert!(request.validate().unwrap_err().to_string().contains(
            "attachment release descriptor overlaps stdio or another isolation protocol role"
        ));

        let (plan, authorities) = complete_plan();
        let request = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            plan,
            authorities,
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, 7)]),
            adapter_fd: 7,
            status_fd: 6,
            lifecycle: AdapterLaunchLifecycle::Run,
        };
        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("adapter descriptor overlaps stdio or another isolation protocol role")
        );
    }

    #[test]
    fn self_contained_launch_has_no_artifact_descriptor_requirement() {
        let (plan, authorities) = complete_plan();
        let request = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            plan,
            authorities,
            artifacts: BTreeMap::new(),
            adapter_fd: 10,
            status_fd: 6,
            lifecycle: AdapterLaunchLifecycle::Run,
        };
        request.validate().unwrap();
    }

    #[test]
    fn target_channel_may_already_occupy_its_exact_target_descriptor() {
        let (mut plan, mut authorities) = complete_plan();
        let source = IsolationAuthorityId::new("session-channel").unwrap();
        plan.target_channels.push(IsolationTargetChannel {
            source: source.clone(),
            target_fd: 9,
            env_name: "RYEOS_SESSION_FD".to_owned(),
        });
        plan.environment
            .values
            .insert("RYEOS_SESSION_FD".to_owned(), "9".to_owned());
        authorities.push(IsolationAuthority {
            id: source,
            inherited_fd: 9,
            purpose: IsolationAuthorityPurpose::TargetDuplexChannel,
        });
        AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            plan,
            authorities,
            artifacts: BTreeMap::new(),
            adapter_fd: 10,
            status_fd: 6,
            lifecycle: AdapterLaunchLifecycle::Run,
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn launch_lifecycle_wire_shape_is_exact() {
        let (plan, authorities) = complete_plan();
        let run = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            plan: plan.clone(),
            authorities: authorities.clone(),
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, 7)]),
            adapter_fd: 8,
            status_fd: 6,
            lifecycle: AdapterLaunchLifecycle::Run,
        };
        let value = serde_json::to_value(&run).unwrap();
        assert_eq!(value["lifecycle"], serde_json::json!({ "kind": "run" }));
        run.validate().unwrap();

        let awaiting = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            plan,
            authorities,
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, 7)]),
            adapter_fd: 10,
            status_fd: 6,
            lifecycle: AdapterLaunchLifecycle::AwaitAttachment {
                release_fd: 8,
                release_keepalive_fd: 9,
            },
        };
        let value = serde_json::to_value(&awaiting).unwrap();
        assert_eq!(
            value["lifecycle"],
            serde_json::json!({
                "kind": "await_attachment",
                "release_fd": 8,
                "release_keepalive_fd": 9
            })
        );
        awaiting.validate().unwrap();
    }

    #[test]
    fn protocol_collection_and_string_limits_are_independent() {
        let (plan, authorities) = complete_plan();

        let mut too_many_authorities = authorities.clone();
        too_many_authorities.resize(
            MAX_AUTHORITIES + 1,
            authority("overflow", 99, IsolationAuthorityPurpose::ReadOnlyMount),
        );
        assert!(
            plan.validate(&too_many_authorities)
                .unwrap_err()
                .to_string()
                .contains("too many authorities")
        );

        let (mut too_many_mounts, authorities) = complete_plan();
        too_many_mounts
            .mounts
            .resize(MAX_MOUNTS + 1, too_many_mounts.mounts[1].clone());
        assert!(
            too_many_mounts
                .validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("too many mounts")
        );

        let (mut too_many_arguments, authorities) = complete_plan();
        too_many_arguments
            .target
            .arguments
            .resize(MAX_ARGUMENTS + 1, "argument".to_string());
        assert!(
            too_many_arguments
                .validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("too many target arguments")
        );

        let (mut too_many_environment, authorities) = complete_plan();
        too_many_environment.environment.values = (0..=MAX_ENVIRONMENT_ENTRIES)
            .map(|index| (format!("KEY_{index}"), "value".to_string()))
            .collect();
        assert!(
            too_many_environment
                .validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("too many environment entries")
        );

        let (mut oversized_string, authorities) = complete_plan();
        oversized_string.target.argv0 = "x".repeat(MAX_STRING_BYTES + 1);
        assert!(
            oversized_string
                .validate(&authorities)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
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
        assert!(
            diagnostic
                .validate()
                .unwrap_err()
                .to_string()
                .contains("too many diagnostic")
        );
    }

    #[test]
    fn inspection_contract_validates_identity_descriptors_and_digests() {
        let request = AdapterInspectionRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            target: IsolationTargetTriple::X86_64UnknownLinuxGnu,
            backend_id: "invalid/example".to_string(),
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, 3)]),
        };
        assert!(request.validate().is_err());

        let response = AdapterInspectionResponse {
            protocol: IsolationAdapterProtocolVersion::Current,
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
        assert!(
            response
                .validate()
                .unwrap_err()
                .to_string()
                .contains("lowercase SHA-256")
        );

        let self_contained_request = AdapterInspectionRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            target: IsolationTargetTriple::X86_64UnknownLinuxGnu,
            backend_id: "native-linux".to_string(),
            artifacts: BTreeMap::new(),
        };
        self_contained_request.validate().unwrap();
        AdapterInspectionResponse {
            protocol: IsolationAdapterProtocolVersion::Current,
            adapter_build: "0.1.0".to_string(),
            effective_capabilities: BTreeSet::from([IsolationCapability::FilesystemPrivateRoot]),
            artifacts: BTreeMap::new(),
        }
        .validate()
        .unwrap();
    }

    fn workspace_request(operation: WorkspaceLifecycleOperation) -> AdapterWorkspaceRequest {
        AdapterWorkspaceRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            operation,
            workspace_id: "workspace-one".to_string(),
            launch_owner: "{\"attempt\":1}".to_string(),
            base_snapshot: "a".repeat(64),
            transfer_fd: (operation == WorkspaceLifecycleOperation::Create).then_some(12),
            mount_identity: (operation != WorkspaceLifecycleOperation::Create)
                .then(|| "b".repeat(64)),
            authorities: vec![
                IsolationAuthority {
                    id: IsolationAuthorityId::new("workspace-project").unwrap(),
                    inherited_fd: 10,
                    purpose: IsolationAuthorityPurpose::WorkspaceProject,
                },
                IsolationAuthority {
                    id: IsolationAuthorityId::new("workspace-backend-state").unwrap(),
                    inherited_fd: 11,
                    purpose: IsolationAuthorityPurpose::WorkspaceBackendState,
                },
            ],
        }
    }

    #[test]
    fn workspace_exposes_only_project_and_opaque_backend_state() {
        let request = workspace_request(WorkspaceLifecycleOperation::Create);
        request.validate().unwrap();
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["protocol"], "ryeos.isolation-adapter/v10");
        let purposes = encoded["authorities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|authority| authority["purpose"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            purposes,
            BTreeSet::from(["workspace_project", "workspace_backend_state"])
        );
        assert!(!encoded.to_string().contains("workspace_upper"));
        assert!(!encoded.to_string().contains("workspace_work"));
    }

    #[test]
    fn workspace_diff_must_name_a_safe_backend_relative_content_root() {
        let request = workspace_request(WorkspaceLifecycleOperation::FreezeAndDiff);
        let mut response = AdapterWorkspaceResponse {
            protocol: IsolationAdapterProtocolVersion::Current,
            operation: WorkspaceLifecycleOperation::FreezeAndDiff,
            workspace_id: request.workspace_id.clone(),
            launch_owner: request.launch_owner.clone(),
            backend_id: "example-backend".to_string(),
            backend_version: "1".to_string(),
            pinned_root_identities: BTreeMap::from([
                ("project".to_string(), "dev1-ino2".to_string()),
                ("backend_state".to_string(), "dev1-ino3".to_string()),
            ]),
            mount_identity: request.mount_identity.clone(),
            view_descriptor_identity: None,
            mutation_content_root: Some("backend-output".to_string()),
            mutations: Vec::new(),
            destroyed: false,
        };
        response.validate_for(&request).unwrap();
        response.mutation_content_root = Some("../upper".to_string());
        assert!(response.validate_for(&request).is_err());
        response.mutation_content_root = None;
        assert!(response.validate_for(&request).is_err());
    }

    fn workspace_response(request: &AdapterWorkspaceRequest) -> AdapterWorkspaceResponse {
        AdapterWorkspaceResponse {
            protocol: request.protocol,
            operation: request.operation,
            workspace_id: request.workspace_id.clone(),
            launch_owner: request.launch_owner.clone(),
            backend_id: "example-backend".to_string(),
            backend_version: "1".to_string(),
            pinned_root_identities: BTreeMap::from([
                ("project".to_string(), "dev1-ino2".to_string()),
                ("backend_state".to_string(), "dev1-ino3".to_string()),
            ]),
            mount_identity: if request.operation == WorkspaceLifecycleOperation::Create {
                Some("b".repeat(64))
            } else {
                request.mount_identity.clone()
            },
            view_descriptor_identity: (request.operation == WorkspaceLifecycleOperation::Create)
                .then(|| "c".repeat(64)),
            mutation_content_root: (request.operation
                == WorkspaceLifecycleOperation::FreezeAndDiff)
                .then(|| "output".to_string()),
            mutations: Vec::new(),
            destroyed: request.operation == WorkspaceLifecycleOperation::Destroy,
        }
    }

    #[test]
    fn workspace_symlink_mutation_is_bounded_exact_and_closed() {
        let original = WorkspaceMutation {
            path: "products/runtime/bin/program".into(),
            kind: WorkspaceMutationKind::UpsertSymlink,
            normalized_mode: None,
            size: None,
            content_hash: None,
            target: Some("../lib/program".into()),
        };
        original.validate().unwrap();
        let wire = serde_json::to_value(&original).unwrap();
        assert_eq!(
            serde_json::from_value::<WorkspaceMutation>(wire.clone()).unwrap(),
            original
        );
        for target in [
            None,
            Some("".into()),
            Some("/host/bin/program".into()),
            Some("bad\0target".into()),
            Some("x".repeat(MAX_WORKSPACE_SYMLINK_TARGET_BYTES + 1)),
        ] {
            let mut mutation = original.clone();
            mutation.target = target;
            assert!(mutation.validate().is_err());
        }
        let mut at_bound = original.clone();
        at_bound.target = Some("x".repeat(MAX_WORKSPACE_SYMLINK_TARGET_BYTES));
        at_bound.validate().unwrap();
        for field in ["normalized_mode", "size", "content_hash"] {
            let mut changed = wire.clone();
            changed[field] = if field == "content_hash" {
                serde_json::json!("a".repeat(64))
            } else {
                serde_json::json!(1)
            };
            assert!(
                serde_json::from_value::<WorkspaceMutation>(changed)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove("target");
        assert!(serde_json::from_value::<WorkspaceMutation>(missing).is_err());
        let mut unknown = wire;
        unknown["follow_target"] = true.into();
        assert!(serde_json::from_value::<WorkspaceMutation>(unknown).is_err());
        for kind in [
            WorkspaceMutationKind::DeletePath,
            WorkspaceMutationKind::EnsureDirectory,
            WorkspaceMutationKind::OpaqueDirectory,
        ] {
            let mut mutation = original.clone();
            mutation.kind = kind;
            assert!(mutation.validate().is_err());
            mutation.target = None;
            mutation.validate().unwrap();
        }
        let mut regular = original;
        regular.kind = WorkspaceMutationKind::UpsertRegular;
        regular.normalized_mode = Some(0o755);
        regular.size = Some(1);
        regular.content_hash = Some("a".repeat(64));
        assert!(regular.validate().is_err());
        regular.target = None;
        regular.validate().unwrap();
    }

    #[test]
    fn workspace_mutations_cannot_descend_through_a_symlink() {
        let request = workspace_request(WorkspaceLifecycleOperation::FreezeAndDiff);
        let mut response = workspace_response(&request);
        response.mutations = vec![
            WorkspaceMutation {
                path: "products/link".into(),
                kind: WorkspaceMutationKind::UpsertSymlink,
                normalized_mode: None,
                size: None,
                content_hash: None,
                target: Some("real".into()),
            },
            WorkspaceMutation {
                path: "products/link/child".into(),
                kind: WorkspaceMutationKind::EnsureDirectory,
                normalized_mode: None,
                size: None,
                content_hash: None,
                target: None,
            },
        ];
        assert!(response.validate_for(&request).is_err());
        response.mutations.pop();
        response.validate_for(&request).unwrap();
    }

    #[test]
    fn workspace_create_requires_exact_transfer_and_no_previous_view() {
        let original = workspace_request(WorkspaceLifecycleOperation::Create);
        for descriptor in [None, Some(0), Some(2), Some(10), Some(11)] {
            let mut request = original.clone();
            request.transfer_fd = descriptor;
            assert!(request.validate().is_err());
        }
        let mut request = original;
        request.mount_identity = Some("b".repeat(64));
        assert!(request.validate().is_err());
    }

    #[test]
    fn workspace_noncreate_cannot_introduce_a_view() {
        for operation in [
            WorkspaceLifecycleOperation::FreezeAndDiff,
            WorkspaceLifecycleOperation::Destroy,
        ] {
            let mut request = workspace_request(operation);
            request.validate().unwrap();
            let mut response = workspace_response(&request);
            response.validate_for(&request).unwrap();
            request.transfer_fd = Some(12);
            assert!(request.validate().is_err());
            request.transfer_fd = None;
            response.view_descriptor_identity = Some("c".repeat(64));
            assert!(response.validate_for(&request).is_err());
            response.view_descriptor_identity = None;
            response.mount_identity = Some("d".repeat(64));
            assert!(response.validate_for(&request).is_err());
        }
        let mut request = workspace_request(WorkspaceLifecycleOperation::FreezeAndDiff);
        request.mount_identity = None;
        assert!(request.validate().is_err());
        request.operation = WorkspaceLifecycleOperation::Destroy;
        request.validate().unwrap();
        let response = workspace_response(&request);
        assert!(response.mount_identity.is_none());
        response.validate_for(&request).unwrap();
    }

    #[test]
    fn workspace_nullable_identity_fields_must_be_explicit() {
        let request = workspace_request(WorkspaceLifecycleOperation::Create);
        for field in ["transfer_fd", "mount_identity"] {
            let mut encoded = serde_json::to_value(&request).unwrap();
            encoded.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<AdapterWorkspaceRequest>(encoded).is_err(),
                "{field}"
            );
        }
        let request = workspace_request(WorkspaceLifecycleOperation::Destroy);
        let response = workspace_response(&request);
        for field in ["mount_identity", "view_descriptor_identity"] {
            let mut encoded = serde_json::to_value(&response).unwrap();
            encoded.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<AdapterWorkspaceResponse>(encoded).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn workspace_creation_identity_commits_owner_roots_and_actual_view() {
        let request = workspace_request(WorkspaceLifecycleOperation::Create);
        let response = workspace_response(&request);
        response.validate_for(&request).unwrap();
        let identity = response.mount_identity_value(&request).unwrap();
        assert_eq!(identity["view_descriptor_identity"], "c".repeat(64));
        assert_eq!(
            identity["pinned_root_identities"],
            serde_json::to_value(&response.pinned_root_identities).unwrap()
        );
        let mut changed = request.clone();
        changed.launch_owner = "{\"attempt\":2}".to_string();
        assert_ne!(identity, response.mount_identity_value(&changed).unwrap());
        changed = request.clone();
        changed.base_snapshot = "d".repeat(64);
        assert_ne!(identity, response.mount_identity_value(&changed).unwrap());
        let mut changed = response.clone();
        changed.view_descriptor_identity = Some("e".repeat(64));
        assert_ne!(identity, changed.mount_identity_value(&request).unwrap());
        changed.view_descriptor_identity = None;
        assert!(changed.validate_for(&request).is_err());
        assert!(changed.mount_identity_value(&request).is_err());
    }

    #[test]
    fn workspace_transfer_receipt_is_closed_and_bounded() {
        let mut receipt = WorkspaceViewTransferReceipt {
            protocol: IsolationAdapterProtocolVersion::Current,
            request_digest: "a".repeat(64),
            response_digest: "b".repeat(64),
        };
        receipt.validate().unwrap();
        assert!(serde_json::to_vec(&receipt).unwrap().len() < MAX_WORKSPACE_VIEW_RECEIPT_BYTES);
        let mut value = serde_json::to_value(&receipt).unwrap();
        value["descriptor"] = serde_json::json!(3);
        assert!(serde_json::from_value::<WorkspaceViewTransferReceipt>(value).is_err());
        receipt.response_digest = "B".repeat(64);
        assert!(receipt.validate().is_err());
    }
}
