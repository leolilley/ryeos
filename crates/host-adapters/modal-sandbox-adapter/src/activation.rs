//! One-shot Modal guest activation: the fixed staged sequence the daemon's
//! single claimed activation runs through the signed lease facade.
//!
//! The daemon has already committed one durable activation claim before the
//! adapter is invoked, so every step below may have reached the facade even
//! when its reply is lost. The sequence is never retried and reconciliation
//! never calls it. Whatever the facade answers, the adapter attests nothing
//! about the guest: the stage and seal digests are the facade's testimony
//! about files it holds, and only the guest's own authenticated channel can
//! establish that it is running.

use anyhow::{Context as _, Result, ensure};
use lillux::InheritedDescriptorAuthority;
use ryeos_external_execution_contract::guest_import_authorization::{
    MAX_GUEST_IMPORT_AUTHORIZATION_BYTES, MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES,
    SignedGuestImportAuthorization, SignedGuestOccurrenceAssignment,
};
use ryeos_external_execution_contract::{
    LifecycleAdapterRequest, canonical_json, from_json_slice_strict,
};
use serde::Deserialize;

use crate::facade::{
    CallError, FacadeClient, FacadeTransport, Reply, RunOnceObservation, SealObservation,
    StageObservation, UploadObservation,
};
use crate::provider_spec::{
    LAUNCHER_UPLOAD_BYTES, RUN_STDIN_BYTES, STAGE_CHUNK_BYTES, STAGE_SLOT_BYTES, valid_sandbox_id,
};

/// Slot names are opaque to the facade; these are the adapter's fixed choice.
pub const SLOT_RUNTIME: &str = "runtime";
pub const SLOT_SIGNED_IMPORT: &str = "signed-import";
pub const SLOT_PACKAGE: &str = "package";

/// The package descriptor is a read-only sealed file.
const PACKAGE_FILE_MODE: u32 = 0o400;

/// One sealed input of the activation, measured and streamed exactly.
pub(crate) trait SealedInput {
    fn bytes(&self) -> u64;
    fn sha256(&self) -> &str;
    /// Re-measure the input against its declared identity. A point
    /// observation: the streaming read re-checks the digest on its own.
    fn verify(&self) -> Result<()>;
    /// Visit the exact bytes in order, at most `chunk_bytes` at a time, with
    /// each chunk's offset. Fails when the content differs from the declared
    /// identity, including after the last chunk was visited.
    fn stream(
        &self,
        chunk_bytes: usize,
        visit: &mut dyn FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<()>;

    fn read_all(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(usize::try_from(self.bytes()).unwrap_or(0));
        self.stream(1024 * 1024, &mut |_, chunk| {
            out.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(out)
    }
}

/// An input held in memory.
pub(crate) struct MemoryInput<'a> {
    bytes: &'a [u8],
    sha256: String,
}

impl<'a> MemoryInput<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            sha256: lillux::sha256_hex(bytes),
            bytes,
        }
    }
}

impl SealedInput for MemoryInput<'_> {
    fn bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn sha256(&self) -> &str {
        &self.sha256
    }

    fn verify(&self) -> Result<()> {
        ensure!(
            lillux::sha256_hex(self.bytes) == self.sha256,
            "staged input changed"
        );
        Ok(())
    }

    fn stream(
        &self,
        chunk_bytes: usize,
        visit: &mut dyn FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut offset = 0u64;
        for chunk in self.bytes.chunks(chunk_bytes) {
            visit(offset, chunk)?;
            offset += chunk.len() as u64;
        }
        Ok(())
    }
}

/// An input behind an inherited, sealed descriptor.
pub(crate) struct DescriptorInput<'a> {
    authority: &'a InheritedDescriptorAuthority,
    bytes: u64,
    sha256: String,
    required_mode: Option<u32>,
}

impl<'a> DescriptorInput<'a> {
    pub(crate) fn new(
        authority: &'a InheritedDescriptorAuthority,
        bytes: u64,
        sha256: String,
        required_mode: Option<u32>,
    ) -> Self {
        Self {
            authority,
            bytes,
            sha256,
            required_mode,
        }
    }
}

