//! Loopback channel controller: the controller half of the supervisor wire for
//! one occurrence. It attaches the supervisor, authenticates every supervisor
//! frame against the binding it issued, acknowledges each as applied, and
//! keeps a transcript. It holds no durable journal: the probe is one-shot, so
//! a replayed supervisor frame is answered from the retained response.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, bail, ensure};
use lillux::crypto::SigningKey;
use ryeos_external_execution::launcher::ExternalCandidateLauncherSpec;
use ryeos_external_execution_contract::EXTERNAL_CANDIDATE_LAUNCHER_SPEC_SCHEMA;
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_ATTACH_PATH, EXTERNAL_CHANNEL_EXCHANGE_PATH,
    EXTERNAL_CHANNEL_TRANSPORT_SCHEMA, ExternalChannelAttachRequest, ExternalChannelAttachResponse,
    ExternalChannelExchangeRequest, ExternalChannelExchangeResponse, ExternalChannelResponseFrame,
    ExternalSupervisorBootstrap,
};
use ryeos_state::external_execution::{
    ChannelDirection, EXECUTION_CHANNEL_BINDING_SCHEMA, ExecutionChannelBinding,
    ExecutionChannelPayload, ExecutionFrame, ExecutionFrameApplication, SignedExecutionFrame,
};

use crate::loopback_tls::{HttpReply, HttpsRequest};

/// What the controller observed, in the form the evidence reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeOutcome {
    pub transcript_digest: String,
    pub frame_count: u64,
    pub applied_launch_receipt_digest: Option<String>,
    pub candidate_program_digest: Option<String>,
    pub launcher_spec_digest: Option<String>,
    pub launcher_occurrence_digest: Option<String>,
    pub command_terminated: bool,
}

pub struct Controller {
    bootstrap: ExternalSupervisorBootstrap,
    key: SigningKey,
    expected_ready_inputs: Vec<String>,
    attach: Option<(String, ExecutionChannelBinding)>,
    transcript: Vec<serde_json::Value>,
    supervisor_frames: BTreeMap<u64, (String, Vec<u8>)>,
    responses: BTreeMap<u64, Vec<u8>>,
    last_supervisor_digest: Option<String>,
    last_owner_digest: Option<String>,
    owner_sequence: u64,
    saw_ready: bool,
    receipt_digest: Option<String>,
    candidate_program_digest: Option<String>,
    launcher_spec_digest: Option<String>,
    launcher_occurrence_digest: Option<String>,
    command_terminated: bool,
}

fn json_reply(status: u16, value: &impl serde::Serialize) -> Result<HttpReply> {
    Ok(HttpReply {
        status,
        body: serde_json::to_vec(value)?,
    })
}

impl Controller {
    pub fn new(
        bootstrap_bytes: &[u8],
        key: SigningKey,
        expected_ready_inputs: Vec<String>,
    ) -> Result<Self> {
        let bootstrap: ExternalSupervisorBootstrap = serde_json::from_slice(bootstrap_bytes)?;
        bootstrap.validate()?;
        ensure!(
            bootstrap.owner_public_key
                == ryeos_state::external_execution::encode_channel_public_key(
                    &key.verifying_key()
                )?,
            "controller key differs from the sealed bootstrap owner key"
        );
        Ok(Self {
            bootstrap,
            key,
            expected_ready_inputs,
            attach: None,
            transcript: Vec::new(),
            supervisor_frames: BTreeMap::new(),
            responses: BTreeMap::new(),
            last_supervisor_digest: None,
            last_owner_digest: None,
            owner_sequence: 0,
            saw_ready: false,
            receipt_digest: None,
            candidate_program_digest: None,
            launcher_spec_digest: None,
            launcher_occurrence_digest: None,
            command_terminated: false,
        })
    }

    pub fn outcome(&self) -> Result<ExchangeOutcome> {
        Ok(ExchangeOutcome {
            transcript_digest: lillux::sha256_hex(
                lillux::canonical_json(&serde_json::Value::Array(self.transcript.clone()))?
                    .as_bytes(),
            ),
            frame_count: self.transcript.len() as u64,
            applied_launch_receipt_digest: self.receipt_digest.clone(),
            candidate_program_digest: self.candidate_program_digest.clone(),
            launcher_spec_digest: self.launcher_spec_digest.clone(),
            launcher_occurrence_digest: self.launcher_occurrence_digest.clone(),
            command_terminated: self.command_terminated,
        })
    }

