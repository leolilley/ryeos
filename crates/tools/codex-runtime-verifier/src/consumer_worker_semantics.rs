//! Finite interpretation of the actual admitted Q Worker's conversation.
//!
//! These checks do not establish native death, provider settlement, writer
//! exclusion or an issued qualification. Those facts have separate owners.
use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_codex_external_configuration::qualification_request::{
    Q_CONTROLLER_CANARY_DENIAL, controller_canary_commands, resolve_controller_canary_turn,
    validate_controller_canary_conversation,
};
use ryeos_external_execution_contract::ProviderConfigurationLaunchInput;
use ryeos_external_execution_contract::qualification_fixture::{
    MAX_Q_FIXTURE_BYTES, QFixtureContract,
};
use ryeos_state::external_execution::qualification_controller_canary::QControllerCanaryEvidence;
use ryeos_state::objects::qualification_fixture::{
    QFixtureCaptureContent, QFixtureCaptureReference, q_fixture_contract_digest,
};
use ryeos_state::objects::{QualificationWorkerTranscriptStream, canonical_value_digest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::consumer_record::ConsumerInputRecord;
use crate::consumer_worker_transcript::VerifiedConsumerWorkerTranscript;

const MAX_EVENTS: usize = 512;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QCommandExpectation {
    pub schema: String,
    pub environment_id: String,
    pub command: String,
    pub command_shell: String,
    pub command_presentation: String,
    pub cwd: String,
    pub output: String,
    pub exit_code: i32,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub expected_final_content_b64: Option<String>,
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl QCommandExpectation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == "ryeos.codex.q_command.v3",
            "unsupported Q command expectation"
        );
        ensure!(
            self.environment_id == "ryeos-external-candidate",
            "Q command must name the external Codex environment"
        );
        for value in [&self.command, &self.command_presentation] {
            ensure!(
                !value.is_empty() && value.len() <= 4096 && !value.contains('\0'),
                "Q command expectation exceeds its text bound"
            );
        }
        ensure!(
            !self.output.is_empty() && self.output.len() <= 16384 && !self.output.contains('\0'),
            "Q output expectation exceeds its text bound"
        );
        ensure!(
            self.cwd == "/workspace",
            "Q command cwd differs from the admitted external workspace"
        );
        ensure!(
            self.exit_code == 0,
            "Q positive command expectation must require exit zero"
        );
        ensure!(
            self.command_presentation_for(&self.command)? == self.command_presentation,
            "Q positive command presentation differs from its signed shell argv"
        );
        ensure!(
            self.expected_final_content_b64.is_some(),
            "Q v3 positive write requires expected final fixture content"
        );
        self.decode_expected_content(MAX_Q_FIXTURE_BYTES)?;
        Ok(())
    }

    fn command_presentation_for(&self, command: &str) -> Result<String> {
        // Pinned Codex core/shell.rs derives these three argv elements for
        // Sh/Bash/Zsh with login=false. App-server item_builders.rs displays
        // shlex::try_join(argv), then redacts secrets. Any transformed display
        // that differs is refused; this is not an applied-shell proof.
        let shell = self.command_shell.as_str();
        ensure!(
            !shell.is_empty()
                && shell.len() <= 1024
                && shell
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
                && (matches!(shell, "sh" | "bash" | "zsh")
                    || (shell.starts_with('/')
                        && shell[1..]
                            .split('/')
                            .all(|part| !part.is_empty() && part != "." && part != "..")
                        && matches!(shell.rsplit('/').next(), Some("sh" | "bash" | "zsh")))),
            "Q command shell is not an exact bounded POSIX shell argv selection"
        );
        shlex::try_join([shell, "-c", command]).context("Q command argv is not representable")
    }

    fn decode_expected_content(&self, maximum: u64) -> Result<Option<Vec<u8>>> {
        let Some(encoded) = self.expected_final_content_b64.as_ref() else {
            return Ok(None);
        };
        ensure!(
            maximum <= MAX_Q_FIXTURE_BYTES && encoded.len() as u64 <= maximum.div_ceil(3) * 4,
            "Q expected fixture content exceeds its bound"
        );
        let mut bytes = vec![0; maximum as usize];
        let count = STANDARD
            .decode_slice(encoded, &mut bytes)
            .context("Q expected fixture content is not bounded base64")?;
        bytes.truncate(count);
        ensure!(
            STANDARD.encode(&bytes) == *encoded,
            "Q expected fixture content is not canonical base64"
        );
        Ok(Some(bytes))
    }

    pub(crate) fn expected_fixture_content(
        &self,
        fixture: Option<&QFixtureContract>,
    ) -> Result<Option<Vec<u8>>> {
        self.validate()?;
        match (fixture, self.expected_final_content_b64.as_ref()) {
            (Some(fixture), Some(_)) => {
                let initial = fixture.decoded_initial_content()?;
                let expected = self
                    .decode_expected_content(fixture.max_bytes)?
                    .context("Q expected fixture bytes missing")?;
                ensure!(
                    expected != initial,
                    "Q expected final fixture content must differ from its signed initial content"
                );
                Ok(Some(expected))
            }
            _ => bail!("Q expected fixture presence differs from the signed Worker request"),
        }
    }
}

/// Verify finite product meaning against protected capture data. Native
/// settlement authority comes from the controller's retained custody join.
pub(crate) fn check_fixture_capture(
    expected: &QCommandExpectation,
    fixture: Option<&QFixtureContract>,
    reference: Option<&QFixtureCaptureReference>,
    content: Option<&QFixtureCaptureContent>,
    binding_digest: &str,
) -> Result<()> {
    let expected_bytes = expected.expected_fixture_content(fixture)?;
    match (fixture, expected_bytes, reference, content) {
        (Some(fixture), Some(expected_bytes), Some(reference), Some(content)) => {
            ensure!(
                content.metadata.fixture_contract_digest == q_fixture_contract_digest(fixture)?
                    && content.metadata.native_settlement.binding_digest == binding_digest
                    && reference.size_bytes <= fixture.max_bytes,
                "Q captured fixture changed its signed contract, native binding or bound"
            );
            ensure!(
                content.verify_content_identity(reference)? == expected_bytes,
                "Q captured fixture does not equal the expected final content"
            );
            Ok(())
        }
        _ => bail!("Q protected fixture capture presence differs from the signed expectation"),
    }
}

/// Checked app-server semantics, not a qualification result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckedQConversation {
    pub thread_id: String,
    pub turn_id: String,
    pub fixture_write_call_id: String,
    pub canary_read_call_id: String,
    pub local_refusal_call_id: String,
    pub notification_count: usize,
}

/// Constructed only after the full protected-record, fixture, setup, lifecycle
/// and three-role interpretation. Notification-only unit checks cannot supply
/// this result input. It is product meaning, not native or issuer authority.
#[derive(Debug, Serialize)]
pub struct VerifiedQConversation {
    #[serde(flatten)]
    conversation: CheckedQConversation,
    #[serde(skip)]
    original_record_sha256: String,
    #[serde(skip)]
    transcript_manifest_hash: String,
    #[serde(skip)]
    transcript_stream_digest: String,
}

impl VerifiedQConversation {
    pub(crate) fn require_for(
        &self,
        original: &ConsumerInputRecord,
        transcript: &VerifiedConsumerWorkerTranscript,
    ) -> Result<()> {
        ensure!(
            self.original_record_sha256 == lillux::cas::sha256_hex(&original.canonical_bytes()?)
                && self.transcript_manifest_hash == transcript.reference.manifest_hash
                && self.transcript_stream_digest
                    == canonical_value_digest(&serde_json::to_value(&transcript.stream)?)?,
            "checked Q conversation differs from result record or transcript"
        );
        Ok(())
    }
}