impl SealedInput for DescriptorInput<'_> {
    fn bytes(&self) -> u64 {
        self.bytes
    }

    fn sha256(&self) -> &str {
        &self.sha256
    }

    fn verify(&self) -> Result<()> {
        self.authority.require_owned_regular()?;
        let observation = self.authority.regular_file_observation()?;
        ensure!(
            self.required_mode
                .is_none_or(|mode| observation.full_permission_mode().is_ok_and(|m| m == mode))
                && observation.size() == self.bytes
                && self
                    .authority
                    .digest_regular_file_stable_exact(&observation)?
                    == self.sha256,
            "inherited activation input changed its declared identity"
        );
        Ok(())
    }

    fn stream(
        &self,
        chunk_bytes: usize,
        visit: &mut dyn FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        use std::io::Read as _;
        let mut reader =
            self.authority
                .stable_regular_reader_exact(self.bytes, &self.sha256, self.bytes)?;
        let mut buffer = vec![0u8; chunk_bytes];
        let mut offset = 0u64;
        while offset < self.bytes {
            let want = usize::try_from((self.bytes - offset).min(chunk_bytes as u64))?;
            let mut filled = 0;
            while filled < want {
                let count = reader.read(&mut buffer[filled..want])?;
                ensure!(count > 0, "inherited activation input ended early");
                filled += count;
            }
            visit(offset, &buffer[..want])?;
            offset += want as u64;
        }
        reader.finish()
    }
}

/// Everything the sequence delivers.
pub(crate) struct ActivationInputs<'a> {
    pub runtime: &'a dyn SealedInput,
    pub signed_import: &'a dyn SealedInput,
    pub package: &'a dyn SealedInput,
    pub launcher: &'a dyn SealedInput,
    /// The signed assignment, delivered as the guest's stdin.
    pub assignment: &'a [u8],
    /// The digest the launcher must have, from the activation intent.
    pub launcher_sha256: &'a str,
}

/// Why the sequence stopped. The caller settles Pending for every one.
#[derive(Debug)]
pub(crate) enum Stopped {
    /// Nothing was sent to the facade.
    BeforeContact(anyhow::Error),
    /// A step may have reached the facade.
    /// The sequence may have reached the facade. The provider's reply is not
    /// retained: nothing of it may leave the adapter, and the outcome is
    /// pending either way.
    AfterContact,
}

fn preflight(inputs: &ActivationInputs<'_>) -> Result<Vec<u8>> {
    for (name, input) in [
        ("guest runtime", inputs.runtime),
        ("signed import", inputs.signed_import),
        ("package", inputs.package),
    ] {
        ensure!(
            (1..=STAGE_SLOT_BYTES).contains(&input.bytes()),
            "{name} is outside the facade's slot bound"
        );
        input
            .verify()
            .with_context(|| format!("{name} failed preflight"))?;
    }
    ensure!(
        (1..=LAUNCHER_UPLOAD_BYTES).contains(&inputs.launcher.bytes())
            && inputs.launcher.sha256() == inputs.launcher_sha256,
        "launcher differs from the activation intent or exceeds the upload bound"
    );
    ensure!(
        (1..=RUN_STDIN_BYTES).contains(&(inputs.assignment.len() as u64)),
        "signed assignment is outside the facade's stdin bound"
    );
    inputs
        .launcher
        .verify()
        .context("launcher failed preflight")?;
    let launcher = inputs.launcher.read_all().context("launcher read failed")?;
    ensure!(
        lillux::sha256_hex(&launcher) == inputs.launcher_sha256,
        "launcher changed during preflight"
    );
    Ok(launcher)
}

fn observed<T: for<'de> Deserialize<'de>>(
    step: &str,
    reply: Result<Reply, CallError>,
) -> Result<T> {
    let reply = reply.map_err(|error| anyhow::anyhow!("{step}: {error:?}"))?;
    reply
        .observation::<T>()
        .ok_or_else(|| anyhow::anyhow!("{step}: facade did not acknowledge ({:?})", reply.outcome))
}

fn stage_and_seal<T: FacadeTransport>(
    client: &FacadeClient<'_, T>,
    sandbox_id: &str,
    slot: &str,
    input: &dyn SealedInput,
    chunk_bytes: u64,
) -> Result<()> {
    input.stream(usize::try_from(chunk_bytes)?, &mut |offset, chunk| {
        let length = chunk.len() as u64;
        let chunk_sha256 = lillux::sha256_hex(chunk);
        let ack: StageObservation =
            observed("stage", client.stage(sandbox_id, slot, offset, chunk))?;
        ensure!(
            ack.sandbox_id == sandbox_id
                && ack.slot == slot
                && ack.offset == offset
                && ack.bytes == length
                && ack.slot_bytes == offset + length
                && ack.sha256 == chunk_sha256,
            "stage acknowledgement differs from the chunk sent"
        );
        Ok(())
    })?;
    let ack: SealObservation = observed(
        "stage-seal",
        client.stage_seal(sandbox_id, slot, input.bytes(), input.sha256()),
    )?;
    ensure!(
        ack.sandbox_id == sandbox_id
            && ack.slot == slot
            && ack.sealed
            && !ack.already_sealed
            && ack.bytes == input.bytes()
            && ack.sha256 == input.sha256(),
        "seal acknowledgement differs from the slot sent"
    );
    Ok(())
}