    /// Route one HTTP request. A returned error is a protocol violation by the
    /// peer or a controller fault; the one-shot probe fails on it.
    pub fn handle(&mut self, request: HttpsRequest) -> Result<HttpReply> {
        ensure!(request.method == "POST", "channel route accepts only POST");
        match request.path.as_str() {
            EXTERNAL_CHANNEL_ATTACH_PATH => self.attach(&request.body),
            EXTERNAL_CHANNEL_EXCHANGE_PATH => self.exchange(&request.body),
            other => bail!("unknown channel route {other}"),
        }
    }

    fn attach(&mut self, body: &[u8]) -> Result<HttpReply> {
        let request: ExternalChannelAttachRequest = serde_json::from_slice(body)?;
        request.validate_for_bootstrap(&self.bootstrap)?;
        let request_digest = request.digest()?;
        if let Some((retained, binding)) = &self.attach {
            ensure!(
                *retained == request_digest,
                "a different supervisor tried to attach"
            );
            return json_reply(
                200,
                &ExternalChannelAttachResponse {
                    schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
                    binding_digest: binding.digest()?,
                    binding: binding.clone(),
                },
            );
        }
        let now = lillux::time::timestamp_millis();
        ensure!(
            now < self.bootstrap.attachment_deadline_ms,
            "attachment deadline elapsed"
        );
        let execution_ms = i64::from(self.bootstrap.execution_timeout_seconds) * 1_000;
        let post_ms = i64::from(self.bootstrap.post_execution_timeout_seconds) * 1_000;
        let binding = ExecutionChannelBinding {
            schema: EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode: self.bootstrap.candidate_program.execution_mode(),
            placement_thread_id: self.bootstrap.placement_thread_id.clone(),
            allocation_request_digest: self.bootstrap.allocation_request_digest.clone(),
            occurrence_id: self.bootstrap.occurrence_id.clone(),
            admitted_capsule_hash: self.bootstrap.admitted_capsule_hash.clone(),
            base_snapshot_hash: self.bootstrap.base_snapshot_hash.clone(),
            execution_binding_hash: self.bootstrap.execution_binding_hash.clone(),
            supervisor_runtime_hash: self.bootstrap.supervisor_runtime_hash.clone(),
            candidate_program_digest: self.bootstrap.candidate_program.digest()?,
            channel_nonce: hex::encode(lillux::crypto::generate_random_bytes::<32>()),
            owner_public_key: self.bootstrap.owner_public_key.clone(),
            supervisor_public_key: request.supervisor_public_key.clone(),
            issued_at_ms: now,
            execution_deadline_ms: now + execution_ms,
            expires_at_ms: now + execution_ms + post_ms,
            candidate_export_max_bytes: self.bootstrap.candidate_export_max_bytes,
            max_frames: self.bootstrap.binding_max_frames()?,
            max_bytes: self.bootstrap.channel_max_bytes,
        };
        self.bootstrap
            .validate_attached_binding(&binding, &request.supervisor_public_key)?;
        // The controller derives the launcher specification it expects the
        // launcher to accept; the launcher never echoes a specification
        // digest, so this is an expectation, not an observation from its own binding, program and inputs.
        let spec = ExternalCandidateLauncherSpec::from_admitted_program(
            binding.clone(),
            &self.bootstrap.candidate_program,
            &self.bootstrap.guest_inputs,
        )?;
        ensure!(
            spec.schema == EXTERNAL_CANDIDATE_LAUNCHER_SPEC_SCHEMA,
            "launcher specification schema differs from the compiled pin"
        );
        self.launcher_spec_digest = Some(spec.digest()?);
        let response = ExternalChannelAttachResponse {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            binding_digest: binding.digest()?,
            binding: binding.clone(),
        };
        response.validate_for_bootstrap(&self.bootstrap, &request.supervisor_public_key)?;
        self.attach = Some((request_digest, binding));
        json_reply(200, &response)
    }