/// Interpret the transcript returned by the protected bounded reader. That
/// reader authenticates the complete chunk/batch stream; this function joins
/// product meaning to its retained frames and original signed expectations.
/// A caller-constructed public transcript is not an authority or qualification.
pub fn check_q_conversation(
    original: &ConsumerInputRecord,
    transcript: &VerifiedConsumerWorkerTranscript,
) -> Result<VerifiedQConversation> {
    crate::consumer_worker_transcript::validate_reference_for_record(
        &transcript.reference,
        original,
    )?;
    let configuration = original.scripted_configuration()?;
    let expected = &configuration.qualification_worker;
    expected.validate()?;
    let request = original
        .purpose
        .policy_source
        .policy
        .producer_scenarios
        .get(&original.coordinate.scenario_id)
        .and_then(|scenario| scenario.consumer_worker_request.as_ref())
        .context("Q semantic check has no original signed conversation")?;
    request.validate()?;
    validate_controller_canary_conversation(&request.session_setup, &request.body)?;
    let manifest = &transcript.reference.manifest;
    let canary = manifest
        .controller_canary
        .as_ref()
        .context("Q conversation has no original authenticated controller canary")?;
    canary.validate()?;
    ensure!(
        request.controller_canary.is_some(),
        "Q signed request did not select the canary roles"
    );
    let resolved = request.bound_controller_canary_body(Some(&canary.binding))?;
    ensure!(
        resolved
            == check_controller_canary_body(
                &request.body,
                canary,
                &manifest.provider_configuration
            )?,
        "Q request differs from the exact provider-owned interpretation"
    );
    ensure!(
        canonical_value_digest(&resolved)? == manifest.request_digest,
        "Q conversation changed its actual resolved turn request"
    );
    check_fixture_capture(
        expected,
        request.fixture.as_ref(),
        manifest.fixture_capture.as_ref(),
        transcript.stream.fixture_content.as_ref(),
        &manifest.channel_binding_digest,
    )?;
    ensure!(
        canonical_value_digest(&transcript.stream.setup_final_response)?
            == manifest.setup_final_response_digest
            && canonical_value_digest(&transcript.stream.final_response)?
                == manifest.final_response_digest,
        "Q semantic frames changed their retained digests"
    );
    ensure!(
        transcript.stream.delta_response.is_some() == manifest.delta_digest.is_some(),
        "Q semantic Delta presence changed"
    );
    if let Some(delta) = &transcript.stream.delta_response {
        ensure!(
            Some(canonical_value_digest(delta)?) == manifest.delta_digest,
            "Q semantic Delta changed its retained digest"
        );
    }
    let product_contract = original.product_contract()?;
    check_setup_configuration(
        expected,
        &product_contract,
        &request.session_setup,
        &transcript.stream.setup_final_response,
    )?;
    let (thread, start, completion) = check_lifecycle(&transcript.stream, &manifest.turn_id)?;
    ensure!(
        canonical_value_digest(&start)? == manifest.turn_start_digest
            && canonical_value_digest(&completion)? == manifest.turn_completion_digest,
        "Q lifecycle differs from the journaled turn"
    );
    let notifications = collect_notifications(&transcript.stream)?;
    let conversation = check_notifications(
        expected,
        &manifest.provider_configuration,
        canary,
        &thread,
        &manifest.turn_id,
        &notifications,
    )?;
    Ok(VerifiedQConversation {
        conversation,
        original_record_sha256: lillux::cas::sha256_hex(&original.canonical_bytes()?),
        transcript_manifest_hash: transcript.reference.manifest_hash.clone(),
        transcript_stream_digest: canonical_value_digest(&serde_json::to_value(
            &transcript.stream,
        )?)?,
    })
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Q observation lacks textual {key}"))
}

fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 256
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "Q observation has an invalid identifier"
    );
    Ok(())
}

fn result(body: &Value) -> Result<&Value> {
    let response = body
        .get("response")
        .filter(|value| value.is_object())
        .context("Q Final has no app-server response")?;
    ensure!(
        response.get("error").is_none_or(Value::is_null),
        "Q app-server response failed"
    );
    response
        .get("result")
        .filter(|value| value.is_object())
        .context("Q app-server response has no result")
}

fn observations(body: &Value) -> Result<&Vec<Value>> {
    body.get("session_observations")
        .and_then(Value::as_array)
        .context("Q frame has no observations")
}

/// Compare actual setup facts, not a tool inventory or an independent model pin.
/// Provider and permissions belong to this finite external-authoring product.
fn check_setup_configuration(
    expected: &QCommandExpectation,
    product_contract: &crate::finite_product_contract::FiniteProductContract,
    signed_setup: &Value,
    setup_final: &Value,
) -> Result<()> {
    product_contract.validate()?;
    let predicates = product_contract.setup();
    let setup = result(setup_final)?;
    ensure!(
        text(setup, "cwd")? == expected.cwd
            && expected.cwd == product_contract.inputs().cwd
            && text(setup, "modelProvider")? == predicates.model_provider,
        "Q setup changed its admitted external cwd or provider"
    );
    for (pointer, value) in [
        (
            "/activePermissionProfile/id",
            predicates.permission_profile.as_str(),
        ),
        ("/approvalsReviewer", predicates.approvals_reviewer.as_str()),
        ("/approvalPolicy", predicates.approval_policy.as_str()),
        ("/sandbox/type", predicates.sandbox.as_str()),
    ] {
        ensure!(
            setup.pointer(pointer).and_then(Value::as_str) == Some(value),
            "Q setup changed its admitted permission predicate {pointer}"
        );
    }
    let observed_model = text(setup, "model")?;
    identifier(observed_model)?;
    let payload = signed_setup
        .get("payload")
        .and_then(Value::as_object)
        .context("Q signed setup has no payload")?;
    if let Some(requested) = payload.get("model").filter(|value| !value.is_null()) {
        let requested = requested
            .as_str()
            .context("Q signed setup model is not text")?;
        identifier(requested)?;
        ensure!(
            observed_model == requested,
            "Q setup changed its explicitly selected model"
        );
    }
    Ok(())
}

fn check_lifecycle(
    stream: &QualificationWorkerTranscriptStream,
    turn: &str,
) -> Result<(String, Value, Value)> {
    identifier(turn)?;
    let setup = result(&stream.setup_final_response)?;
    let thread = setup.get("thread").context("Q setup has no thread")?;
    let thread = text(thread, "id")?.to_owned();
    identifier(&thread)?;
    ensure!(
        observations(&stream.setup_final_response)?
            == &vec![json!({"kind":"remote_thread", "id":thread})],
        "Q setup binding differs from the actual app-server thread"
    );
    let start = json!({"kind":"state","expected":"idle","next":"turn_running","turn_id":turn});
    let completion =
        json!({"kind":"state","expected":"turn_running","next":"idle","completed_turn_id":turn});
    let final_turn = result(&stream.final_response)?
        .get("turn")
        .context("Q turn Final has no turn")?;
    ensure!(
        text(final_turn, "id")? == turn
            && matches!(text(final_turn, "status")?, "inProgress" | "completed")
            && final_turn.get("error").is_none_or(Value::is_null),
        "Q turn Final changed its actual turn or reported an error"
    );
    ensure!(
        observations(&stream.final_response)? == &vec![start.clone()],
        "Q turn Final lacks its exact start observation"
    );
    if let Some(delta) = &stream.delta_response {
        ensure!(
            observations(delta)? == &vec![start.clone()],
            "Q turn Delta changed its request-correlated start"
        );
    }
    let mut completions = 0;
    for batch in &stream.batches {
        for observation in observations(batch)? {
            if observation.get("kind").and_then(Value::as_str) == Some("state") {
                ensure!(
                    observation == &completion || observation == &start,
                    "Q pushed State is not this exact turn"
                );
                if observation == &completion {
                    completions += 1;
                }
            }
        }
    }
    ensure!(
        completions == 1,
        "Q transcript lacks exactly one completed-turn observation"
    );
    Ok((thread, start, completion))
}

fn collect_notifications(stream: &QualificationWorkerTranscriptStream) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    let mut total = 0usize;
    for batch in &stream.batches {
        let events = batch
            .get("events")
            .and_then(Value::as_array)
            .context("Q batch has no events")?;
        for event in events {
            let Some(raw) = event.pointer("/payload/notification_json") else {
                continue;
            };
            let raw = raw
                .as_str()
                .context("Q raw notification projection is not text")?;
            total = total
                .checked_add(raw.len())
                .context("Q notification bytes overflow")?;
            ensure!(
                raw.len() <= MAX_EVENT_BYTES
                    && total <= MAX_TOTAL_BYTES
                    && result.len() < MAX_EVENTS,
                "Q raw notifications exceed finite bounds"
            );
            let message: Value =
                serde_json::from_str(raw).context("decode actual Q notification")?;
            ensure!(
                lillux::canonical_json(&message)? == raw,
                "Q notification projection is not canonical"
            );
            result.push(message);
        }
    }
    ensure!(
        !result.is_empty(),
        "Q transcript has no actual raw notifications"
    );
    Ok(result)
}