/// Run the staged sequence once. Call only from the first claimed
/// activation, never from reconciliation. A complete success is still only a
/// delivery: the caller settles Pending either way.
pub(crate) fn first_activation_contact<T: FacadeTransport>(
    client: &FacadeClient<'_, T>,
    sandbox_id: &str,
    inputs: &ActivationInputs<'_>,
    chunk_bytes: u64,
) -> std::result::Result<(), Stopped> {
    if !valid_sandbox_id(sandbox_id) || chunk_bytes == 0 || chunk_bytes > STAGE_CHUNK_BYTES {
        return Err(Stopped::BeforeContact(anyhow::anyhow!(
            "activation target or chunk size is invalid"
        )));
    }
    let launcher = preflight(inputs).map_err(Stopped::BeforeContact)?;
    contact(client, sandbox_id, inputs, &launcher, chunk_bytes).map_err(|_| Stopped::AfterContact)
}

fn contact<T: FacadeTransport>(
    client: &FacadeClient<'_, T>,
    sandbox_id: &str,
    inputs: &ActivationInputs<'_>,
    launcher: &[u8],
    chunk_bytes: u64,
) -> Result<()> {
    for (slot, input) in [
        (SLOT_RUNTIME, inputs.runtime),
        (SLOT_SIGNED_IMPORT, inputs.signed_import),
        (SLOT_PACKAGE, inputs.package),
    ] {
        stage_and_seal(client, sandbox_id, slot, input, chunk_bytes)?;
    }
    let uploaded: UploadObservation = observed("upload", client.upload(sandbox_id, launcher))?;
    ensure!(
        uploaded.sandbox_id == sandbox_id
            && uploaded.bytes == launcher.len() as u64
            && uploaded.sha256 == inputs.launcher_sha256,
        "upload acknowledgement differs from the launcher sent"
    );
    let run: RunOnceObservation =
        observed("run-once", client.run_once(sandbox_id, inputs.assignment))?;
    ensure!(
        run.sandbox_id == sandbox_id
            && run.released
            && run.stdin_bytes == inputs.assignment.len() as u64
            && run.stdin_sha256 == lillux::sha256_hex(inputs.assignment),
        "run-once acknowledgement differs from the assignment sent"
    );
    Ok(())
}

/// The sealed inputs of one first activation, verified against the request.
pub struct PreparedActivation {
    signed_import: Vec<u8>,
    assignment: Vec<u8>,
    package: InheritedDescriptorAuthority,
    package_bytes: u64,
    package_sha256: String,
    runtime: InheritedDescriptorAuthority,
    runtime_bytes: u64,
    runtime_sha256: String,
    launcher: InheritedDescriptorAuthority,
    launcher_bytes: u64,
    launcher_sha256: String,
}

/// Adopt the descriptor named by an inherited environment variable without
/// consuming the variable.
///
/// # Safety
/// The trusted lifecycle runner must have installed the named descriptor once
/// for this process and no other owner may hold it.
unsafe fn adopt_env_descriptor(name: &str) -> Result<InheritedDescriptorAuthority> {
    let value =
        std::env::var(name).with_context(|| format!("missing inherited descriptor {name}"))?;
    ensure!(
        value.len() <= 32,
        "inherited descriptor {name} exceeds its bound"
    );
    let descriptor: u32 = value
        .parse()
        .with_context(|| format!("inherited descriptor {name} is invalid"))?;
    // SAFETY: upheld by this function's caller.
    unsafe { lillux::take_inherited_descriptor_authority(descriptor) }.map_err(anyhow::Error::msg)
}