    fn exchange(&mut self, body: &[u8]) -> Result<HttpReply> {
        let request: ExternalChannelExchangeRequest = serde_json::from_slice(body)?;
        request.validate_shape()?;
        let (_, binding) = self.attach.clone().context("exchange before attachment")?;
        ensure!(
            request.placement_thread_id == binding.placement_thread_id
                && request.occurrence_id == binding.occurrence_id,
            "exchange changed its occurrence"
        );
        let wire = request.decode_frame()?;
        let verified = SignedExecutionFrame::decode_and_verify(
            &wire,
            &binding,
            lillux::time::timestamp_millis(),
        )?;
        let frame = verified.frame().clone();
        let digest = verified.digest().to_owned();
        ensure!(
            frame.direction == ChannelDirection::SupervisorToOwner,
            "exchange requires a supervisor frame"
        );
        if let Some((retained, _)) = self.supervisor_frames.get(&frame.sequence) {
            ensure!(*retained == digest, "supervisor forked a retained sequence");
            let cached = self
                .responses
                .get(&frame.sequence)
                .context("replayed frame has no retained response")?;
            let mut replay: ExternalChannelExchangeResponse = serde_json::from_slice(cached)?;
            replay.incoming_new = false;
            return json_reply(200, &replay);
        }
        let expected_sequence = self.supervisor_frames.len() as u64 + 1;
        ensure!(
            frame.sequence == expected_sequence
                && frame.previous_frame_digest == self.last_supervisor_digest,
            "supervisor frame is out of sequence"
        );
        self.record(&frame, &binding)?;
        self.last_supervisor_digest = Some(digest.clone());
        self.supervisor_frames
            .insert(frame.sequence, (digest.clone(), wire));
        self.transcript.push(serde_json::json!({
            "direction": "supervisor_to_owner",
            "sequence": frame.sequence,
            "digest": digest,
        }));
        let mut outbound = Vec::new();
        let mut acknowledgement = None;
        // The supervisor suppresses acknowledgement of acknowledgements, so a
        // peer acknowledgement ends its chain here.
        if !matches!(frame.payload, ExecutionChannelPayload::Acknowledge { .. }) {
            let owner = self.acknowledge(&frame, &digest, &binding)?;
            acknowledgement = Some(owner.digest.clone());
            outbound.push(ExternalChannelResponseFrame::new(
                owner.sequence,
                owner.digest,
                &owner.wire,
            )?);
        }
        let response = ExternalChannelExchangeResponse {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            incoming_new: true,
            incoming_sequence: frame.sequence,
            incoming_frame_digest: self.last_supervisor_digest.clone().context("digest")?,
            acknowledgement_frame_digest: acknowledgement,
            outbound_frames: outbound,
            urgent_revocation_frame: None,
        };
        response.validate_shape()?;
        let encoded = serde_json::to_vec(&response)?;
        self.responses.insert(frame.sequence, encoded);
        json_reply(200, &response)
    }