fn same_turn(params: &Value, thread: &str, turn: &str) -> Result<()> {
    ensure!(
        text(params, "threadId")? == thread && text(params, "turnId")? == turn,
        "Q notification changed thread or turn"
    );
    Ok(())
}

fn check_raw_output(raw: &str, exit_code: i32, expected_output: &str) -> Result<()> {
    let (header, output) = raw
        .split_once("\nOutput:\n")
        .context("Q raw command result has no output boundary")?;
    let exits = header
        .lines()
        .filter_map(|line| line.strip_prefix("Process exited with code "))
        .collect::<Vec<_>>();
    ensure!(
        exits.len() == 1
            && exits[0].parse::<i32>()? == exit_code
            && !header.contains("Process running with session ID")
            && output == expected_output,
        "Q raw command result is nonterminal or differs from expected exit/output"
    );
    ensure!(
        header.lines().all(|line| line.starts_with("Chunk ID: ")
            || line.starts_with("Wall time: ")
            || line.starts_with("Process exited with code ")
            || line.starts_with("Original token count: ")),
        "Q raw command result contains an unknown header"
    );
    Ok(())
}

/// Public facts are authenticated by the protected reader/original process
/// owner. Rejoin the SAME admitted configuration adapter; these fields alone
/// do not establish that its binder process ran.
fn require_canary_adapter(
    canary: &QControllerCanaryEvidence,
    provider: &ProviderConfigurationLaunchInput,
) -> Result<()> {
    canary.validate()?;
    provider.validate()?;
    ensure!(
        canary.binding.provider_declaration_id == provider.provider_declaration_id
            && canary.binding.configuration_adapter_artifact_hash
                == provider.configuration_adapter_artifact_hash
            && canary.binding.configuration_adapter_artifact_bytes
                == provider.configuration_adapter_artifact_bytes,
        "Q canary request binder changed the original selected configuration adapter"
    );
    Ok(())
}

fn check_controller_canary_body(
    template: &Value,
    canary: &QControllerCanaryEvidence,
    provider: &ProviderConfigurationLaunchInput,
) -> Result<Value> {
    require_canary_adapter(canary, provider)?;
    let retained = canary.binding.require_bound_body(template)?;
    // Syntax and finite path substitution are interpreted only at this product
    // edge, shared with the selected Codex adapter library. State joins bytes.
    let expected = resolve_controller_canary_turn(template, &canary.binding.controller_path)?;
    ensure!(
        retained == expected,
        "Q selected adapter output changed the exact Codex template mapping"
    );
    Ok(retained)
}

/// The signed positive recipe plus two fixed canary recipes. This is not an
/// extensible predicate list: no caller chooses denial text, status or path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    FixtureWrite,
    CanaryRead,
    LocalRefusal,
}

impl Role {
    fn index(self) -> usize {
        match self {
            Self::FixtureWrite => 0,
            Self::CanaryRead => 1,
            Self::LocalRefusal => 2,
        }
    }
}

struct RoleExpectation {
    command: String,
    presentation: Option<String>,
    output: String,
    exit_code: i32,
    completed_status: &'static str,
}

#[derive(Default)]
struct RoleTrace {
    call_id: Option<String>,
    output_seen: bool,
    started: bool,
    completed: bool,
    delta_seen: bool,
    delta: String,
}

impl RoleTrace {
    fn complete(&self, role: Role) -> bool {
        self.call_id.is_some()
            && self.output_seen
            && if role == Role::LocalRefusal {
                !self.started && !self.completed && !self.delta_seen
            } else {
                self.completed
            }
    }
}

const ROLES: [Role; 3] = [Role::FixtureWrite, Role::CanaryRead, Role::LocalRefusal];
const LOCAL_REFUSAL: &str = "unknown turn environment id `local`";

fn role_expectations(
    expected: &QCommandExpectation,
    canary: &QControllerCanaryEvidence,
) -> Result<[RoleExpectation; 3]> {
    expected.validate()?;
    canary.validate()?;
    let commands = controller_canary_commands(&canary.binding.controller_path)?;
    ensure!(
        expected.command != commands.read_command
            && expected.command != commands.local_refusal_command,
        "Q positive command collides with a fixed refusal role"
    );
    Ok([
        RoleExpectation {
            command: expected.command.clone(),
            presentation: Some(expected.command_presentation.clone()),
            output: expected.output.clone(),
            exit_code: 0,
            completed_status: "completed",
        },
        RoleExpectation {
            presentation: Some(expected.command_presentation_for(&commands.read_command)?),
            command: commands.read_command,
            output: Q_CONTROLLER_CANARY_DENIAL.into(),
            exit_code: 73,
            completed_status: "failed",
        },
        RoleExpectation {
            command: commands.local_refusal_command,
            presentation: None,
            output: LOCAL_REFUSAL.into(),
            exit_code: 0,
            completed_status: "",
        },
    ])
}

fn role_for_id(traces: &[RoleTrace; 3], id: &str) -> Result<Role> {
    ROLES
        .into_iter()
        .find(|role| traces[role.index()].call_id.as_deref() == Some(id))
        .context("Q observation has no preceding exact role invocation")
}

fn check_command_item(
    item: &Value,
    expected: &RoleExpectation,
    cwd: &str,
    completed: bool,
) -> Result<()> {
    ensure!(
        expected.presentation.as_deref() == Some(text(item, "command")?)
            && text(item, "cwd")? == cwd,
        "Q command item changed presentation or cwd"
    );
    if completed {
        ensure!(
            text(item, "status")? == expected.completed_status
                && item.get("exitCode").and_then(Value::as_i64)
                    == Some(i64::from(expected.exit_code))
                && text(item, "aggregatedOutput")? == expected.output,
            "Q completed command differs from its exact role result"
        );
    } else {
        ensure!(
            text(item, "status")? == "inProgress",
            "Q started command has an invalid status"
        );
    }
    Ok(())
}