impl PreparedActivation {
    /// Read and verify every sealed input of a first activation. Runs before
    /// any credential or network input is read.
    ///
    /// # Safety
    /// The trusted lifecycle runner must have installed each named
    /// descriptor exactly once for this process.
    pub unsafe fn from_environment(request: &LifecycleAdapterRequest) -> Result<Self> {
        use ryeos_external_execution_contract::{
            LIFECYCLE_GUEST_RUNTIME_FD_ENV, LIFECYCLE_LAUNCHER_FD_ENV,
            LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV, LIFECYCLE_SIGNED_IMPORT_FD_ENV,
            LIFECYCLE_SUPERVISOR_FD_ENV,
        };
        let LifecycleAdapterRequest::ActivateSupervisor {
            common: _,
            occurrence: _,
            activation,
            guest_input_identity: _,
            guest_input_projection: _,
            guest_package,
            import_ticket,
        } = request
        else {
            anyhow::bail!("sealed activation inputs belong to first activation");
        };
        let signed_import = ryeos_remote_lease_core::read_sealed_env(
            LIFECYCLE_SIGNED_IMPORT_FD_ENV,
            MAX_GUEST_IMPORT_AUTHORIZATION_BYTES + 256,
        )?;
        let signed = verify_signed_import_handoff(&signed_import, request)?;
        let assignment = ryeos_remote_lease_core::read_sealed_env(
            LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV,
            MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES,
        )?;
        verify_signed_assignment_handoff(&assignment, &signed)?;
        guest_package.validate()?;
        // SAFETY: upheld by this function's caller; the package descriptor
        // is distinct from the controller's original coordinate.
        let package =
            unsafe { lillux::take_inherited_descriptor_authority(guest_package.descriptor) }
                .map_err(anyhow::Error::msg)?;
        // SAFETY: upheld by this function's caller.
        let supervisor = unsafe { adopt_env_descriptor(LIFECYCLE_SUPERVISOR_FD_ENV) }?;
        // SAFETY: upheld by this function's caller.
        let runtime = unsafe { adopt_env_descriptor(LIFECYCLE_GUEST_RUNTIME_FD_ENV) }?;
        // SAFETY: upheld by this function's caller.
        let launcher = unsafe { adopt_env_descriptor(LIFECYCLE_LAUNCHER_FD_ENV) }?;
        // The package carries the supervisor; its descriptor is measured
        // only to bind the signed import ticket and is never staged.
        let (_, supervisor_sha256) = measure(&supervisor)?;
        let (runtime_bytes, runtime_sha256) = measure(&runtime)?;
        let (launcher_bytes, launcher_sha256) = measure(&launcher)?;
        ensure!(
            launcher_sha256 == activation.launcher_artifact_hash,
            "launcher descriptor differs from the activation intent"
        );
        require_runtime_matches_intent(
            runtime_bytes,
            &runtime_sha256,
            activation.guest_runtime_artifact_bytes,
            activation.guest_runtime_artifact_hash.as_deref(),
        )?;
        require_supervisor_matches_ticket(&supervisor_sha256, &import_ticket.supervisor_sha256)?;
        let prepared = Self {
            signed_import,
            assignment,
            package,
            package_bytes: guest_package.framed_bytes,
            package_sha256: guest_package.payload_sha256.clone(),
            runtime,
            runtime_bytes,
            runtime_sha256,
            launcher,
            launcher_bytes,
            launcher_sha256,
        };
        Ok(prepared)
    }

    fn package_input(&self) -> DescriptorInput<'_> {
        DescriptorInput::new(
            &self.package,
            self.package_bytes,
            self.package_sha256.clone(),
            Some(PACKAGE_FILE_MODE),
        )
    }

    /// The one contact sequence. The caller settles Pending for every
    /// outcome except a stop before any contact, which is a refusal.
    pub(crate) fn contact<T: FacadeTransport>(
        &self,
        client: &FacadeClient<'_, T>,
        sandbox_id: &str,
    ) -> std::result::Result<(), Stopped> {
        let runtime = DescriptorInput::new(
            &self.runtime,
            self.runtime_bytes,
            self.runtime_sha256.clone(),
            None,
        );
        let launcher = DescriptorInput::new(
            &self.launcher,
            self.launcher_bytes,
            self.launcher_sha256.clone(),
            None,
        );
        let signed_import = MemoryInput::new(&self.signed_import);
        let package = self.package_input();
        let inputs = ActivationInputs {
            runtime: &runtime,
            signed_import: &signed_import,
            package: &package,
            launcher: &launcher,
            assignment: &self.assignment,
            launcher_sha256: &self.launcher_sha256,
        };
        first_activation_contact(client, sandbox_id, &inputs, STAGE_CHUNK_BYTES)
    }
}

/// Decide what a contact sequence means for the response. A stop before any
/// contact is an adapter refusal; any sequence that may have reached the
/// facade, complete or not, leaves the activation pending.
pub(crate) fn pending_unless_refused(outcome: std::result::Result<(), Stopped>) -> Result<()> {
    match outcome {
        Err(Stopped::BeforeContact(error)) => Err(error),
        Ok(()) | Err(Stopped::AfterContact) => Ok(()),
    }
}

/// The staged runtime archive is the artifact the activation intent names:
/// the launcher re-observes the extracted tree, but nothing unexpected is
/// streamed to the provider.
fn require_runtime_matches_intent(
    measured_bytes: u64,
    measured_sha256: &str,
    intent_bytes: Option<u64>,
    intent_sha256: Option<&str>,
) -> Result<()> {
    ensure!(
        intent_bytes == Some(measured_bytes) && intent_sha256 == Some(measured_sha256),
        "guest runtime descriptor differs from the activation intent"
    );
    Ok(())
}

/// The supervisor descriptor is the artifact the signed import ticket names.
fn require_supervisor_matches_ticket(measured: &str, ticket: &str) -> Result<()> {
    ensure!(
        measured == ticket,
        "supervisor descriptor differs from the signed import ticket"
    );
    Ok(())
}

