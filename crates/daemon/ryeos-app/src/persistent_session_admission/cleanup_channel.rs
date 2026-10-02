//! Product-private normal-close transport. A candidate frame is NOT accepted
//! driver retirement. The executable owner must separately authenticate the
//! admitted producer, original reservation/channel and actual held occurrence,
//! then join independent group retirement and the existing financial owner.
//!
//! This transport remains unwired while those executable prerequisites refuse.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

const MAX_FRAME_BYTES: usize = 4096;
pub(crate) const CHANNEL_ENV: &str = "RYEOS_RESOURCE_CLEANUP_FD";
pub(crate) const TARGET_FD: u32 = 4;

/// Comparison coordinates only; construction does not authenticate an owner.
/// The reservation ID is the existing canonical original reservation digest;
/// process/resource digests come from ExecutionProcessIdentity's existing APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupBindings {
    pub(crate) teardown: String,
    pub(crate) capsule_hash: String,
    pub(crate) cleanup_contract: String,
    pub(crate) process_owner: String,
    pub(crate) resource_set: String,
}

impl CleanupBindings {
    fn validate(&self) -> Result<()> {
        for digest in [
            &self.teardown,
            &self.capsule_hash,
            &self.cleanup_contract,
            &self.process_owner,
            &self.resource_set,
        ] {
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "cleanup comparison coordinate is not an existing canonical digest"
            );
        }
        Ok(())
    }
}

/// Product-private startup comparisons, not selected-device or launch authority.
/// Deliberately no production constructor, Deserialize or default: the retained
/// independently admitted device/product issuer is not implemented yet. A later
/// issuer must consume those actual owners; raw bytes/hashes cannot mint it.
#[derive(Serialize)]
pub(crate) struct CleanupStartupComparisons {
    uuid_v2_hex: String,
    gnu_bounds: ryeos_state::external_content::products::ProductBounds,
    driver_bounds: ryeos_state::external_content::products::ProductBounds,
}

impl CleanupStartupComparisons {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.uuid_v2_hex.len() == 32
                && self
                    .uuid_v2_hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && self.uuid_v2_hex.bytes().any(|b| b != b'0'),
            "startup selected-device UUID comparison is invalid"
        );
        for bounds in [&self.gnu_bounds, &self.driver_bounds] {
            bounds.validate()?;
            ensure!(
                bounds.maximum_entries > 0
                    && bounds.maximum_depth > 0
                    && bounds.maximum_file_bytes > 0
                    && bounds.maximum_total_bytes > 0
                    && bounds.maximum_file_bytes <= bounds.maximum_total_bytes,
                "startup product bounds comparison is invalid"
            );
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct InitFrame<'a> {
    #[serde(flatten)]
    bindings: Frame,
    #[serde(flatten)]
    startup: &'a CleanupStartupComparisons,
}

fn write_init(
    writer: &mut impl Write,
    bindings: &CleanupBindings,
    startup: &CleanupStartupComparisons,
) -> Result<()> {
    bindings.validate()?;
    startup.validate()?;
    let mut frame = Frame::new("init", bindings);
    frame.version = 2;
    let body = serde_json::to_vec(&InitFrame {
        bindings: frame,
        startup,
    })?;
    write_bounded_body(writer, &body)
}

fn write_bounded_body(writer: &mut impl Write, body: &[u8]) -> Result<()> {
    ensure!(
        !body.is_empty() && body.len() <= MAX_FRAME_BYTES,
        "cleanup frame exceeds its bound"
    );
    writer.write_all(&(body.len() as u32).to_be_bytes())?;
    writer.write_all(body)?;
    writer.flush()?;
    Ok(())
}