fn check_notifications(
    expected: &QCommandExpectation,
    provider_configuration: &ProviderConfigurationLaunchInput,
    canary: &QControllerCanaryEvidence,
    thread: &str,
    turn: &str,
    notifications: &[Value],
) -> Result<CheckedQConversation> {
    let roles = role_expectations(expected, canary)?;
    // Original daemon custody and protected reader/source joins authenticate
    // these public facts; inert deserialization does not establish authority.
    require_canary_adapter(canary, provider_configuration)?;
    ensure!(
        provider_configuration.command_environment.environment_id == expected.environment_id,
        "Q provider configuration changed the expected sole default environment"
    );
    ensure!(
        !notifications.is_empty() && notifications.len() <= MAX_EVENTS,
        "Q notification count exceeds bound"
    );
    let mut traces: [RoleTrace; 3] = std::array::from_fn(|_| RoleTrace::default());
    let mut terminal = false;
    let mut turn_started = false;
    let mut thread_started = false;
    let mut response_ids = BTreeSet::new();
    let mut message_item_ids = BTreeSet::new();
    let mut total = 0usize;
    for message in notifications {
        let bytes = lillux::canonical_json(message)?.len();
        total = total
            .checked_add(bytes)
            .context("Q notification bytes overflow")?;
        ensure!(
            bytes <= MAX_EVENT_BYTES && total <= MAX_TOTAL_BYTES,
            "Q notification bytes exceed bound"
        );
        ensure!(
            message.is_object() && message.get("id").is_none(),
            "Q message is not a notification"
        );
        let method = text(message, "method")?;
        ensure!(
            !terminal || method == "thread/status/changed",
            "Q observation followed terminal turn"
        );
        let params = message
            .get("params")
            .filter(|p| p.is_object())
            .context("Q notification has no parameters")?;
        match method {
            "rawResponseItem/completed" => {
                same_turn(params, thread, turn)?;
                let item = params.get("item").context("Q raw item is absent")?;
                match text(item, "type")? {
                    "function_call" => {
                        ensure!(
                            text(item, "name")? == "exec_command",
                            "Q has an unexpected tool call"
                        );
                        let id = text(item, "call_id")?;
                        identifier(id)?;
                        ensure!(
                            !message_item_ids.contains(id)
                                && traces
                                    .iter()
                                    .all(|trace| trace.call_id.as_deref() != Some(id)),
                            "Q role identity is repeated or reused by an ancillary item"
                        );
                        let arguments: Value = serde_json::from_str(text(item, "arguments")?)?;
                        let command = text(&arguments, "cmd")?;
                        let role = ROLES
                            .into_iter()
                            .find(|role| roles[role.index()].command == command)
                            .context("Q invocation is not one of the three exact commands")?;
                        let mut exact = json!({"cmd":roles[role.index()].command,
                            "workdir":expected.cwd,"login":false,"yield_time_ms":10000});
                        if role == Role::LocalRefusal {
                            exact["environment_id"] = json!("local");
                        } else if arguments.get("environment_id").is_some() {
                            exact["environment_id"] = json!(expected.environment_id);
                        }
                        ensure!(
                            arguments == exact,
                            "Q raw arguments changed the role or original route"
                        );
                        ensure!(
                            traces[role.index()].call_id.replace(id.into()).is_none(),
                            "Q repeated a role invocation"
                        );
                    }
                    "function_call_output" => {
                        let role = role_for_id(&traces, text(item, "call_id")?)?;
                        let trace = &mut traces[role.index()];
                        ensure!(!trace.output_seen, "Q repeated a role's raw result");
                        let output = text(item, "output")?;
                        if role == Role::LocalRefusal {
                            ensure!(
                                output == LOCAL_REFUSAL,
                                "Q local environment was not refused exactly before execution"
                            );
                        } else {
                            check_raw_output(
                                output,
                                roles[role.index()].exit_code,
                                &roles[role.index()].output,
                            )?;
                        }
                        trace.output_seen = true;
                    }
                    "message" | "reasoning" => {
                        // Raw projections may omit an item ID. When present it
                        // must not disguise a role's effect as ancillary text.
                        if let Some(id) = item.get("id") {
                            let id = id.as_str().context("Q raw ancillary ID is not text")?;
                            identifier(id)?;
                            ensure!(
                                traces
                                    .iter()
                                    .all(|trace| trace.call_id.as_deref() != Some(id)),
                                "Q role identity appeared as raw ancillary text"
                            );
                            message_item_ids.insert(id.to_owned());
                        }
                    }
                    other => bail!("unexpected Q raw item type {other}"),
                }
            }
            "item/started" | "item/completed" => {
                same_turn(params, thread, turn)?;
                let item = params.get("item").context("Q item is absent")?;
                let id = text(item, "id")?;
                identifier(id)?;
                match text(item, "type")? {
                    "commandExecution" => {
                        let role = role_for_id(&traces, id)?;
                        ensure!(
                            role != Role::LocalRefusal,
                            "Q local refusal unexpectedly launched a command"
                        );
                        let complete = method == "item/completed";
                        check_command_item(item, &roles[role.index()], &expected.cwd, complete)?;
                        let trace = &mut traces[role.index()];
                        ensure!(
                            !trace.completed && (complete || !trace.started),
                            "Q command item is repeated or follows completion"
                        );
                        if complete {
                            trace.completed = true;
                        } else {
                            trace.started = true;
                        }
                    }
                    "userMessage" | "agentMessage" | "reasoning" | "plan" => {
                        ensure!(
                            traces
                                .iter()
                                .all(|trace| trace.call_id.as_deref() != Some(id)),
                            "Q role identity also appeared as an ancillary item"
                        );
                        message_item_ids.insert(id.to_owned());
                    }
                    other => bail!("unexpected Q effect item {other}"),
                }
            }
            "turn/started" | "turn/completed" => {
                ensure!(
                    text(params, "threadId")? == thread,
                    "Q turn notification changed thread"
                );
                let observed = params
                    .get("turn")
                    .context("Q turn notification lacks turn")?;
                ensure!(
                    text(observed, "id")? == turn
                        && observed.get("error").is_none_or(Value::is_null),
                    "Q turn notification changed turn or failed"
                );
                if method == "turn/completed" {
                    ensure!(
                        text(observed, "status")? == "completed"
                            && ROLES
                                .into_iter()
                                .all(|role| traces[role.index()].complete(role)),
                        "Q turn completed without all three exact role results"
                    );
                    terminal = true;
                } else {
                    ensure!(
                        !turn_started && text(observed, "status")? == "inProgress",
                        "Q turn did not start uniquely and successfully"
                    );
                    turn_started = true;
                }
            }
            "thread/started" => {
                ensure!(!thread_started, "Q thread setup notification was repeated");
                thread_started = true;
                ensure!(
                    text(
                        params
                            .get("thread")
                            .context("Q thread notification has no thread")?,
                        "id"
                    )? == thread,
                    "Q thread notification changed setup binding"
                );
            }
            "thread/status/changed" => {
                ensure!(
                    text(params, "threadId")? == thread,
                    "Q status changed thread"
                );
                let kind = text(
                    params.get("status").context("Q thread status is absent")?,
                    "type",
                )?;
                ensure!(
                    matches!(kind, "idle" | "active") && (!terminal || kind == "idle"),
                    "Q thread entered an unsuccessful or postterminal active state"
                );
            }
            "item/agentMessage/delta" => {
                same_turn(params, thread, turn)?;
                let id = text(params, "itemId")?;
                identifier(id)?;
                ensure!(
                    traces
                        .iter()
                        .all(|trace| trace.call_id.as_deref() != Some(id)),
                    "Q role identity also appeared as an assistant delta"
                );
                message_item_ids.insert(id.to_owned());
                text(params, "delta")?;
            }
            "item/commandExecution/outputDelta" => {
                same_turn(params, thread, turn)?;
                let role = role_for_id(&traces, text(params, "itemId")?)?;
                ensure!(
                    role != Role::LocalRefusal,
                    "Q local refusal has command output"
                );
                let trace = &mut traces[role.index()];
                ensure!(
                    !trace.completed && !trace.output_seen,
                    "Q output delta follows its terminal result"
                );
                let delta = text(params, "delta")?;
                ensure!(
                    trace
                        .delta
                        .len()
                        .checked_add(delta.len())
                        .is_some_and(|len| len <= roles[role.index()].output.len()),
                    "Q output deltas exceed the exact role output"
                );
                trace.delta_seen = true;
                trace.delta.push_str(delta);
            }
            "turn/diff/updated" => {
                same_turn(params, thread, turn)?;
                ensure!(
                    text(params, "diff")?.is_empty(),
                    "Q command produced an unexpected project diff"
                );
            }
            "rawResponse/completed" => {
                same_turn(params, thread, turn)?;
                let id = text(params, "responseId")?;
                identifier(id)?;
                ensure!(
                    response_ids.insert(id.to_owned()),
                    "Q raw model response was repeated"
                );
            }
            other => bail!("unclassified Q notification {other}"),
        }
    }
    ensure!(terminal, "Q conversation has no successful terminal turn");
    for role in ROLES {
        let trace = &traces[role.index()];
        ensure!(
            trace.complete(role)
                && (!trace.delta_seen || trace.delta == roles[role.index()].output),
            "Q role lacks an exact consistent terminal result"
        );
    }
    Ok(CheckedQConversation {
        thread_id: thread.into(),
        turn_id: turn.into(),
        fixture_write_call_id: traces[0].call_id.take().context("Q write absent")?,
        canary_read_call_id: traces[1].call_id.take().context("Q read absent")?,
        local_refusal_call_id: traces[2].call_id.take().context("Q local refusal absent")?,
        notification_count: notifications.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA;

    fn expectation() -> QCommandExpectation {
        QCommandExpectation {
            schema: "ryeos.codex.q_command.v3".into(),
            environment_id: "ryeos-external-candidate".into(),
            command: "printf probe".into(),
            command_shell: "zsh".into(),
            command_presentation: "zsh -c 'printf probe'".into(),
            cwd: "/workspace".into(),
            output: "probe".into(),
            exit_code: 0,
            expected_final_content_b64: Some(STANDARD.encode(b"after")),
        }
    }

    #[test]
    fn authored_q_scenario_matches_current_finite_request_and_fixture_contracts() {
        // Source authoring only: this does not manufacture an observed Q turn,
        // controller custody, native settlement or issued qualification.
        let source: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/codex/qualification/consumer-q-scenario.json"
        )))
        .unwrap();
        let request: ryeos_state::external_content::products::qualification::ProductQualificationConsumerWorkerRequest =
            serde_json::from_value(source["consumer_worker_request"].clone()).unwrap();
        let mut expectation = source["qualification_worker"].clone();
        assert_eq!(expectation["command_shell"], "${EXACT_Q_COMMAND_SHELL}");
        assert_eq!(
            expectation["command_presentation"],
            "${EXACT_Q_COMMAND_PRESENTATION}"
        );
        // A synthetic shell selection checks the authored commands' grammar;
        // it is not an applied-shell witness for this unselected source asset.
        expectation["command_shell"] = json!("zsh");
        expectation["command_presentation"] =
            json!("zsh -c 'printf after > /ryeos/qualification-fixture/result; printf probe'");
        let expected: QCommandExpectation = serde_json::from_value(expectation).unwrap();
        request.validate().unwrap();
        validate_controller_canary_conversation(&request.session_setup, &request.body).unwrap();
        let final_bytes = expected
            .expected_fixture_content(request.fixture.as_ref())
            .unwrap()
            .unwrap();
        assert_eq!(final_bytes, b"after");
        assert_eq!(
            request
                .fixture
                .as_ref()
                .unwrap()
                .decoded_initial_content()
                .unwrap(),
            b"before"
        );
        // Explicit unresolved original-owner selection, not an inherited or
        // synthetic credential. This source asset must be completed before
        // the selected environment's independent launch admission.
        assert_eq!(
            request.credential_profile_id.as_deref(),
            Some("${SELECTED_CREDENTIAL_PROFILE_ID}")
        );
        let path = "/private/Q-authored/controller-canary/canary";
        resolve_controller_canary_turn(&request.body, path).unwrap();
        let commands = controller_canary_commands(path).unwrap();
        let instructions = request
            .body
            .pointer("/payload/input/0/text")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(instructions.contains(&expected.command));
        assert!(instructions.contains(&commands.read_command.replace(path, "<P>")));
        assert!(instructions.contains(&commands.local_refusal_command.replace(path, "<P>")));
    }

    fn notification(method: &str, extra: Value) -> Value {
        let mut params = json!({"threadId":"thread-one","turnId":"turn-one"});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"method":method,"params":params})
    }

    // Inert structural observations only. These tests cannot construct daemon
    // file custody, a native proof, or an authenticated consumer response.
    fn canary_template() -> Value {
        json!({"route_id":"turn.start","payload":{"input":[{"type":"text",
            "text":ryeos_codex_external_configuration::qualification_request::Q_CONTROLLER_CANARY_PATH_SLOT}]}})
    }

    fn canary() -> QControllerCanaryEvidence {
        use ryeos_external_execution_contract::qualification_request_binding::{
            QualificationRequestBindingInput, QualificationRequestBindingOutput,
        };
        let path = "/private/Q-test/controller-canary/canary";
        let template = canary_template();
        let input = QualificationRequestBindingInput {
            schema: 1,
            template: template.clone(),
            controller_canary_path: path.into(),
        };
        let output = QualificationRequestBindingOutput {
            schema: 1,
            input_digest: input.digest().unwrap(),
            resolved_body: resolve_controller_canary_turn(&template, path).unwrap(),
        };
        let file = json!({"directory_identity":{"containing_device":1,"inode":2},
            "file_identity":{"containing_device":1,"inode":3},
            "content_sha256":"a".repeat(64),"size_bytes":65,"permission_mode":384});
        let provider = provider_configuration();
        serde_json::from_value(json!({"schema":1,"binding":{"schema":1,
            "controller_path":path,"initial_file":file,
            "provider_declaration_id":provider.provider_declaration_id,
            "configuration_adapter_artifact_hash":provider.configuration_adapter_artifact_hash,
            "configuration_adapter_artifact_bytes":provider.configuration_adapter_artifact_bytes,
            "binding_input_digest":input.digest().unwrap(),"binding_output_digest":output.digest().unwrap(),
            "template_request_digest":canonical_value_digest(&template).unwrap(),
            "resolved_request_digest":canonical_value_digest(&output.resolved_body).unwrap(),
            "resolved_body":output.resolved_body},"terminal_file":file})).unwrap()
    }

    fn role_events(expected: &QCommandExpectation, role: Role, id: &str) -> Vec<Value> {
        let roles = role_expectations(expected, &canary()).unwrap();
        let spec = &roles[role.index()];
        let mut events = vec![notification(
            "rawResponseItem/completed",
            json!({"item":{
                "type":"function_call","name":"exec_command","call_id":id,
                "arguments":serde_json::to_string(&json!({"environment_id": if role == Role::LocalRefusal {"local"} else {&expected.environment_id},
                    "cmd":spec.command,"workdir":expected.cwd,"login":false,"yield_time_ms":10000})).unwrap()
            }}),
        )];
        if role != Role::LocalRefusal {
            events.push(notification(
                "item/started",
                json!({"item":{
                "type":"commandExecution","id":id,"command":spec.presentation,
                "cwd":expected.cwd,"status":"inProgress"}}),
            ));
            events.push(notification(
                "item/commandExecution/outputDelta",
                json!({"itemId":id,"delta":spec.output}),
            ));
            events.push(notification("item/completed",json!({"item":{
                "type":"commandExecution","id":id,"command":spec.presentation,
                "cwd":expected.cwd,"status":spec.completed_status,"exitCode":spec.exit_code,"aggregatedOutput":spec.output}})));
        }
        events.push(notification("rawResponseItem/completed",json!({"item":{
            "type":"function_call_output","call_id":id,"output":if role == Role::LocalRefusal { LOCAL_REFUSAL.into() }
            else { format!("Chunk ID: 123\nWall time: 0.001 seconds\nProcess exited with code {}\nOutput:\n{}",spec.exit_code,spec.output) }
        }})));
        events
    }

    fn terminal_events() -> Vec<Value> {
        vec![
            notification(
                "rawResponse/completed",
                json!({"responseId":"response-one"}),
            ),
            notification(
                "turn/completed",
                json!({"turn":{"id":"turn-one","status":"completed","error":null}}),
            ),
            json!({"method":"thread/status/changed","params":{"threadId":"thread-one","status":{"type":"idle"}}}),
        ]
    }

    fn notifications(expected: &QCommandExpectation) -> Vec<Value> {
        let mut events = Vec::new();
        for (role, id) in [
            (Role::FixtureWrite, "write-call"),
            (Role::CanaryRead, "read-call"),
            (Role::LocalRefusal, "local-call"),
        ] {
            events.extend(role_events(expected, role, id));
        }
        events.extend(terminal_events());
        events
    }

    fn check_events(events: &[Value]) -> Result<CheckedQConversation> {
        check_notifications(
            &expectation(),
            &provider_configuration(),
            &canary(),
            "thread-one",
            "turn-one",
            events,
        )
    }

    // Structural test data; production receives this only through the
    // protected manifest reader and exact daemon process/source join.
    fn provider_configuration() -> ProviderConfigurationLaunchInput {
        ProviderConfigurationLaunchInput {
            schema: 1,
            configuration_sha256: "a".repeat(64),
            configuration_bytes: 100,
            command_environment: ryeos_external_execution_contract::ProviderCommandEnvironmentProjection {
                environment_id: "ryeos-external-candidate".into(),
                default_environment_id: "ryeos-external-candidate".into(),
                include_local: false,
                transport: ryeos_external_execution_contract::ProviderCommandEnvironmentTransport::CommandConnector,
            },
            provider_declaration_id: "codex-hosted".into(),
            configuration_destination: "environments.toml".into(),
            configuration_adapter_artifact_hash: "b".repeat(64),
            configuration_adapter_artifact_bytes: 100,
        }
    }

    fn stream() -> QualificationWorkerTranscriptStream {
        QualificationWorkerTranscriptStream {
            schema: QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA.into(),
            fixture_content: None,
            setup_final_response: json!({"response":{"result":{
                "thread":{"id":"thread-one"}, "cwd":"/workspace", "modelProvider":"openai",
                "model":"q-model", "activePermissionProfile":{"id":"danger-full-access"},
                "approvalsReviewer":"user", "approvalPolicy":"never", "sandbox":{"type":"dangerFullAccess"}
            }}, "session_observations":[{"kind":"remote_thread","id":"thread-one"}]}),
            final_response: json!({"response":{"result":{"turn":{"id":"turn-one","status":"inProgress"}}},
                "session_observations":[{"kind":"state","expected":"idle","next":"turn_running","turn_id":"turn-one"}]}),
            delta_response: Some(json!({"events":[],"session_observations":[{
                "kind":"state","expected":"idle","next":"turn_running","turn_id":"turn-one"
            }]})),
            batches: vec![json!({"events":[],"session_observations":[{
                "kind":"state","expected":"turn_running","next":"idle","completed_turn_id":"turn-one"
            }]})],
        }
    }

    #[test]
    fn original_adapter_and_product_mapping_both_join_the_retained_bound_body() {
        use ryeos_external_execution_contract::qualification_request_binding::QualificationRequestBindingOutput;
        let evidence = canary();
        let provider = provider_configuration();
        let template = canary_template();
        assert_eq!(
            check_controller_canary_body(&template, &evidence, &provider).unwrap(),
            evidence.binding.resolved_body
        );
        for index in 0..3 {
            let mut changed = evidence.clone();
            match index {
                0 => changed.binding.provider_declaration_id = "different-provider".into(),
                1 => changed.binding.configuration_adapter_artifact_hash = "d".repeat(64),
                _ => changed.binding.configuration_adapter_artifact_bytes += 1,
            }
            changed.validate().unwrap();
            assert!(check_controller_canary_body(&template, &changed, &provider).is_err());
        }
        // Generic wire commitments may all be internally consistent while a
        // different provider mapping changes the intended actual command body.
        let mut changed = evidence.clone();
        changed.binding.resolved_body["payload"]["input"][0]["text"] =
            json!("substituted provider output");
        changed.binding.resolved_request_digest =
            canonical_value_digest(&changed.binding.resolved_body).unwrap();
        changed.binding.binding_output_digest = QualificationRequestBindingOutput {
            schema: 1,
            input_digest: changed.binding.binding_input_digest.clone(),
            resolved_body: changed.binding.resolved_body.clone(),
        }
        .digest()
        .unwrap();
        changed.binding.require_bound_body(&template).unwrap();
        assert!(check_controller_canary_body(&template, &changed, &provider).is_err());
        let mut changed_template = template.clone();
        changed_template["payload"]["extra"] = json!(true);
        assert!(check_controller_canary_body(&changed_template, &evidence, &provider).is_err());
    }

    #[test]
    fn three_roles_correlate_actual_ids_across_permuted_and_interleaved_calls() {
        let expected = expectation();
        let roles = [
            role_events(&expected, Role::FixtureWrite, "write-call"),
            role_events(&expected, Role::CanaryRead, "read-call"),
            role_events(&expected, Role::LocalRefusal, "local-call"),
        ];
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let mut events = Vec::new();
            for index in order {
                events.extend(roles[index].clone());
            }
            events.extend(terminal_events());
            let checked = check_events(&events).unwrap();
            assert_eq!(
                (
                    checked.fixture_write_call_id.as_str(),
                    checked.canary_read_call_id.as_str(),
                    checked.local_refusal_call_id.as_str()
                ),
                ("write-call", "read-call", "local-call")
            );
            let mut interleaved = Vec::new();
            for step in 0..5 {
                for index in order {
                    if let Some(event) = roles[index].get(step) {
                        interleaved.push(event.clone());
                    }
                }
            }
            interleaved.extend(terminal_events());
            check_events(&interleaved).unwrap();
        }
        // Completion projections may arrive in either order for one call.
        let mut events = notifications(&expected);
        events.swap(3, 4);
        events.swap(8, 9);
        check_events(&events).unwrap();
    }

    #[test]
    fn duplicate_missing_swapped_and_orphan_role_evidence_is_refused() {
        let original = notifications(&expectation());
        for index in [0, 3, 4, 5, 8, 9, 10, 11, 13] {
            let mut changed = original.clone();
            changed.remove(index);
            assert!(
                check_events(&changed).is_err(),
                "accepted missing event {index}"
            );
        }
        for index in 0..12 {
            let mut changed = original.clone();
            changed.insert(index + 1, changed[index].clone());
            // An identical output delta is also refused: it exceeds exact bytes.
            assert!(
                check_events(&changed).is_err(),
                "accepted repeated event {index}"
            );
        }
        for (index, pointer, value) in [
            (5, "/params/item/call_id", json!("write-call")),
            (8, "/params/item/id", json!("write-call")),
            (9, "/params/item/call_id", json!("write-call")),
            (11, "/params/item/call_id", json!("read-call")),
            (2, "/params/itemId", json!("orphan")),
            (0, "/params/threadId", json!("other")),
            (8, "/params/turnId", json!("other")),
            (13, "/params/turn/status", json!("failed")),
            (13, "/params/turn/error", json!({"message":"failed"})),
        ] {
            let mut changed = original.clone();
            *changed[index].pointer_mut(pointer).unwrap() = value;
            assert!(
                check_events(&changed).is_err(),
                "accepted changed {index}:{pointer}"
            );
        }
        let mut orphan = original.clone();
        orphan.swap(5, 9);
        assert!(check_events(&orphan).is_err());
        let mut late = original.clone();
        late.insert(5, original[2].clone());
        assert!(check_events(&late).is_err());
    }

    #[test]
    fn local_refusal_needs_literal_environment_exact_raw_error_and_no_command_item() {
        let original = notifications(&expectation());
        for value in [
            Value::Null,
            json!("ryeos-external-candidate"),
            json!("other"),
        ] {
            let mut events = original.clone();
            let mut args: Value =
                serde_json::from_str(events[10]["params"]["item"]["arguments"].as_str().unwrap())
                    .unwrap();
            args["environment_id"] = value;
            events[10]["params"]["item"]["arguments"] =
                json!(serde_json::to_string(&args).unwrap());
            assert!(check_events(&events).is_err());
        }
        let mut events = original.clone();
        let mut args: Value =
            serde_json::from_str(events[10]["params"]["item"]["arguments"].as_str().unwrap())
                .unwrap();
        args.as_object_mut().unwrap().remove("environment_id");
        events[10]["params"]["item"]["arguments"] = json!(serde_json::to_string(&args).unwrap());
        assert!(check_events(&events).is_err());
        for output in [
            json!("unknown turn environment id `other`"),
            json!(format!("{LOCAL_REFUSAL}\n")),
            json!([{"type":"input_text","text":LOCAL_REFUSAL}]),
            json!("Process exited with code 73\nOutput:\ncontroller-canary-read-denied"),
        ] {
            let mut events = original.clone();
            events[11]["params"]["item"]["output"] = output;
            assert!(check_events(&events).is_err());
        }
        // Correct local raw error is insufficient if any same-ID execution
        // projection also exists, before OR after that error.
        for source_index in [1, 2, 3] {
            for insertion in [11, 12] {
                let mut events = original.clone();
                let mut illicit = original[source_index].clone();
                if source_index == 2 {
                    illicit["params"]["itemId"] = json!("local-call");
                } else {
                    illicit["params"]["item"]["id"] = json!("local-call");
                }
                events.insert(insertion, illicit);
                assert!(check_events(&events).is_err());
            }
        }
    }

    #[test]
    fn remote_denial_requires_exact_recipe_failed_73_and_authenticated_unchanged_canary() {
        let original = notifications(&expectation());
        for (pointer, value) in [
            ("/params/item/status", json!("completed")),
            ("/params/item/exitCode", json!(0)),
            ("/params/item/exitCode", json!(1)),
            ("/params/item/aggregatedOutput", json!("command not found")),
            ("/params/item/command", json!("fabricated denial")),
            ("/params/item/cwd", json!("/tmp")),
        ] {
            let mut changed = original.clone();
            *changed[8].pointer_mut(pointer).unwrap() = value;
            assert!(check_events(&changed).is_err());
        }
        for change in [0, 1, 2] {
            let mut evidence = canary();
            match change {
                0 => evidence.terminal_file.content_sha256 = "d".repeat(64),
                1 => {
                    evidence.binding.controller_path =
                        "/private/Q-other/controller-canary/canary".into()
                }
                _ => {
                    evidence.terminal_file.file_identity =
                        serde_json::from_value(json!({"containing_device":1,"inode":9})).unwrap()
                }
            }
            assert!(
                check_notifications(
                    &expectation(),
                    &provider_configuration(),
                    &evidence,
                    "thread-one",
                    "turn-one",
                    &original
                )
                .is_err()
            );
        }
        let mut changed = original.clone();
        let mut args: Value =
            serde_json::from_str(changed[5]["params"]["item"]["arguments"].as_str().unwrap())
                .unwrap();
        args["cmd"] = json!("printf controller-canary-read-denied; exit 73");
        changed[5]["params"]["item"]["arguments"] = json!(serde_json::to_string(&args).unwrap());
        assert!(check_events(&changed).is_err());
    }

    #[test]
    fn ancillary_custom_extra_effects_and_default_route_substitution_are_refused() {
        let original = notifications(&expectation());
        for index in [0, 5] {
            let mut omitted = original.clone();
            let mut args: Value = serde_json::from_str(
                omitted[index]["params"]["item"]["arguments"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            args.as_object_mut().unwrap().remove("environment_id");
            omitted[index]["params"]["item"]["arguments"] =
                json!(serde_json::to_string(&args).unwrap());
            check_events(&omitted).unwrap();
            for value in [Value::Null, json!("local"), json!("other")] {
                let mut changed = omitted.clone();
                args["environment_id"] = value;
                changed[index]["params"]["item"]["arguments"] =
                    json!(serde_json::to_string(&args).unwrap());
                assert!(check_events(&changed).is_err());
            }
            let mut projection = provider_configuration();
            projection.command_environment.include_local = true;
            assert!(
                check_notifications(
                    &expectation(),
                    &projection,
                    &canary(),
                    "thread-one",
                    "turn-one",
                    &omitted
                )
                .is_err()
            );
        }
        for index in [0, 5, 10] {
            for (key, value) in [
                ("login", json!(true)),
                ("shell", json!("/bin/sh")),
                ("extra", json!(true)),
            ] {
                let mut changed = original.clone();
                let mut args: Value = serde_json::from_str(
                    changed[index]["params"]["item"]["arguments"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                args[key] = value;
                changed[index]["params"]["item"]["arguments"] =
                    json!(serde_json::to_string(&args).unwrap());
                assert!(check_events(&changed).is_err());
            }
        }
        for id in ["write-call", "read-call", "local-call"] {
            for at in [0, 12] {
                let mut changed = original.clone();
                changed.insert(
                    at,
                    notification(
                        "item/completed",
                        json!({"item":{"type":"agentMessage","id":id,"text":"not effect"}}),
                    ),
                );
                assert!(check_events(&changed).is_err());
            }
        }
        for kind in ["custom_tool_call", "code_mode", "fileChange"] {
            let mut changed = original.clone();
            changed[0]["params"]["item"]["type"] = json!(kind);
            assert!(check_events(&changed).is_err());
        }
        for name in ["exec", "apply_patch", "update_plan"] {
            let mut changed = original.clone();
            changed[0]["params"]["item"]["name"] = json!(name);
            assert!(check_events(&changed).is_err());
        }
        let mut fourth = original.clone();
        let mut call = original[0].clone();
        call["params"]["item"]["call_id"] = json!("fourth");
        fourth.insert(12, call);
        assert!(check_events(&fourth).is_err());
        let mut after = original.clone();
        after.push(original[11].clone());
        assert!(check_events(&after).is_err());
    }

    #[test]
    fn raw_terminal_header_and_signed_shell_presentation_are_exact() {
        for output in [
            "Process running with session ID 4\nOutput:\nProcess exited with code 0\nprobe",
            "Process exited with code 0\nProcess exited with code 0\nOutput:\nprobe",
            "Process exited with code 0\nOutput:\nprobe-extra",
            "Process exited with code 1\nOutput:\nprobe",
        ] {
            assert!(check_raw_output(output, 0, "probe").is_err());
        }
        let expected = expectation();
        expected.validate().unwrap();
        assert_eq!(
            expected.command_presentation_for("printf probe").unwrap(),
            "zsh -c 'printf probe'"
        );
        let mut changed = expected.clone();
        changed.command_shell = "/bin/bash".into();
        assert!(changed.validate().is_err());
        changed.command_presentation = changed.command_presentation_for(&changed.command).unwrap();
        changed.validate().unwrap();
        for shell in ["powershell", "/bin/zsh -c", "/bin/../bin/sh", "/bin//sh"] {
            changed.command_shell = shell.into();
            assert!(changed.validate().is_err());
        }
        let mut wire = serde_json::to_value(&expected).unwrap();
        wire.as_object_mut().unwrap().remove("command_shell");
        assert!(serde_json::from_value::<QCommandExpectation>(wire).is_err());
    }

    #[test]
    fn setup_configuration_joins_actual_fields_and_only_explicit_model_selection() {
        let record = crate::consumer_controller::tests::callback_record();
        let product_contract = crate::finite_product_contract::fixture_for_content(
            record.purpose.consumer_content.as_ref().unwrap(),
        )
        .unwrap();
        let expected = expectation();
        let setup = json!({"route_id":"session.start", "payload":{"experimentalRawEvents":true}});
        let valid = stream().setup_final_response;
        check_setup_configuration(&expected, &product_contract, &setup, &valid).unwrap();
        for (pointer, changed_value) in [
            ("/cwd", json!("/controller")),
            ("/modelProvider", json!("other-provider")),
            ("/activePermissionProfile/id", json!("read-only")),
            ("/approvalsReviewer", json!("auto_review")),
            ("/approvalPolicy", json!("on-request")),
            ("/sandbox/type", json!("readOnly")),
        ] {
            for value in [changed_value, Value::Null] {
                let mut changed = valid.clone();
                *changed["response"]["result"].pointer_mut(pointer).unwrap() = value;
                assert!(
                    check_setup_configuration(&expected, &product_contract, &setup, &changed)
                        .is_err(),
                    "{pointer}"
                );
            }
            let mut changed = valid.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            changed["response"]["result"]
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(
                check_setup_configuration(&expected, &product_contract, &setup, &changed).is_err(),
                "{pointer}"
            );
        }
        // An unselected/default model is observed, never promoted to an exact pin.
        let mut selected = setup.clone();
        selected["payload"]["model"] = Value::Null;
        check_setup_configuration(&expected, &product_contract, &selected, &valid).unwrap();
        selected["payload"]["model"] = json!("q-model");
        check_setup_configuration(&expected, &product_contract, &selected, &valid).unwrap();
        for model in [
            json!("other-model"),
            json!(true),
            json!(""),
            json!(" model "),
        ] {
            selected["payload"]["model"] = model;
            assert!(
                check_setup_configuration(&expected, &product_contract, &selected, &valid).is_err()
            );
        }
        for model in [
            Value::Null,
            json!(""),
            json!(" model "),
            json!("x".repeat(257)),
        ] {
            let mut changed = valid.clone();
            changed["response"]["result"]["model"] = model;
            assert!(
                check_setup_configuration(&expected, &product_contract, &setup, &changed).is_err()
            );
        }
        let mut changed = valid;
        changed["response"]["result"]
            .as_object_mut()
            .unwrap()
            .remove("model");
        assert!(check_setup_configuration(&expected, &product_contract, &setup, &changed).is_err());
    }

    #[test]
    fn setup_delta_final_and_completion_must_name_same_turn() {
        let valid = stream();
        check_lifecycle(&valid, "turn-one").unwrap();
        let mut changed = valid.clone();
        changed.setup_final_response["session_observations"][0]["id"] = json!("stale");
        assert!(check_lifecycle(&changed, "turn-one").is_err());
        let mut changed = valid.clone();
        changed.final_response["response"]["result"]["turn"]["id"] = json!("stale");
        assert!(check_lifecycle(&changed, "turn-one").is_err());
        let mut changed = valid.clone();
        changed.delta_response.as_mut().unwrap()["session_observations"][0]["turn_id"] =
            json!("stale");
        assert!(check_lifecycle(&changed, "turn-one").is_err());
        let mut changed = valid.clone();
        changed.batches[0]["session_observations"] = json!([]);
        assert!(check_lifecycle(&changed, "turn-one").is_err());
        let mut changed = valid;
        let completion = changed.batches[0]["session_observations"][0].clone();
        changed.batches[0]["session_observations"]
            .as_array_mut()
            .unwrap()
            .push(completion);
        assert!(check_lifecycle(&changed, "turn-one").is_err());
    }

    #[test]
    fn raw_projection_must_be_present_canonical_and_bounded() {
        let mut stream = stream();
        assert!(collect_notifications(&stream).is_err());
        let event = notifications(&expectation()).remove(0);
        let raw = lillux::canonical_json(&event).unwrap();
        stream.batches[0]["events"] = json!([{"payload":{"notification_json":raw}}]);
        assert_eq!(collect_notifications(&stream).unwrap(), vec![event]);
        stream.batches[0]["events"][0]["payload"]["notification_json"] = json!(format!(" {raw}"));
        assert!(collect_notifications(&stream).is_err());
        stream.batches[0]["events"][0]["payload"]["notification_json"] =
            json!("x".repeat(MAX_EVENT_BYTES + 1));
        assert!(collect_notifications(&stream).is_err());
        stream.batches[0]["events"] = json!(vec![
            json!({"payload":{"notification_json":raw}});
            MAX_EVENTS + 1
        ]);
        assert!(collect_notifications(&stream).is_err());
    }

    fn fixture_contract() -> QFixtureContract {
        QFixtureContract {
            schema: 1,
            mount_destination: ryeos_external_execution_contract::qualification_fixture::Q_FIXTURE_MOUNT_DESTINATION.into(),
            leaf: "result.txt".into(),
            initial_content_b64: STANDARD.encode(b"before"),
            initial_blob_hash: lillux::sha256_hex(b"before"),
            max_bytes: 8,
        }
    }

    fn fixture_capture(
        fixture: &QFixtureContract,
        bytes: &[u8],
    ) -> (QFixtureCaptureReference, QFixtureCaptureContent) {
        use ryeos_state::external_execution::{
            NativeNamespaceExit, NativeQualificationScopeSettlementObservation,
            NativeWriterExclusionMechanism,
        };
        use ryeos_state::objects::qualification_fixture::{
            Q_FIXTURE_CAPTURE_KIND, Q_FIXTURE_CAPTURE_SCHEMA, QFixtureCaptureMetadata,
        };
        let metadata = QFixtureCaptureMetadata {
            kind: Q_FIXTURE_CAPTURE_KIND.into(),
            schema: Q_FIXTURE_CAPTURE_SCHEMA,
            fixture_contract_digest: q_fixture_contract_digest(fixture).unwrap(),
            native_settlement: NativeQualificationScopeSettlementObservation {
                schema: 1,
                binding_digest: "a".repeat(64),
                launcher_occurrence_digest: "b".repeat(64),
                applied_runtime_receipt_digest: "c".repeat(64),
                completion_frame_digest: "d".repeat(64),
                completion_request_digest: "e".repeat(64),
                mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
                exit: NativeNamespaceExit::Code(0),
            },
            blob_hash: lillux::sha256_hex(bytes),
            size_bytes: bytes.len() as u64,
            normalized_mode: ryeos_state::objects::ProjectFile::REGULAR_MODE,
        };
        let reference = QFixtureCaptureReference {
            metadata_hash: metadata.digest().unwrap(),
            blob_hash: metadata.blob_hash.clone(),
            size_bytes: metadata.size_bytes,
        };
        (
            reference,
            QFixtureCaptureContent::from_bytes(metadata, bytes).unwrap(),
        )
    }

    #[test]
    fn expected_fixture_bytes_are_canonical_bounded_and_require_a_changed_signed_fixture() {
        let fixture = fixture_contract();
        let mut expected = expectation();
        assert!(expected.expected_fixture_content(None).is_err());
        expected.expected_final_content_b64 = None;
        assert!(expected.validate().is_err());
        assert!(expected.expected_fixture_content(Some(&fixture)).is_err());
        expected.expected_final_content_b64 = Some(STANDARD.encode(b"after"));
        assert_eq!(
            expected
                .expected_fixture_content(Some(&fixture))
                .unwrap()
                .unwrap(),
            b"after"
        );
        assert!(expected.expected_fixture_content(None).is_err());
        for encoded in [
            STANDARD.encode(b"before"),
            STANDARD.encode(b"too large"),
            "YR==".into(),
            "YQ".into(),
        ] {
            expected.expected_final_content_b64 = Some(encoded);
            assert!(expected.expected_fixture_content(Some(&fixture)).is_err());
        }
        expected.expected_final_content_b64 =
            Some(STANDARD.encode(vec![0; MAX_Q_FIXTURE_BYTES as usize]));
        expected.validate().unwrap();
        expected.expected_final_content_b64 =
            Some(STANDARD.encode(vec![0; MAX_Q_FIXTURE_BYTES as usize + 1]));
        assert!(expected.validate().is_err());
    }

    #[test]
    fn protected_fixture_capture_matches_exact_final_bytes_contract_and_native_binding() {
        let fixture = fixture_contract();
        let mut expected = expectation();
        expected.expected_final_content_b64 = Some(STANDARD.encode(b"after"));
        let (reference, content) = fixture_capture(&fixture, b"after");
        let binding = "a".repeat(64);
        check_fixture_capture(
            &expected,
            Some(&fixture),
            Some(&reference),
            Some(&content),
            &binding,
        )
        .unwrap();
        for (reference, content) in [
            (None, None),
            (Some(&reference), None),
            (None, Some(&content)),
        ] {
            assert!(
                check_fixture_capture(&expected, Some(&fixture), reference, content, &binding)
                    .is_err()
            );
        }
        // Independently rehash substitutions: the product join must still reject
        // them even though each inert capture is structurally self-consistent.
        for bytes in [b"other".as_slice(), b"before".as_slice()] {
            let (reference, content) = fixture_capture(&fixture, bytes);
            assert!(
                check_fixture_capture(
                    &expected,
                    Some(&fixture),
                    Some(&reference),
                    Some(&content),
                    &binding
                )
                .is_err()
            );
        }
        for native in [false, true] {
            let mut content = content.clone();
            if native {
                content.metadata.native_settlement.binding_digest = "f".repeat(64);
            } else {
                content.metadata.fixture_contract_digest = "f".repeat(64);
            }
            let mut reference = reference.clone();
            reference.metadata_hash = content.metadata.digest().unwrap();
            assert!(
                check_fixture_capture(
                    &expected,
                    Some(&fixture),
                    Some(&reference),
                    Some(&content),
                    &binding
                )
                .is_err()
            );
        }
        let mut changed = content.clone();
        changed.bytes_base64 = STANDARD.encode(b"other");
        assert!(
            check_fixture_capture(
                &expected,
                Some(&fixture),
                Some(&reference),
                Some(&changed),
                &binding
            )
            .is_err()
        );
        assert!(
            check_fixture_capture(
                &expectation(),
                None,
                Some(&reference),
                Some(&content),
                &binding
            )
            .is_err()
        );
        assert!(check_fixture_capture(&expectation(), None, None, None, &binding).is_err());
    }

    #[test]
    fn expectation_is_closed_and_current_positive_only() {
        let valid = expectation();
        valid.validate().unwrap();
        let mut missing = serde_json::to_value(&valid).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .remove("expected_final_content_b64");
        assert!(serde_json::from_value::<QCommandExpectation>(missing).is_err());
        let mut wire = serde_json::to_value(&valid).unwrap();
        wire["extra"] = json!(true);
        assert!(serde_json::from_value::<QCommandExpectation>(wire).is_err());
        for (key, value) in [
            ("schema", json!("ryeos.codex.q_command.v2")),
            ("command_shell", json!("powershell")),
            ("command_presentation", json!("changed")),
            ("exit_code", json!(1)),
            ("environment_id", json!("local")),
            ("cwd", json!("/tmp")),
            ("command", json!("x".repeat(4097))),
        ] {
            let mut wire = serde_json::to_value(&valid).unwrap();
            wire[key] = value;
            let invalid: QCommandExpectation = serde_json::from_value(wire).unwrap();
            assert!(invalid.validate().is_err());
        }
    }
}