fn measure(authority: &InheritedDescriptorAuthority) -> Result<(u64, String)> {
    authority.require_owned_regular()?;
    let observation = authority.regular_file_observation()?;
    let digest = authority.digest_regular_file_stable_exact(&observation)?;
    Ok((observation.size(), digest))
}

fn verify_signed_import_handoff(
    bytes: &[u8],
    request: &LifecycleAdapterRequest,
) -> Result<SignedGuestImportAuthorization> {
    let LifecycleAdapterRequest::ActivateSupervisor {
        common,
        occurrence,
        activation,
        guest_input_identity,
        guest_input_projection,
        guest_package,
        import_ticket,
    } = request
    else {
        anyhow::bail!("signed guest import requires first activation");
    };
    let signed: SignedGuestImportAuthorization =
        from_json_slice_strict(bytes, MAX_GUEST_IMPORT_AUTHORIZATION_BYTES + 256)?;
    signed.validate_shape()?;
    ensure!(
        canonical_json(&signed)? == bytes,
        "signed guest import handoff is noncanonical"
    );
    let authorization = &signed.authorization;
    ensure!(
        authorization.execution_binding_hash == common.binding_hash
            && authorization.allocation_request_digest == occurrence.request_digest
            && authorization.occurrence_id == occurrence.occurrence_id
            && authorization.activation_request_digest == activation.activation_request_digest
            && authorization.supervisor_runtime_hash == activation.supervisor_runtime_hash
            && authorization.attachment_deadline_ms == activation.attachment_deadline_ms
            && authorization.base_snapshot_hash
                == guest_input_projection.base_snapshot.snapshot_hash
            && authorization.guest_inputs == **guest_input_projection
            && authorization.ticket == **import_ticket
            && authorization.ticket.guest_input_identity == *guest_input_identity
            && authorization.ticket.payload_sha256 == guest_package.payload_sha256
            && authorization.ticket.manifest_sha256 == guest_package.manifest_sha256
            && authorization.ticket.regular_bytes == guest_package.regular_bytes
            && authorization.ticket.framed_bytes == guest_package.framed_bytes,
        "sealed signed import changed the exact activation or package"
    );
    authorization.require_fresh_admission_at(lillux::time::timestamp_millis())?;
    Ok(signed)
}