    /// Admit exactly the payloads this probe's direct command can produce.
    fn record(&mut self, frame: &ExecutionFrame, binding: &ExecutionChannelBinding) -> Result<()> {
        match &frame.payload {
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash,
                base_snapshot_hash,
                boot,
            } => {
                ensure!(
                    frame.sequence == 1
                        && !self.saw_ready
                        && *supervisor_runtime_hash == binding.supervisor_runtime_hash
                        && *base_snapshot_hash == binding.base_snapshot_hash,
                    "Ready differs from the attached binding"
                );
                let boot = boot
                    .as_ref()
                    .context("leased guest Ready carries no boot")?;
                ensure!(
                    boot.verified_input_digests == self.expected_ready_inputs,
                    "Ready names different verified inputs than the controller delivered"
                );
                self.saw_ready = true;
            }
            ExecutionChannelPayload::CapabilityReport { .. } => {
                ensure!(self.saw_ready, "capability report before Ready");
            }
            ExecutionChannelPayload::RuntimeApplied {
                candidate_program_digest,
                receipt,
                launcher_occurrence_digest,
            } => {
                ensure!(self.saw_ready, "applied launch before Ready");
                ensure!(
                    *candidate_program_digest == binding.candidate_program_digest
                        && lillux::valid_hash(launcher_occurrence_digest),
                    "applied launch names a different program"
                );
                ensure!(self.receipt_digest.is_none(), "second applied launch");
                self.receipt_digest = Some(lillux::sha256_hex(
                    lillux::canonical_json(&serde_json::to_value(receipt)?)?.as_bytes(),
                ));
                self.candidate_program_digest = Some(candidate_program_digest.clone());
                self.launcher_occurrence_digest = Some(launcher_occurrence_digest.clone());
            }
            ExecutionChannelPayload::CommandOutput { .. } => {
                ensure!(
                    self.receipt_digest.is_some(),
                    "command output before applied launch"
                );
            }
            ExecutionChannelPayload::CommandTerminated { .. } => {
                ensure!(
                    self.receipt_digest.is_some(),
                    "command termination before applied launch"
                );
                self.command_terminated = true;
            }
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence,
                peer_frame_digest,
                ..
            } => {
                // Evidence of the supervisor's receipt of our frames; it must
                // name a frame this controller authored.
                ensure!(
                    *peer_frame_sequence <= self.owner_sequence
                        && lillux::valid_hash(peer_frame_digest),
                    "acknowledgement names an owner frame that was never issued"
                );
            }
            _ => bail!("supervisor sent a payload outside the probe's direct command lane"),
        }
        Ok(())
    }

    fn acknowledge(
        &mut self,
        frame: &ExecutionFrame,
        digest: &str,
        binding: &ExecutionChannelBinding,
    ) -> Result<OwnerFrame> {
        let sequence = self.owner_sequence + 1;
        let owner = ExecutionFrame {
            schema: 1,
            binding_digest: binding.digest()?,
            direction: ChannelDirection::OwnerToSupervisor,
            sequence,
            previous_frame_digest: self.last_owner_digest.clone(),
            acknowledged_peer_sequence: frame.sequence,
            payload: ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: frame.sequence,
                peer_frame_digest: digest.to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        };
        let signed = SignedExecutionFrame::sign(owner, binding, &self.key)?;
        let wire = lillux::canonical_json(&serde_json::to_value(&signed)?)?.into_bytes();
        let owner_digest = lillux::sha256_hex(&wire);
        self.owner_sequence = sequence;
        self.last_owner_digest = Some(owner_digest.clone());
        self.transcript.push(serde_json::json!({
            "direction": "owner_to_supervisor",
            "sequence": sequence,
            "digest": owner_digest,
        }));
        Ok(OwnerFrame {
            sequence,
            digest: owner_digest,
            wire,
        })
    }
}

struct OwnerFrame {
    sequence: u64,
    digest: String,
    wire: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopback_tls::{EphemeralPki, LoopbackHttpsServer};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ryeos_external_candidate_supervisor::attach_external_execution_channel;
    use ryeos_external_execution::transport::ExternalExecutionChannelTransport as _;
    use ryeos_external_execution_contract::{
        ExternalGuestInputProjection, GuestBaseSnapshotInput, GuestMountAccess,
        GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
        GuestProductManifestKind, GuestWorkspaceView,
    };
    use ryeos_state::external_execution::SupervisorReadyBoot;
    use ryeos_state::external_execution::admission::{
        AdmittedExternalCandidateProgram, ExternalCandidateExecutionRoute,
        ExternalCandidateProcFilesystem, ExternalCandidateRequirement,
        ExternalCandidateRuntimeAuthority, ExternalCandidateRuntimeRecipe,
        ExternalCandidateRuntimeSource, PROTOCOL,
    };
    use ryeos_state::external_execution::transport::{
        EXTERNAL_CHANNEL_ROUTE_CONTRACT, EXTERNAL_SUPERVISOR_HTTP_BOOTSTRAP_SCHEMA,
        ExternalCapturedNetworkInputs, ExternalControllerTransportContract,
        ExternalNetworkInputPolicy, ExternalNetworkInputSelection,
        ExternalSupervisorNativeEnvelope, external_tls_root_bundle_digest,
    };