fn canonical_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn token(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// Closed product-local comparisons; none of these wire types issues authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreflightDevice {
    role: String,
    destination: String,
    access: PreflightDeviceAccess,
    major: u32,
    minor: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum PreflightDeviceAccess {
    #[serde(rename = "read_write")]
    ReadWrite,
}

impl PreflightDevice {
    fn validate(&self) -> Result<()> {
        ensure!(
            token(&self.role, 64) && self.major > 0,
            "preflight device identity invalid"
        );
        ensure!(
            self.destination.len() <= 4096
                && self.destination.starts_with("/dev/")
                && self.destination[1..]
                    .split('/')
                    .all(|p| !p.is_empty() && p != "." && p != "..")
                && !self.destination.chars().any(char::is_control),
            "preflight device destination invalid"
        );
        ensure!(
            !self.destination.starts_with("/dev/nvidia-caps/"),
            "preflight MIG capability refused"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreflightSelection {
    stable_id: String,
    class: String,
    // Closed empty-fact lane, not arbitrary serde_json::Value content.
    matched_facts: std::collections::BTreeMap<String, ()>,
    observation_contract_digest: String,
    device_binding_digest: String,
    access: PreflightAccess,
    enforcement: PreflightEnforcement,
    character_devices: Vec<PreflightDevice>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum PreflightAccess {
    #[serde(rename = "execution_restricted")]
    ExecutionRestricted,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum PreflightEnforcement {
    #[serde(rename = "character_device_grant")]
    CharacterDeviceGrant,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum PreflightL4Policy {
    #[serde(rename = "l4-is-mig-success-zero-mode-not-supported-v1")]
    SuccessZero,
    #[serde(rename = "l4-is-mig-not-supported-mode-not-supported-v1")]
    NotSupported,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreflightPolicy {
    schema: String,
    primary_device_role: String,
    // Required nullable field: missing is not an implicit default policy.
    #[serde(deserialize_with = "required_nullable_policy")]
    l4_query_policy: Option<PreflightL4Policy>,
}

fn required_nullable_policy<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<PreflightL4Policy>, D::Error> {
    Option::<PreflightL4Policy>::deserialize(deserializer)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreflightInputs {
    resource_selection: PreflightSelection,
    policy: PreflightPolicy,
    handler_source_sha256: String,
    provider_manifest_sha256: String,
    gnu_bounds: ryeos_state::external_content::products::ProductBounds,
    driver_bounds: ryeos_state::external_content::products::ProductBounds,
}

impl PreflightInputs {
    fn validate(&self) -> Result<&PreflightDevice> {
        let selected = &self.resource_selection;
        ensure!(
            token(&selected.stable_id, 256)
                && token(&selected.class, 256)
                && selected.matched_facts.is_empty(),
            "preflight raw selection invalid"
        );
        for digest in [
            &selected.observation_contract_digest,
            &selected.device_binding_digest,
            &self.handler_source_sha256,
            &self.provider_manifest_sha256,
        ] {
            ensure!(
                canonical_hex(digest, 64),
                "preflight comparison digest invalid"
            );
        }
        ensure!(
            self.policy.schema == "ryeos.local_tinygrad_nvml_assignment_policy.v1"
                && token(&self.policy.primary_device_role, 64),
            "preflight finite policy invalid"
        );
        for bounds in [&self.gnu_bounds, &self.driver_bounds] {
            bounds.validate()?;
            ensure!(
                bounds.maximum_entries > 0
                    && bounds.maximum_depth > 0
                    && bounds.maximum_entries <= u32::MAX as usize
                    && bounds.maximum_depth <= u32::MAX as usize
                    && bounds.maximum_file_bytes > 0
                    && bounds.maximum_total_bytes > 0
                    && bounds.maximum_file_bytes <= bounds.maximum_total_bytes,
                "preflight retained bounds invalid"
            );
        }
        ensure!(
            (1..=32).contains(&selected.character_devices.len()),
            "preflight device count invalid"
        );
        let mut previous = "";
        let mut destinations = std::collections::BTreeSet::new();
        let mut identities = std::collections::BTreeSet::new();
        let mut primary = None;
        for device in &selected.character_devices {
            device.validate()?;
            ensure!(
                previous < device.role.as_str()
                    && destinations.insert(&device.destination)
                    && identities.insert((device.major, device.minor)),
                "preflight device alias/order invalid"
            );
            previous = &device.role;
            let primary_path =
                device
                    .destination
                    .strip_prefix("/dev/nvidia")
                    .is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                    });
            if primary_path || (device.major == 195 && device.minor < 254) {
                ensure!(primary.is_none(), "preflight multiple primary devices");
                primary = Some(device);
            }
        }
        let primary = primary.context("preflight primary device missing")?;
        ensure!(
            primary.role == self.policy.primary_device_role
                && primary.major == 195
                && primary.minor < 254
                && primary.destination == format!("/dev/nvidia{}", primary.minor),
            "preflight selected primary mismatch"
        );
        Ok(primary)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightRequest {
    version: u32,
    kind: String,
    bindings: CleanupBindings,
    challenge: String,
    inputs: PreflightInputs,
}

impl PreflightRequest {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 3 && self.kind == "preflight",
            "preflight request version/kind invalid"
        );
        self.bindings.validate()?;
        ensure!(
            canonical_hex(&self.challenge, 64) && self.challenge.bytes().any(|b| b != b'0'),
            "preflight challenge invalid"
        );
        self.inputs.validate()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreflightObservation {
    version: u32,
    stable_resource_id: String,
    device_binding_digest: String,
    primary_device: PreflightDevice,
    uuid_v2_hex: String,
    nvml_shutdown: PreflightNvmlClose,
}

#[derive(Deserialize, Serialize)]
enum PreflightNvmlClose {
    #[serde(rename = "closed")]
    Closed,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PreflightResponse {
    version: u32,
    kind: String,
    bindings: CleanupBindings,
    challenge: String,
    observation: PreflightObservation,
}

/// Parsed correspondence only. No conversion to startup comparisons or proof.
pub(crate) struct PendingPreflightObservation {
    request: PreflightRequest,
    observation: PreflightObservation,
}

fn read_preflight_response(
    reader: &mut impl Read,
    expected: &PreflightRequest,
) -> Result<PendingPreflightObservation> {
    expected.validate()?;
    let mut header = [0; 4];
    reader
        .read_exact(&mut header)
        .context("read preflight header")?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=MAX_FRAME_BYTES).contains(&length),
        "preflight frame exceeds bound"
    );
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .context("read preflight body")?;
    let response: PreflightResponse =
        serde_json::from_slice(&body).context("decode closed preflight result")?;
    ensure!(
        response.version == 3
            && response.kind == "preflight-result"
            && response.bindings == expected.bindings
            && response.challenge == expected.challenge,
        "preflight result belongs to another phase/challenge/owner"
    );
    let observation = response.observation;
    ensure!(
        observation.version == 1
            && observation.stable_resource_id == expected.inputs.resource_selection.stable_id
            && observation.device_binding_digest
                == expected.inputs.resource_selection.device_binding_digest
            && &observation.primary_device == expected.inputs.validate()?
            && canonical_hex(&observation.uuid_v2_hex, 32)
            && observation.uuid_v2_hex.bytes().any(|b| b != b'0'),
        "preflight observation mismatch"
    );
    Ok(PendingPreflightObservation {
        request: expected.clone(),
        observation,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    version: u32,
    kind: String,
    teardown: String,
    capsule_hash: String,
    cleanup_contract: String,
    process_owner: String,
    resource_set: String,
}

impl Frame {
    fn new(kind: &str, bindings: &CleanupBindings) -> Self {
        Self {
            version: 1,
            kind: kind.to_owned(),
            teardown: bindings.teardown.clone(),
            capsule_hash: bindings.capsule_hash.clone(),
            cleanup_contract: bindings.cleanup_contract.clone(),
            process_owner: bindings.process_owner.clone(),
            resource_set: bindings.resource_set.clone(),
        }
    }

    fn matches(&self, kind: &str, bindings: &CleanupBindings) -> Result<()> {
        ensure!(
            self.version == 1 && self.kind == kind,
            "unexpected cleanup frame"
        );
        let actual = CleanupBindings {
            teardown: self.teardown.clone(),
            capsule_hash: self.capsule_hash.clone(),
            cleanup_contract: self.cleanup_contract.clone(),
            process_owner: self.process_owner.clone(),
            resource_set: self.resource_set.clone(),
        };
        actual.validate()?;
        ensure!(
            &actual == bindings,
            "cleanup frame belongs to different comparison coordinates"
        );
        Ok(())
    }
}

fn write_frame(writer: &mut impl Write, kind: &str, bindings: &CleanupBindings) -> Result<()> {
    ensure!(
        matches!(kind, "shutdown" | "retired"),
        "v1 init or unknown cleanup frame refuses"
    );
    bindings.validate()?;
    let body = serde_json::to_vec(&Frame::new(kind, bindings))?;
    write_bounded_body(writer, &body)
}

fn read_retired(reader: &mut impl Read, bindings: &CleanupBindings) -> Result<()> {
    let mut header = [0u8; 4];
    reader
        .read_exact(&mut header)
        .context("read bounded cleanup header")?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=MAX_FRAME_BYTES).contains(&length),
        "cleanup frame exceeds its bound"
    );
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .context("read bounded cleanup body")?;
    let frame: Frame = serde_json::from_slice(&body).context("decode strict cleanup frame")?;
    frame.matches("retired", bindings)?;
    // Successful frame alone is insufficient. Require producer write closure;
    // duplicate frames, trailing bytes and a stalled peer all refuse.
    let mut extra = [0u8; 1];
    loop {
        match reader.read(&mut extra) {
            Ok(0) => return Ok(()),
            Ok(_) => bail!("cleanup producer sent trailing or duplicate bytes"),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("observe cleanup producer write closure"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Created,
    PreflightQueued,
    PreflightResultRead,
    InitQueued,
    ShutdownSent,
    CandidateRead,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupMode {
    InitFirst,
    Preflight,
}

/// One original channel and absolute deadline, never a per-frame timeout.
/// No API returns RuntimeDb cleanup evidence, financial authority or release.
pub(crate) struct TrustedCleanupChannel {
    channel: lillux::InheritedDuplexChannel,
    bindings: Option<CleanupBindings>,
    deadline: lillux::time::MonotonicDeadline,
    phase: Phase,
    startup_mode: StartupMode,
    preflight: Option<PreflightRequest>,
    preflight_candidate: Option<PendingPreflightObservation>,
}

pub(crate) struct PendingDriverClose {
    bindings: CleanupBindings,
}

impl PendingDriverClose {
    pub(crate) fn bindings(&self) -> &CleanupBindings {
        &self.bindings
    }
}

impl TrustedCleanupChannel {
    pub(crate) fn prepare(
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<(
        Self,
        ryeos_engine::isolation::IsolationTargetChannelAuthority,
    )> {
        Self::prepare_mode(deadline, StartupMode::InitFirst)
    }

    /// Explicit new mode; no production caller or accepting factory yet.
    pub(crate) fn prepare_preflight(
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<(
        Self,
        ryeos_engine::isolation::IsolationTargetChannelAuthority,
    )> {
        Self::prepare_mode(deadline, StartupMode::Preflight)
    }

    fn prepare_mode(
        deadline: lillux::time::MonotonicDeadline,
        startup_mode: StartupMode,
    ) -> Result<(
        Self,
        ryeos_engine::isolation::IsolationTargetChannelAuthority,
    )> {
        ensure!(
            !deadline.remaining().is_zero(),
            "cleanup original deadline expired"
        );
        let (channel, child) =
            lillux::inherited_duplex_channel_pair().map_err(anyhow::Error::msg)?;
        let target = ryeos_engine::isolation::IsolationTargetChannelAuthority::new(
            child,
            TARGET_FD,
            CHANNEL_ENV,
        )?;
        Ok((
            Self {
                channel,
                bindings: None,
                deadline,
                phase: Phase::Created,
                startup_mode,
                preflight: None,
                preflight_candidate: None,
            },
            target,
        ))
    }

    /// Future caller must prove actual Attached original ownership before this
    /// operation, and release only after it succeeds. No acknowledgment while held.
    pub(crate) fn queue_preflight(
        &mut self,
        bindings: CleanupBindings,
        challenge: String,
        inputs: PreflightInputs,
    ) -> Result<()> {
        let previous = std::mem::replace(&mut self.phase, Phase::Failed);
        ensure!(
            !self.deadline.remaining().is_zero(),
            "preflight original deadline expired"
        );
        ensure!(
            self.startup_mode == StartupMode::Preflight && previous == Phase::Created,
            "preflight queue reused or wrong startup mode"
        );
        self.preflight = Some(PreflightRequest {
            version: 3,
            kind: "preflight".into(),
            bindings,
            challenge,
            inputs,
        });
        let request = self
            .preflight
            .as_ref()
            .expect("retained before validation/write");
        request.validate()?;
        self.channel
            .ensure_peer_live_and_quiet()
            .context("unexpected preflight bytes before queue")?;
        write_bounded_body(
            &mut self.channel.with_deadline(self.deadline),
            &serde_json::to_vec(request)?,
        )?;
        ensure!(
            !self.deadline.remaining().is_zero(),
            "preflight original deadline expired after queue"
        );
        self.phase = Phase::PreflightQueued;
        Ok(())
    }

    /// Future original owner must separately corroborate release/live applied
    /// identity. A parsed result is NOT acceptance; FD4 stays open for later init.
    pub(crate) fn receive_preflight_candidate(&mut self) -> Result<&PendingPreflightObservation> {
        let previous = std::mem::replace(&mut self.phase, Phase::Failed);
        ensure!(
            !self.deadline.remaining().is_zero(),
            "preflight original deadline expired"
        );
        ensure!(
            self.startup_mode == StartupMode::Preflight && previous == Phase::PreflightQueued,
            "preflight result premature/reused"
        );
        let request = self
            .preflight
            .as_ref()
            .context("preflight request custody missing")?;
        self.preflight_candidate = Some(read_preflight_response(
            &mut self.channel.with_deadline(self.deadline),
            request,
        )?);
        // An instantaneous quiet check is not future duplicate/descendant proof.
        self.channel
            .ensure_peer_live_and_quiet()
            .context("unexpected queued preflight bytes")?;
        ensure!(
            !self.deadline.remaining().is_zero(),
            "preflight original deadline expired after result"
        );
        self.phase = Phase::PreflightResultRead;
        Ok(self
            .preflight_candidate
            .as_ref()
            .expect("retained before late checks"))
    }

    /// Legacy mode queues before release. Explicit preflight mode requires a
    /// parsed candidate and a separately issued private startup value afterwards.
    /// No production factory exists; phase correspondence alone cannot issue it.
    pub(crate) fn queue_init(
        &mut self,
        bindings: CleanupBindings,
        startup: &CleanupStartupComparisons,
    ) -> Result<()> {
        let previous = std::mem::replace(&mut self.phase, Phase::Failed);
        ensure!(
            (self.startup_mode == StartupMode::InitFirst && previous == Phase::Created)
                || (self.startup_mode == StartupMode::Preflight
                    && previous == Phase::PreflightResultRead),
            "cleanup initialization was reused"
        );
        bindings.validate()?;
        if self.startup_mode == StartupMode::Preflight {
            ensure!(
                !self.deadline.remaining().is_zero(),
                "preflight original deadline expired before init"
            );
            ensure!(
                self.preflight
                    .as_ref()
                    .is_some_and(|request| request.bindings == bindings),
                "preflight initialization changed original bindings"
            );
            let candidate = self
                .preflight_candidate
                .as_ref()
                .context("preflight parsed candidate missing")?;
            ensure!(
                candidate.observation.uuid_v2_hex == startup.uuid_v2_hex
                    && candidate.request.inputs.gnu_bounds == startup.gnu_bounds
                    && candidate.request.inputs.driver_bounds == startup.driver_bounds,
                "preflight initialization changed parsed comparisons"
            );
            self.channel
                .ensure_peer_live_and_quiet()
                .context("unexpected producer bytes before init")?;
        }
        write_init(
            &mut self.channel.with_deadline(self.deadline),
            &bindings,
            startup,
        )?;
        if self.startup_mode == StartupMode::Preflight {
            ensure!(
                !self.deadline.remaining().is_zero(),
                "preflight original deadline expired after init"
            );
        }
        self.bindings = Some(bindings);
        self.phase = Phase::InitQueued;
        Ok(())
    }

    /// Caller closes/quiesces the existing session request owner first.
    pub(crate) fn request_retirement(&mut self) -> Result<()> {
        let previous = std::mem::replace(&mut self.phase, Phase::Failed);
        ensure!(
            previous == Phase::InitQueued,
            "cleanup teardown was premature or reused"
        );
        self.channel
            .ensure_peer_live_and_quiet()
            .context("cleanup producer sent data before authorized teardown")?;
        write_frame(
            &mut self.channel.with_deadline(self.deadline),
            "shutdown",
            self.bindings
                .as_ref()
                .context("cleanup original bindings are missing")?,
        )?;
        self.phase = Phase::ShutdownSent;
        Ok(())
    }

    pub(crate) fn receive_candidate(&mut self) -> Result<PendingDriverClose> {
        let previous = std::mem::replace(&mut self.phase, Phase::Failed);
        ensure!(
            previous == Phase::ShutdownSent,
            "cleanup observation was premature or reused"
        );
        let bindings = self
            .bindings
            .as_ref()
            .context("cleanup original bindings are missing")?;
        read_retired(&mut self.channel.with_deadline(self.deadline), bindings)?;
        self.phase = Phase::CandidateRead;
        Ok(PendingDriverClose {
            bindings: bindings.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn bindings() -> CleanupBindings {
        CleanupBindings {
            teardown: "1".repeat(64),
            capsule_hash: "2".repeat(64),
            cleanup_contract: "3".repeat(64),
            process_owner: "4".repeat(64),
            resource_set: "5".repeat(64),
        }
    }

    fn framed(body: &[u8]) -> Vec<u8> {
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        bytes
    }

    // Wire fixtures only. No independently qualified owner, provider or UUID.
    fn preflight_request() -> PreflightRequest {
        let device = PreflightDevice {
            role: "primary".into(),
            destination: "/dev/nvidia0".into(),
            access: PreflightDeviceAccess::ReadWrite,
            major: 195,
            minor: 0,
        };
        let selected = PreflightSelection {
            stable_id: "gpu0".into(),
            class: "gpu".into(),
            matched_facts: Default::default(),
            observation_contract_digest: "6".repeat(64),
            device_binding_digest: "7".repeat(64),
            access: PreflightAccess::ExecutionRestricted,
            enforcement: PreflightEnforcement::CharacterDeviceGrant,
            character_devices: vec![device],
        };
        let startup = startup();
        PreflightRequest {
            version: 3,
            kind: "preflight".into(),
            bindings: bindings(),
            challenge: "8".repeat(64),
            inputs: PreflightInputs {
                resource_selection: selected,
                policy: PreflightPolicy {
                    schema: "ryeos.local_tinygrad_nvml_assignment_policy.v1".into(),
                    primary_device_role: "primary".into(),
                    l4_query_policy: None,
                },
                handler_source_sha256: "9".repeat(64),
                provider_manifest_sha256: "a".repeat(64),
                gnu_bounds: startup.gnu_bounds,
                driver_bounds: startup.driver_bounds,
            },
        }
    }

    fn preflight_response(request: &PreflightRequest) -> serde_json::Value {
        serde_json::to_value(PreflightResponse {
            version: 3,
            kind: "preflight-result".into(),
            bindings: request.bindings.clone(),
            challenge: request.challenge.clone(),
            observation: PreflightObservation {
                version: 1,
                stable_resource_id: request.inputs.resource_selection.stable_id.clone(),
                device_binding_digest: request
                    .inputs
                    .resource_selection
                    .device_binding_digest
                    .clone(),
                primary_device: request.inputs.validate().unwrap().clone(),
                uuid_v2_hex: "1".repeat(32),
                nvml_shutdown: PreflightNvmlClose::Closed,
            },
        })
        .unwrap()
    }

    #[test]
    fn preflight_closed_nested_policy_selection_and_result_grammar() {
        let request = preflight_request();
        request.validate().unwrap();
        let original = serde_json::to_value(&request).unwrap();
        for pointer in [
            "",
            "/bindings",
            "/inputs",
            "/inputs/policy",
            "/inputs/resource_selection",
            "/inputs/resource_selection/character_devices/0",
            "/inputs/gnu_bounds",
        ] {
            let mut altered = original.clone();
            altered
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown".into(), serde_json::json!(true));
            assert!(
                serde_json::from_value::<PreflightRequest>(altered).is_err(),
                "{pointer}"
            );
        }
        let mut missing_nullable = original.clone();
        missing_nullable["inputs"]["policy"]
            .as_object_mut()
            .unwrap()
            .remove("l4_query_policy");
        assert!(serde_json::from_value::<PreflightRequest>(missing_nullable).is_err());
        let response = preflight_response(&request);
        let bytes = framed(&serde_json::to_vec(&response).unwrap());
        // Parsing one complete candidate does not require EOF or accept a proof.
        read_preflight_response(&mut Cursor::new(&bytes), &request).unwrap();
        for pointer in [
            "/challenge",
            "/bindings/process_owner",
            "/observation/device_binding_digest",
            "/observation/uuid_v2_hex",
            "/observation/nvml_shutdown",
        ] {
            let mut altered = response.clone();
            *altered.pointer_mut(pointer).unwrap() = serde_json::json!("foreign");
            assert!(
                read_preflight_response(
                    &mut Cursor::new(framed(&serde_json::to_vec(&altered).unwrap())),
                    &request
                )
                .is_err()
            );
        }
        for end in 0..bytes.len() {
            assert!(read_preflight_response(&mut Cursor::new(&bytes[..end]), &request).is_err());
        }
        for length in [0u32, 4097, u32::MAX] {
            assert!(
                read_preflight_response(&mut Cursor::new(length.to_be_bytes()), &request).is_err()
            );
        }
    }

    #[test]
    fn preflight_requires_explicit_mode_and_candidate_before_init_with_sticky_failure() {
        let deadline = lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(1));
        let (mut channel, _actual_peer) =
            TrustedCleanupChannel::prepare_preflight(deadline).unwrap();
        assert!(channel.queue_init(bindings(), &startup()).is_err());
        assert!(matches!(channel.phase, Phase::Failed));
        let request = preflight_request();
        assert!(
            channel
                .queue_preflight(request.bindings, request.challenge, request.inputs)
                .is_err()
        );
        let (mut legacy, _legacy_peer) = TrustedCleanupChannel::prepare(deadline).unwrap();
        let request = preflight_request();
        assert!(
            legacy
                .queue_preflight(request.bindings, request.challenge, request.inputs)
                .is_err()
        );
        assert!(matches!(legacy.phase, Phase::Failed));
        assert!(legacy.queue_init(bindings(), &startup()).is_err());
    }

    #[test]
    fn preflight_expired_original_clock_retains_actual_channel_and_request() {
        let (channel, _peer) = lillux::inherited_duplex_channel_pair().unwrap();
        let request = preflight_request();
        let mut owner = TrustedCleanupChannel {
            channel,
            bindings: None,
            deadline: lillux::time::MonotonicDeadline::after(std::time::Duration::ZERO),
            phase: Phase::PreflightQueued,
            startup_mode: StartupMode::Preflight,
            preflight: Some(request),
            preflight_candidate: None,
        };
        assert!(owner.receive_preflight_candidate().is_err());
        assert!(matches!(owner.phase, Phase::Failed));
        assert!(owner.preflight.is_some());
        assert!(owner.receive_preflight_candidate().is_err());
    }

    #[test]
    fn cleanup_wire_requires_exact_single_retired_and_write_closure() {
        let bindings = bindings();
        let mut bytes = Vec::new();
        write_frame(&mut bytes, "retired", &bindings).unwrap();
        read_retired(&mut Cursor::new(&bytes), &bindings).unwrap();
        let mut duplicate = bytes.clone();
        duplicate.extend_from_slice(&bytes);
        assert!(read_retired(&mut Cursor::new(duplicate), &bindings).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(read_retired(&mut Cursor::new(trailing), &bindings).is_err());
        for end in 0..bytes.len() {
            assert!(read_retired(&mut Cursor::new(&bytes[..end]), &bindings).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn exact_retired_from_live_peer_without_eof_is_only_a_candidate() {
        let bindings = bindings();
        let (parent, child) = lillux::inherited_duplex_channel_pair().unwrap();
        let mut request = lillux::SubprocessRequest {
            cmd: "/bin/sh".into(),
            argv0: None,
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 5.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        child
            .bind_to_subprocess_request(&mut request, CHANNEL_ENV, TARGET_FD)
            .unwrap();
        drop(child); // Only the Lillux-launched peer may keep this write end open.

        let mut frame = Vec::new();
        write_frame(&mut frame, "retired", &bindings).unwrap();
        let octal = frame
            .iter()
            .map(|byte| format!("\\0{byte:03o}"))
            .collect::<String>();
        // The shell writes the exact frame, duplicates fd 4, closes fd 4,
        // then blocks reading from its duplicate. A frame is not write closure.
        request.args = vec![
            "-c".into(),
            format!("printf '{octal}' >&4; exec 5>&4; exec 4>&-; read hold <&5"),
        ];
        let process = lillux::spawn(request).unwrap();
        let mut channel = TrustedCleanupChannel {
            channel: parent,
            bindings: Some(bindings),
            deadline: lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(2)),
            phase: Phase::ShutdownSent,
            startup_mode: StartupMode::InitFirst,
            preflight: None,
            preflight_candidate: None,
        };

        let candidate = channel.receive_candidate();
        let process_cleanup = process.abort_and_reap_checked();
        assert!(process_cleanup.is_ok(), "{process_cleanup:?}");
        let Err(error) = candidate else {
            panic!("retired without EOF cannot become a candidate");
        };
        assert!(
            format!("{error:#}").contains("observe cleanup producer write closure"),
            "{error:#}"
        );
        assert!(matches!(channel.phase, Phase::Failed));
    }

    #[test]
    fn cleanup_wire_refuses_foreign_coordinates_and_protocol_shapes() {
        let expected = bindings();
        let original = serde_json::to_value(Frame::new("retired", &expected)).unwrap();
        for name in [
            "teardown",
            "capsule_hash",
            "cleanup_contract",
            "process_owner",
            "resource_set",
        ] {
            let mut altered = original.clone();
            altered[name] = serde_json::json!("a".repeat(64));
            assert!(
                read_retired(
                    &mut Cursor::new(framed(&serde_json::to_vec(&altered).unwrap())),
                    &expected
                )
                .is_err()
            );
        }
        for name in ["init", "shutdown", "error", "final", "observation_batch"] {
            let bytes = framed(&serde_json::to_vec(&Frame::new(name, &expected)).unwrap());
            assert!(read_retired(&mut Cursor::new(bytes), &expected).is_err());
        }
        let body = serde_json::to_string(&original).unwrap();
        let duplicate = body.replacen('{', "{\"version\":1,", 1);
        let extra = body.replacen('{', "{\"verified\":true,", 1);
        for body in [
            duplicate.as_bytes(),
            extra.as_bytes(),
            b"null",
            b"\xff",
            b"{}",
        ] {
            assert!(read_retired(&mut Cursor::new(framed(body)), &expected).is_err());
        }
        for length in [0u32, 4097, u32::MAX] {
            assert!(read_retired(&mut Cursor::new(length.to_be_bytes()), &expected).is_err());
        }
    }
    fn startup() -> CleanupStartupComparisons {
        let bounds = || ryeos_state::external_content::products::ProductBounds {
            maximum_entries: 8,
            maximum_depth: 4,
            maximum_file_bytes: 16,
            maximum_total_bytes: 32,
        };
        CleanupStartupComparisons {
            uuid_v2_hex: "01000000000000000000000000000000".into(),
            gnu_bounds: bounds(),
            driver_bounds: bounds(),
        }
    }

    #[test]
    fn init_v2_serializes_exact_required_comparisons_and_keeps_retirement_v1() {
        let expected = bindings();
        let mut bytes = Vec::new();
        write_init(&mut bytes, &expected, &startup()).unwrap();
        let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(length, bytes.len() - 4);
        let value: serde_json::Value = serde_json::from_slice(&bytes[4..]).unwrap();
        let keys: std::collections::BTreeSet<_> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([
                "version",
                "kind",
                "teardown",
                "capsule_hash",
                "cleanup_contract",
                "process_owner",
                "resource_set",
                "uuid_v2_hex",
                "gnu_bounds",
                "driver_bounds"
            ])
        );
        assert_eq!(value["version"], 2);
        assert_eq!(value["kind"], "init");
        assert_eq!(value["uuid_v2_hex"], "01000000000000000000000000000000");
        for name in ["gnu_bounds", "driver_bounds"] {
            let keys: std::collections::BTreeSet<_> = value[name]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                std::collections::BTreeSet::from([
                    "maximum_entries",
                    "maximum_depth",
                    "maximum_file_bytes",
                    "maximum_total_bytes"
                ])
            );
        }
        assert!(read_retired(&mut Cursor::new(bytes), &expected).is_err());
        assert!(write_frame(&mut Vec::new(), "init", &expected).is_err());
        for kind in ["shutdown", "retired"] {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, kind, &expected).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes[4..]).unwrap();
            assert_eq!(value["version"], 1);
            assert!(value.get("uuid_v2_hex").is_none());
        }
    }

    #[test]
    fn invalid_init_comparisons_write_nothing_and_cannot_default() {
        let expected = bindings();
        for uuid in [
            "",
            "00000000000000000000000000000000",
            "ABCDEF00000000000000000000000000",
            "1",
        ] {
            let mut startup = startup();
            startup.uuid_v2_hex = uuid.into();
            let mut bytes = Vec::new();
            assert!(write_init(&mut bytes, &expected, &startup).is_err());
            assert!(bytes.is_empty());
        }
        for invalid_gnu in [true, false] {
            for field in 0..5 {
                let mut startup = startup();
                let bounds = if invalid_gnu {
                    &mut startup.gnu_bounds
                } else {
                    &mut startup.driver_bounds
                };
                match field {
                    0 => bounds.maximum_entries = 0,
                    1 => bounds.maximum_depth = 0,
                    2 => bounds.maximum_file_bytes = 0,
                    3 => bounds.maximum_total_bytes = 0,
                    _ => bounds.maximum_file_bytes = bounds.maximum_total_bytes + 1,
                }
                let mut bytes = Vec::new();
                assert!(write_init(&mut bytes, &expected, &startup).is_err());
                assert!(bytes.is_empty());
            }
        }
    }
}