/// Correlation only; the guest verifies the node-root signature and its own
/// installed runtime identity before trusting the delegated key.
fn verify_signed_assignment_handoff(
    bytes: &[u8],
    import: &SignedGuestImportAuthorization,
) -> Result<()> {
    use lillux::crypto::{Signature, Verifier as _, VerifyingKey};
    let signed: SignedGuestOccurrenceAssignment =
        from_json_slice_strict(bytes, MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES)?;
    signed.validate_shape()?;
    ensure!(
        canonical_json(&signed)? == bytes,
        "signed guest assignment handoff is noncanonical"
    );
    let assignment = &signed.assignment;
    let authorization = &import.authorization;
    ensure!(
        assignment.placement_thread_id == authorization.placement_thread_id
            && assignment.admitted_capsule_hash == authorization.admitted_capsule_hash
            && assignment.base_snapshot_hash == authorization.base_snapshot_hash
            && assignment.execution_binding_hash == authorization.execution_binding_hash
            && assignment.allocation_request_digest == authorization.allocation_request_digest
            && assignment.occurrence_id == authorization.occurrence_id
            && assignment.activation_request_digest == authorization.activation_request_digest
            && assignment.supervisor_runtime_hash == authorization.supervisor_runtime_hash
            && assignment.guest_runtime_manifest_hash == authorization.guest_runtime_manifest_hash
            && assignment.attachment_deadline_ms == authorization.attachment_deadline_ms,
        "signed guest assignment changed the exact import coordinates"
    );
    let owner_bytes: [u8; 32] = hex::decode(&assignment.owner_public_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("signed guest assignment owner key changed length"))?;
    let owner_key = VerifyingKey::from_bytes(&owner_bytes)?;
    ensure!(
        !owner_key.is_weak(),
        "signed guest assignment owner key is weak"
    );
    let signature_bytes: [u8; 64] = hex::decode(&import.signature_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("signed guest import signature changed length"))?;
    owner_key
        .verify(
            &authorization.signing_bytes()?,
            &Signature::from_bytes(&signature_bytes),
        )
        .context("signed guest import differs from the assigned occurrence owner")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::facade::test_support::*;

    const SANDBOX: &str = "sb-abc123";

    fn sha(bytes: &[u8]) -> String {
        lillux::sha256_hex(bytes)
    }

    fn stage_ok(slot: &str, offset: u64, chunk: &[u8]) -> Step {
        envelope(
            "stage",
            200,
            json!({"ok": true, "sandbox_id": SANDBOX, "slot": slot, "offset": offset,
                   "bytes": chunk.len(), "slot_bytes": offset + chunk.len() as u64,
                   "sha256": sha(chunk)}),
        )
    }

    fn seal_ok(slot: &str, content: &[u8]) -> Step {
        envelope(
            "stage_seal",
            200,
            json!({"ok": true, "sandbox_id": SANDBOX, "slot": slot, "sealed": true,
                   "already_sealed": false, "bytes": content.len(), "sha256": sha(content)}),
        )
    }

    fn upload_ok(content: &[u8]) -> Step {
        envelope(
            "upload",
            200,
            json!({"ok": true, "sandbox_id": SANDBOX, "bytes": content.len(), "sha256": sha(content)}),
        )
    }

    fn run_ok(stdin: &[u8]) -> Step {
        envelope(
            "run_once",
            200,
            json!({"ok": true, "sandbox_id": SANDBOX, "released": true,
                   "stdin_bytes": stdin.len(), "stdin_sha256": sha(stdin)}),
        )
    }

    const RUNTIME: &[u8] = b"runtime-archive";
    const IMPORT: &[u8] = b"signed-import";
    const PACKAGE: &[u8] = b"0123456789";
    const LAUNCHER: &[u8] = b"launcher";
    const ASSIGNMENT: &[u8] = b"assignment";

    /// A fake input that counts how often it was streamed and can lie about
    /// its identity.
    struct Fake {
        content: Vec<u8>,
        declared: String,
        verified: std::cell::Cell<u32>,
        streamed: std::cell::Cell<u32>,
        verify_fails: bool,
    }

    impl Fake {
        fn new(content: &[u8]) -> Self {
            Self {
                content: content.to_vec(),
                declared: sha(content),
                verified: Default::default(),
                streamed: Default::default(),
                verify_fails: false,
            }
        }
    }

    impl SealedInput for Fake {
        fn bytes(&self) -> u64 {
            self.content.len() as u64
        }
        fn sha256(&self) -> &str {
            &self.declared
        }
        fn verify(&self) -> Result<()> {
            self.verified.set(self.verified.get() + 1);
            ensure!(!self.verify_fails, "digest mismatch");
            Ok(())
        }
        fn stream(
            &self,
            chunk_bytes: usize,
            visit: &mut dyn FnMut(u64, &[u8]) -> Result<()>,
        ) -> Result<()> {
            self.streamed.set(self.streamed.get() + 1);
            let mut offset = 0u64;
            for chunk in self.content.chunks(chunk_bytes) {
                visit(offset, chunk)?;
                offset += chunk.len() as u64;
            }
            Ok(())
        }
    }

    struct Set {
        runtime: Fake,
        import: Fake,
        package: Fake,
        launcher: Fake,
    }

    fn set() -> Set {
        Set {
            runtime: Fake::new(RUNTIME),
            import: Fake::new(IMPORT),
            package: Fake::new(PACKAGE),
            launcher: Fake::new(LAUNCHER),
        }
    }

    fn run(set: &Set, fake: &Scripted, chunk: u64) -> std::result::Result<(), Stopped> {
        let pin = pin();
        let launcher_sha = sha(LAUNCHER);
        let inputs = ActivationInputs {
            runtime: &set.runtime,
            signed_import: &set.import,
            package: &set.package,
            launcher: &set.launcher,
            assignment: ASSIGNMENT,
            launcher_sha256: &launcher_sha,
        };
        first_activation_contact(&FacadeClient::new(fake, &pin), SANDBOX, &inputs, chunk)
    }

    fn full_script(chunk: usize) -> Vec<Step> {
        let mut steps = Vec::new();
        for (slot, content) in [
            (SLOT_RUNTIME, RUNTIME),
            (SLOT_SIGNED_IMPORT, IMPORT),
            (SLOT_PACKAGE, PACKAGE),
        ] {
            let mut offset = 0u64;
            for part in content.chunks(chunk) {
                steps.push(stage_ok(slot, offset, part));
                offset += part.len() as u64;
            }
            steps.push(seal_ok(slot, content));
        }
        steps.push(upload_ok(LAUNCHER));
        steps.push(run_ok(ASSIGNMENT));
        steps
    }

    #[test]
    fn the_fixed_order_runs_each_call_exactly_once() {
        let set = set();
        let fake = Scripted::new(full_script(1024));
        run(&set, &fake, 1024).unwrap();
        assert!(fake.exhausted());
        assert_eq!(
            fake.routes(),
            [
                crate::provider_spec::RouteName::Stage,
                crate::provider_spec::RouteName::StageSeal,
                crate::provider_spec::RouteName::Stage,
                crate::provider_spec::RouteName::StageSeal,
                crate::provider_spec::RouteName::Stage,
                crate::provider_spec::RouteName::StageSeal,
                crate::provider_spec::RouteName::Upload,
                crate::provider_spec::RouteName::RunOnce,
            ]
        );
        let requests = fake.requests.borrow();
        let slots: Vec<&str> = [0usize, 2, 4]
            .iter()
            .map(|index| requests[*index].1["slot"].as_str().unwrap())
            .collect();
        assert_eq!(slots, [SLOT_RUNTIME, SLOT_SIGNED_IMPORT, SLOT_PACKAGE]);
        assert_eq!(requests[6].1["sandbox_id"], SANDBOX);
        use base64::Engine as _;
        assert_eq!(
            requests[7].1["stdin_b64"],
            base64::engine::general_purpose::STANDARD.encode(ASSIGNMENT)
        );
    }

    #[test]
    fn a_multi_chunk_slot_is_appended_at_sequential_offsets() {
        let set = set();
        let fake = Scripted::new(full_script(4));
        run(&set, &fake, 4).unwrap();
        let requests = fake.requests.borrow();
        let package: Vec<(u64, usize)> = requests
            .iter()
            .filter(|(route, body)| {
                *route == crate::provider_spec::RouteName::Stage && body["slot"] == SLOT_PACKAGE
            })
            .map(|(_, body)| {
                (
                    body["offset"].as_u64().unwrap(),
                    PACKAGE
                        .len()
                        .min(usize::try_from(body["offset"].as_u64().unwrap()).unwrap() + 4)
                        - usize::try_from(body["offset"].as_u64().unwrap()).unwrap(),
                )
            })
            .collect();
        assert_eq!(package, [(0, 4), (4, 4), (8, 2)]);
        let seal = requests
            .iter()
            .find(|(route, body)| {
                *route == crate::provider_spec::RouteName::StageSeal && body["slot"] == SLOT_PACKAGE
            })
            .unwrap();
        assert_eq!(seal.1["total_bytes"], PACKAGE.len());
        assert_eq!(seal.1["sha256"], sha(PACKAGE));
    }

    #[test]
    fn a_digest_mismatch_aborts_before_any_contact() {
        let mut set = set();
        set.package.verify_fails = true;
        let fake = Scripted::new(vec![]);
        assert!(matches!(
            run(&set, &fake, 1024),
            Err(Stopped::BeforeContact(_))
        ));
        assert!(fake.requests.borrow().is_empty());
        let mut set = self::set();
        set.runtime.verify_fails = true;
        assert!(matches!(
            run(&set, &fake, 1024),
            Err(Stopped::BeforeContact(_))
        ));
        let mut set = self::set();
        set.launcher.declared = sha(b"other");
        assert!(matches!(
            run(&set, &fake, 1024),
            Err(Stopped::BeforeContact(_))
        ));
        assert!(fake.requests.borrow().is_empty());
    }

    #[test]
    fn a_lost_reply_stops_the_sequence_without_a_retry() {
        let set = set();
        let fake = Scripted::new(vec![
            stage_ok(SLOT_RUNTIME, 0, RUNTIME),
            Step::Fail(crate::facade::TransportFailure::MayHaveBeenSent),
        ]);
        assert!(matches!(run(&set, &fake, 1024), Err(Stopped::AfterContact)));
        assert_eq!(fake.requests.borrow().len(), 2);
        assert!(fake.exhausted());
    }

    #[test]
    fn a_refusal_or_a_mismatched_acknowledgement_stops_the_sequence() {
        for steps in [
            vec![refusal("stage", "offset_mismatch")],
            vec![stage_ok(SLOT_RUNTIME, 0, b"different")],
            vec![
                stage_ok(SLOT_RUNTIME, 0, RUNTIME),
                refusal("stage_seal", "seal_digest_mismatch"),
            ],
        ] {
            let count = steps.len();
            let set = set();
            let fake = Scripted::new(steps);
            assert!(matches!(run(&set, &fake, 1024), Err(Stopped::AfterContact)));
            assert_eq!(fake.requests.borrow().len(), count);
        }
    }

    #[test]
    fn a_refused_run_once_is_the_last_call() {
        let set = set();
        let mut steps = full_script(1024);
        steps.pop();
        steps.push(refusal("run_once", "slot_not_sealed"));
        let fake = Scripted::new(steps);
        assert!(matches!(run(&set, &fake, 1024), Err(Stopped::AfterContact)));
        assert_eq!(fake.requests.borrow().len(), 8);
    }

    #[test]
    fn oversized_or_invalid_targets_never_reach_the_facade() {
        let fake = Scripted::new(vec![]);
        let set = set();
        let pin = pin();
        let launcher_sha = sha(LAUNCHER);
        let inputs = ActivationInputs {
            runtime: &set.runtime,
            signed_import: &set.import,
            package: &set.package,
            launcher: &set.launcher,
            assignment: &vec![0u8; usize::try_from(RUN_STDIN_BYTES).unwrap() + 1],
            launcher_sha256: &launcher_sha,
        };
        let client = FacadeClient::new(&fake, &pin);
        assert!(matches!(
            first_activation_contact(&client, SANDBOX, &inputs, 1024),
            Err(Stopped::BeforeContact(_))
        ));
        assert!(matches!(
            first_activation_contact(&client, "sbx-1", &inputs, 1024),
            Err(Stopped::BeforeContact(_))
        ));
        assert!(matches!(
            first_activation_contact(&client, SANDBOX, &inputs, STAGE_CHUNK_BYTES + 1),
            Err(Stopped::BeforeContact(_))
        ));
        assert!(fake.requests.borrow().is_empty());
    }

    #[test]
    fn a_launcher_one_byte_over_the_adapter_cap_is_refused_before_contact() {
        let fake = Scripted::new(vec![]);
        let mut set = set();
        let oversized = vec![7u8; usize::try_from(LAUNCHER_UPLOAD_BYTES).unwrap() + 1];
        set.launcher = Fake::new(&oversized);
        let pin = pin();
        let launcher_sha = sha(&oversized);
        let inputs = ActivationInputs {
            runtime: &set.runtime,
            signed_import: &set.import,
            package: &set.package,
            launcher: &set.launcher,
            assignment: ASSIGNMENT,
            launcher_sha256: &launcher_sha,
        };
        let client = FacadeClient::new(&fake, &pin);
        assert!(matches!(
            first_activation_contact(&client, SANDBOX, &inputs, 1024),
            Err(Stopped::BeforeContact(_))
        ));
        assert!(fake.requests.borrow().is_empty());
    }

    #[test]
    fn memory_inputs_stream_exact_chunks_and_detect_change() {
        let input = MemoryInput::new(b"abcdefg");
        let mut seen = Vec::new();
        input
            .stream(3, &mut |offset, chunk| {
                seen.push((offset, chunk.to_vec()));
                Ok(())
            })
            .unwrap();
        assert_eq!(
            seen,
            [
                (0, b"abc".to_vec()),
                (3, b"def".to_vec()),
                (6, b"g".to_vec())
            ]
        );
        assert_eq!(input.read_all().unwrap(), b"abcdefg");
        input.verify().unwrap();
    }

    #[test]
    fn a_runtime_other_than_the_intent_is_refused_before_contact() {
        let digest = sha(RUNTIME);
        let bytes = RUNTIME.len() as u64;
        assert!(require_runtime_matches_intent(bytes, &digest, Some(bytes), Some(&digest)).is_ok());
        assert!(
            require_runtime_matches_intent(bytes, &sha(b"other"), Some(bytes), Some(&digest))
                .is_err()
        );
        assert!(
            require_runtime_matches_intent(bytes + 1, &digest, Some(bytes), Some(&digest)).is_err()
        );
        assert!(require_runtime_matches_intent(bytes, &digest, None, None).is_err());
    }

    #[test]
    fn a_supervisor_digest_other_than_the_ticket_is_refused_before_contact() {
        let supervisor = b"supervisor-bytes";
        let ticket = sha(supervisor);
        assert!(require_supervisor_matches_ticket(&ticket, &ticket).is_ok());
        assert!(require_supervisor_matches_ticket(&sha(b"other"), &ticket).is_err());
    }

    #[test]
    fn only_a_stop_before_contact_is_a_refusal() {
        let before = Err(Stopped::BeforeContact(anyhow::anyhow!("no contact")));
        let after = Err(Stopped::AfterContact(anyhow::anyhow!("maybe contact")));
        assert!(pending_unless_refused(before).is_err());
        assert!(pending_unless_refused(after).is_ok());
        assert!(pending_unless_refused(Ok(())).is_ok());
    }

    #[test]
    fn a_seal_that_was_already_sealed_is_refused() {
        let fake = Scripted::new(vec![
            stage_ok(SLOT_PACKAGE, 0, PACKAGE),
            envelope(
                "stage_seal",
                200,
                json!({"ok": true, "sandbox_id": SANDBOX, "slot": SLOT_PACKAGE, "sealed": true,
                       "already_sealed": true, "bytes": PACKAGE.len(), "sha256": sha(PACKAGE)}),
            ),
        ]);
        let pin = pin();
        let client = FacadeClient::new(&fake, &pin);
        let input = MemoryInput::new(PACKAGE);
        assert!(stage_and_seal(&client, SANDBOX, SLOT_PACKAGE, &input, 1024).is_err());
    }
}