    fn bootstrap(port: u16, root_der: &[u8], owner: &SigningKey) -> ExternalSupervisorBootstrap {
        let roots = vec![STANDARD.encode(root_der)];
        let recipe = ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        };
        let runtime_recipe_digest = recipe.digest().unwrap();
        let guest_inputs = ExternalGuestInputProjection {
            workspace_view: GuestWorkspaceView::FullSnapshot,
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 55,
                snapshot_hash: "c".repeat(64),
                closure_digest: "5".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![GuestMountInput {
                role: GuestMountRole::Product,
                authority_id: "runtime".into(),
                descriptor: 64,
                destination: "/runtime".into(),
                kind: GuestMountKind::Directory,
                access: GuestMountAccess::ReadOnly,
                normalized_mode: None,
                content_authority: GuestMountContentAuthority::ProductManifest {
                    manifest_kind: GuestProductManifestKind::Content,
                    manifest_hash: "e".repeat(64),
                    manifest_descriptor: 65,
                    manifest_bytes: 256,
                },
                bytes: 1,
            }],
            executable_search: vec!["/runtime/bin".into()],
            environment: BTreeMap::new(),
        };
        let guest_input_identity = guest_inputs.identity_digest().unwrap();
        let requirement = ExternalCandidateRequirement {
            schema: 7,
            required_lifecycle_capabilities: Default::default(),
            protocol: PROTOCOL.into(),
            connector_protocol: ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL
                .into(),
            execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "runtime".into(),
            runtime_authority: ExternalCandidateRuntimeAuthority::CapturedProduct,
            runtime_recipe: recipe,
        };
        let qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        ExternalSupervisorBootstrap {
            schema: EXTERNAL_SUPERVISOR_HTTP_BOOTSTRAP_SCHEMA,
            controller: ExternalControllerTransportContract {
                schema: 2,
                network_inputs: ExternalNetworkInputPolicy {
                    resolver: ExternalNetworkInputSelection {
                        source: "/etc/resolv.conf".into(),
                        max_bytes: 64 * 1024,
                    },
                    hosts: ExternalNetworkInputSelection {
                        source: "/etc/hosts".into(),
                        max_bytes: 64 * 1024,
                    },
                },
                https_origin: format!("https://127.0.0.1:{port}"),
                route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
                tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
                connect_timeout_ms: 2_000,
                request_timeout_ms: 5_000,
                maximum_response_bytes: 256 * 1024,
            }
            .into(),
            tls_root_certificates_der_base64: roots,
            placement_thread_id: "T-controller-test".into(),
            occurrence_id: "occurrence-controller-test".into(),
            allocation_request_digest: "a".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            launcher_artifact_hash: "4".repeat(64),
            candidate_program: AdmittedExternalCandidateProgram {
                requirement,
                qualification_use,
                runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
                runtime_manifest_hash: "e".repeat(64),
                runtime_source: ExternalCandidateRuntimeSource::CapturedProduct {
                    witness_hash: "1".repeat(64),
                },
                qualification_attestation_hash: "2".repeat(64),
                selection_identity_digest: "3".repeat(64),
                runtime_recipe_digest,
            }
            .into(),
            guest_input_identity,
            guest_inputs,
            owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &owner.verifying_key(),
            )
            .unwrap(),
            bootstrap_capability: STANDARD.encode(lillux::crypto::generate_random_bytes::<32>()),
            attachment_deadline_ms: lillux::time::timestamp_millis() + 60_000,
            execution_timeout_seconds: 30,
            post_execution_timeout_seconds: 30,
            candidate_export_max_bytes: 1024 * 1024,
            channel_max_bytes: 2 * 1024 * 1024,
            native_journal: ExternalSupervisorNativeEnvelope {
                storage_bytes: 67_108_864,
                allocator_steps: 2_000_000_000,
                connections: 16,
                operation_millis: 4_860_000,
                cleanup_millis: 120_000,
                cleanup_steps: 3_000_000,
            },
            report_capabilities: false,
        }
    }

    #[test]
    fn loopback_controller_attaches_the_workspace_client_and_acknowledges_ready() {
        let pki = EphemeralPki::generate().unwrap();
        let server = LoopbackHttpsServer::bind(&pki).unwrap();
        let owner = lillux::crypto::generate_signing_key();
        let supervisor_key = lillux::crypto::generate_signing_key();
        let bootstrap = bootstrap(server.port(), pki.root_der(), &owner);
        let bytes = bootstrap.canonical_bytes().unwrap();
        let inputs = vec!["1".repeat(64), "2".repeat(64), "3".repeat(64)];
        let mut controller = Controller::new(&bytes, owner, inputs.clone()).unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let captured = ExternalCapturedNetworkInputs::from_bytes(
            &bootstrap.controller.http().unwrap().network_inputs,
            b"nameserver 127.0.0.1\n",
            b"127.0.0.1 localhost\n",
        )
        .unwrap();
        let outcome = std::thread::scope(|scope| {
            let served = scope.spawn(|| {
                server.serve(deadline, &stop, 1024 * 1024, |request| {
                    controller.handle(request)
                })
            });
            let (binding, mut channel) =
                attach_external_execution_channel(&bootstrap, &supervisor_key, &captured).unwrap();
            let ready = ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction: ChannelDirection::SupervisorToOwner,
                sequence: 1,
                previous_frame_digest: None,
                acknowledged_peer_sequence: 0,
                payload: ExecutionChannelPayload::Ready {
                    supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                    base_snapshot_hash: binding.base_snapshot_hash.clone(),
                    boot: Some(SupervisorReadyBoot {
                        boot_id: "123e4567-e89b-12d3-a456-426614174000".into(),
                        verified_input_digests: inputs.clone(),
                        ready_at_ms: binding.issued_at_ms,
                    }),
                },
            };
            let signed = SignedExecutionFrame::sign(ready, &binding, &supervisor_key).unwrap();
            let wire = lillux::canonical_json(&serde_json::to_value(&signed).unwrap())
                .unwrap()
                .into_bytes();
            let first = channel.exchange(&wire).unwrap();
            assert!(first.incoming_new);
            assert_eq!(first.outbound_frames.len(), 1);
            let owner_frame = SignedExecutionFrame::decode_and_verify(
                &first.outbound_frames[0].canonical_wire,
                &binding,
                lillux::time::timestamp_millis(),
            )
            .unwrap();
            assert!(matches!(
                owner_frame.frame().payload,
                ExecutionChannelPayload::Acknowledge {
                    application: ExecutionFrameApplication::Applied,
                    ..
                }
            ));
            // An exact replay is answered from the retained response.
            let replay = channel.exchange(&wire).unwrap();
            assert!(!replay.incoming_new);
            assert_eq!(replay.outbound_frames, first.outbound_frames);
            stop.store(true, std::sync::atomic::Ordering::Release);
            served.join().unwrap().unwrap();
            true
        });
        assert!(outcome);
        let observed = controller.outcome().unwrap();
        assert_eq!(observed.frame_count, 2);
        assert!(observed.applied_launch_receipt_digest.is_none());
        assert!(!observed.command_terminated);
        assert!(observed.launcher_spec_digest.is_some());
        assert!(observed.launcher_occurrence_digest.is_none());
    }

    #[test]
    fn controller_refuses_ready_naming_other_inputs_and_foreign_keys() {
        let owner = lillux::crypto::generate_signing_key();
        let pki = EphemeralPki::generate().unwrap();
        let bootstrap = bootstrap(1, pki.root_der(), &owner);
        let bytes = bootstrap.canonical_bytes().unwrap();
        let wrong_key = lillux::crypto::generate_signing_key();
        assert!(Controller::new(&bytes, wrong_key, Vec::new()).is_err());
        let mut controller = Controller::new(&bytes, owner, vec!["1".repeat(64)]).unwrap();
        let get = crate::loopback_tls::HttpsRequest {
            method: "GET".into(),
            path: EXTERNAL_CHANNEL_ATTACH_PATH.into(),
            body: Vec::new(),
        };
        assert!(controller.handle(get).is_err());
        let unknown = crate::loopback_tls::HttpsRequest {
            method: "POST".into(),
            path: "/other".into(),
            body: Vec::new(),
        };
        assert!(controller.handle(unknown).is_err());
        let early = crate::loopback_tls::HttpsRequest {
            method: "POST".into(),
            path: EXTERNAL_CHANNEL_EXCHANGE_PATH.into(),
            body: b"{}".to_vec(),
        };
        assert!(controller.handle(early).is_err());
    }
}
