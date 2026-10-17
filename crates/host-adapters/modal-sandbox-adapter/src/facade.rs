//! Client of the signed Modal lease facade: request bodies, envelope
//! verification and typed observations.
//!
//! The facade answers every request with a JSON envelope naming the digest of
//! the exact facade source that produced it. A reply is evidence only when
//! that digest equals the one the signed provider spec pins; any other reply,
//! including one from Modal's own proxy, is not attested and settles nothing.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::provider_spec::{FacadePin, RouteName};
use ryeos_external_execution_contract::{canonical_json, from_json_slice_strict};

/// Bound on one facade reply the adapter will read. The largest reply is a
/// list of 256 sandbox ids.
pub const MAX_REPLY_BYTES: usize = 256 * 1024;

/// Every refusal code the facade documents. A reply naming another code is
/// not attested.
const KNOWN_REFUSALS: [&str; 67] = [
    "unknown_route",
    "method_not_allowed",
    "content_length_required",
    "body_too_large",
    "body_length_mismatch",
    "request_aborted",
    "too_many_structures",
    "invalid_json",
    "body_not_object",
    "unknown_field",
    "missing_field",
    "image_refused",
    "command_refused",
    "gpu_refused",
    "secrets_refused",
    "volumes_refused",
    "cidr_refused",
    "ports_refused",
    "resources_refused",
    "path_refused",
    "invalid_name",
    "invalid_lease_set",
    "invalid_sandbox_id",
    "ambiguous_selector",
    "timeout_out_of_range",
    "invalid_network",
    "invalid_network_type",
    "empty_allow_list",
    "too_many_rules",
    "invalid_rule",
    "port_refused",
    "plain_http_refused",
    "non_https_rule_refused",
    "wildcard_host_refused",
    "invalid_host",
    "duplicate_host",
    NAME_TAKEN_POLICY_MISMATCH,
    NAME_TAKEN_UNTAGGED,
    NAME_TAKEN_TAG_MISMATCH,
    "invalid_sha256",
    "sha256_mismatch",
    "invalid_base64",
    "content_too_large",
    "stdin_too_large",
    "not_facade_sandbox",
    "not_lease_app_sandbox",
    SANDBOX_TERMINATED,
    "upload_already_claimed",
    "run_once_already_claimed",
    "upload_not_complete",
    "guest_step_failed",
    "invalid_slot",
    "invalid_offset",
    "invalid_total_bytes",
    "chunk_too_large",
    "chunk_empty",
    "chunk_sha256_mismatch",
    "offset_mismatch",
    "chunk_already_claimed",
    "too_many_slots",
    "slot_too_large",
    "slot_sealed",
    "slot_not_started",
    "slot_not_sealed",
    "seal_size_mismatch",
    "seal_digest_mismatch",
    "seal_digest_conflict",
];

pub const NAME_TAKEN_POLICY_MISMATCH: &str = "name_taken_policy_mismatch";
pub const NAME_TAKEN_UNTAGGED: &str = "name_taken_untagged";
pub const NAME_TAKEN_TAG_MISMATCH: &str = "name_taken_tag_mismatch";
pub const SANDBOX_TERMINATED: &str = "sandbox_terminated";
/// The provider failure class the facade reports when a lease ended between
/// its duplicate-name error and the name lookup; the caller retries.
pub const PROVIDER_NOT_FOUND: &str = "NotFoundError";

/// One transport-level reply, before any interpretation.
#[derive(Debug)]
pub struct RawReply {
    pub status: u16,
    pub json_content_type: bool,
    pub body: Vec<u8>,
}

/// Whether the transport can prove nothing reached the facade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportFailure {
    NoRequestSent,
    MayHaveBeenSent,
}

/// One POST to a fixed facade route. The implementation owns the origin, the
/// proxy credentials and the deadlines.
pub trait FacadeTransport {
    fn post(&self, route: RouteName, body: &[u8]) -> Result<RawReply, TransportFailure>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The request was provably never sent.
    NoRequestSent,
    /// The request may have reached the facade and no reply was obtained.
    Ambiguous,
    /// A reply arrived that is not an attested facade envelope.
    Unattested(&'static str),
    /// The envelope names a facade source other than the pinned one.
    PinMismatch,
}

#[derive(Debug)]
pub enum Outcome {
    /// An observation: the full envelope object.
    Observation(Value),
    /// A refusal the facade documents.
    Refusal(&'static str),
    /// A provider failure, by exception class name only.
    ProviderError(String),
}

/// An attested facade reply and the digest of its exact bytes.
#[derive(Debug)]
pub struct Reply {
    pub outcome: Outcome,
    pub body_sha256: String,
}

impl Reply {
    /// The observation as `T`, or `None` when the reply is a refusal, a
    /// provider failure, or an observation of another shape.
    pub fn observation<T: for<'de> Deserialize<'de>>(&self) -> Option<T> {
        match &self.outcome {
            Outcome::Observation(value) => serde_json::from_value(value.clone()).ok(),
            Outcome::Refusal(_) | Outcome::ProviderError(_) => None,
        }
    }

    pub fn refusal(&self) -> Option<&'static str> {
        match &self.outcome {
            Outcome::Refusal(code) => Some(code),
            Outcome::Observation(_) | Outcome::ProviderError(_) => None,
        }
    }

    pub fn provider_error(&self) -> Option<&str> {
        match &self.outcome {
            Outcome::ProviderError(class) => Some(class),
            Outcome::Observation(_) | Outcome::Refusal(_) => None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateObservation {
    pub created: bool,
    #[serde(default)]
    pub name_taken: bool,
    pub name: String,
    pub sandbox_id: String,
    pub state: String,
    pub policy_sha256: String,
    pub policy: Value,
}

#[derive(Debug, Deserialize)]
pub struct GetObservation {
    pub found: bool,
    #[serde(default)]
    pub sandbox_id: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub returncode: Option<i64>,
    #[serde(default)]
    pub tagged: Option<bool>,
    #[serde(default)]
    pub name_verified: Option<bool>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct UploadObservation {
    pub sandbox_id: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
pub struct RunOnceObservation {
    pub sandbox_id: String,
    pub released: bool,
    pub stdin_bytes: u64,
    pub stdin_sha256: String,
}

/// The facade's acknowledgement of one appended chunk.
#[derive(Debug, Deserialize)]
pub struct StageObservation {
    pub sandbox_id: String,
    pub slot: String,
    pub offset: u64,
    pub bytes: u64,
    pub slot_bytes: u64,
    pub sha256: String,
}

/// The facade's acknowledgement of one sealed slot.
#[derive(Debug, Deserialize)]
pub struct SealObservation {
    pub sandbox_id: String,
    pub slot: String,
    pub sealed: bool,
    pub already_sealed: bool,
    pub bytes: u64,
    pub sha256: String,
}

pub struct FacadeClient<'a, T: FacadeTransport> {
    transport: &'a T,
    pin: &'a FacadePin,
}

impl<'a, T: FacadeTransport> FacadeClient<'a, T> {
    pub fn new(transport: &'a T, pin: &'a FacadePin) -> Self {
        Self { transport, pin }
    }

    pub fn pin(&self) -> &FacadePin {
        self.pin
    }

    /// `network` is the facade's exact network object.
    pub fn create(
        &self,
        name: &str,
        lease_set: &str,
        timeout_seconds: u32,
        network: &Value,
    ) -> Result<Reply, CallError> {
        self.call(
            RouteName::Create,
            &json!({
                "name": name,
                "lease_set": lease_set,
                "timeout_seconds": timeout_seconds,
                "network": network,
            }),
        )
    }

    pub fn get_by_name(&self, name: &str) -> Result<Reply, CallError> {
        self.call(RouteName::GetByName, &json!({ "name": name }))
    }

    pub fn get_by_id(&self, sandbox_id: &str) -> Result<Reply, CallError> {
        self.call(RouteName::GetById, &json!({ "sandbox_id": sandbox_id }))
    }

    pub fn terminate_by_id(&self, sandbox_id: &str) -> Result<Reply, CallError> {
        self.call(RouteName::Terminate, &json!({ "sandbox_id": sandbox_id }))
    }

    /// Terminate by lease name, for a lease that never got its tags.
    pub fn terminate_by_name(&self, name: &str) -> Result<Reply, CallError> {
        self.call(RouteName::Terminate, &json!({ "name": name }))
    }

    pub fn upload(&self, sandbox_id: &str, content: &[u8]) -> Result<Reply, CallError> {
        use base64::Engine as _;
        self.call(
            RouteName::Upload,
            &json!({
                "sandbox_id": sandbox_id,
                "content_b64": base64::engine::general_purpose::STANDARD.encode(content),
                "sha256": lillux::sha256_hex(content),
            }),
        )
    }

    /// Append `chunk` to `slot` at `offset`. The facade appends only when
    /// `offset` equals the slot's current size.
    pub fn stage(
        &self,
        sandbox_id: &str,
        slot: &str,
        offset: u64,
        chunk: &[u8],
    ) -> Result<Reply, CallError> {
        use base64::Engine as _;
        self.call(
            RouteName::Stage,
            &json!({
                "sandbox_id": sandbox_id,
                "slot": slot,
                "offset": offset,
                "content_b64": base64::engine::general_purpose::STANDARD.encode(chunk),
                "chunk_sha256": lillux::sha256_hex(chunk),
            }),
        )
    }

    /// Seal `slot`: the facade digests the staged file and refuses a size or
    /// digest other than these.
    pub fn stage_seal(
        &self,
        sandbox_id: &str,
        slot: &str,
        total_bytes: u64,
        sha256: &str,
    ) -> Result<Reply, CallError> {
        self.call(
            RouteName::StageSeal,
            &json!({
                "sandbox_id": sandbox_id,
                "slot": slot,
                "total_bytes": total_bytes,
                "sha256": sha256,
            }),
        )
    }

    pub fn run_once(&self, sandbox_id: &str, stdin: &[u8]) -> Result<Reply, CallError> {
        use base64::Engine as _;
        self.call(
            RouteName::RunOnce,
            &json!({
                "sandbox_id": sandbox_id,
                "stdin_b64": base64::engine::general_purpose::STANDARD.encode(stdin),
            }),
        )
    }

    fn call(&self, route: RouteName, body: &Value) -> Result<Reply, CallError> {
        let body = canonical_json(body).map_err(|_| CallError::NoRequestSent)?;
        let raw = self.transport.post(route, &body).map_err(|failure| match failure {
            TransportFailure::NoRequestSent => CallError::NoRequestSent,
            TransportFailure::MayHaveBeenSent => CallError::Ambiguous,
        })?;
        self.interpret(route, raw)
    }

    fn interpret(&self, route: RouteName, raw: RawReply) -> Result<Reply, CallError> {
        if !raw.json_content_type {
            return Err(CallError::Unattested("reply is not JSON"));
        }
        if raw.body.len() > MAX_REPLY_BYTES {
            return Err(CallError::Unattested("reply exceeds its byte bound"));
        }
        let value: Value = from_json_slice_strict(&raw.body, MAX_REPLY_BYTES)
            .map_err(|_| CallError::Unattested("reply is not strict JSON"))?;
        let Some(object) = value.as_object() else {
            return Err(CallError::Unattested("reply is not a JSON object"));
        };
        let text = |key: &str| object.get(key).and_then(Value::as_str);
        if text("schema") != Some(self.pin.response_schema.as_str()) {
            return Err(CallError::Unattested("reply is not a facade envelope"));
        }
        if text("facade_sha256") != Some(self.pin.sha256.as_str()) {
            return Err(CallError::PinMismatch);
        }
        if text("facade_app") != Some(self.pin.app.as_str())
            || text("operation") != Some(route.operation())
        {
            return Err(CallError::Unattested("envelope names another app or operation"));
        }
        let ok = object
            .get("ok")
            .and_then(Value::as_bool)
            .ok_or(CallError::Unattested("envelope lacks ok"))?;
        let body_sha256 = lillux::sha256_hex(&raw.body);
        let outcome = match (ok, raw.status) {
            (true, 200) => Outcome::Observation(value),
            (false, 400..=499) => {
                let code = text("refusal")
                    .and_then(|code| KNOWN_REFUSALS.iter().copied().find(|known| *known == code))
                    .ok_or(CallError::Unattested("unrecognised refusal"))?;
                Outcome::Refusal(code)
            }
            (false, 502) => {
                let class = text("provider_error")
                    .filter(|class| {
                        (1..=64).contains(&class.len())
                            && class
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    })
                    .ok_or(CallError::Unattested("provider failure without a class"))?;
                Outcome::ProviderError(class.to_owned())
            }
            _ => return Err(CallError::Unattested("status contradicts the envelope")),
        };
        Ok(Reply {
            outcome,
            body_sha256,
        })
    }
}

/// The SHA-256 of `value` serialized as compact JSON with every object's keys
/// sorted, the form the facade digests its policy record in.
pub fn sorted_compact_sha256(value: &Value) -> String {
    let mut text = String::new();
    write_sorted(value, &mut text);
    lillux::sha256_hex(text.as_bytes())
}

fn write_sorted(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_sorted(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_sorted(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// The policy record the facade builds for a lease, from its pinned fixed
/// inputs and this lease's set, lifetime and network.
pub fn expected_policy_record(
    pin: &FacadePin,
    lease_set: &str,
    timeout_seconds: u32,
    block_network: bool,
    hosts: &[&str],
) -> Value {
    let mut sorted: Vec<&str> = hosts.to_vec();
    sorted.sort_unstable();
    json!({
        "schema": pin.policy.schema,
        "lease_set": lease_set,
        "timeout_seconds": timeout_seconds,
        "cpu": pin.policy.cpu,
        "memory_mib": pin.policy.memory_mib,
        "block_network": block_network,
        "outbound_domain_allowlist": sorted,
        "image": pin.policy.image,
        "command": pin.policy.command,
        "stage": {
            "max_slots": pin.policy.stage.max_slots,
            "max_chunk_bytes": pin.policy.stage.max_chunk_bytes,
            "max_slot_bytes": pin.policy.stage.max_slot_bytes,
        },
    })
}

#[cfg(test)]
pub mod test_support {
    //! A scripted fake facade: replays prepared replies in order and records
    //! each request's route and body.

    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    pub const PIN_SHA: &str = "a9ebf7940e72bdfad4264c5075df04689681998ab1b0994d9588cc451fcb67ba";

    pub fn pin() -> FacadePin {
        serde_json::from_value(json!({
            "path": "facade/ryeos_lease_facade.py",
            "sha256": PIN_SHA,
            "app": "ryeos-lease-facade",
            "lease_app": "ryeos-lease-sandboxes",
            "endpoint_label": "ryeos-lease",
            "response_schema": "ryeos.modal_lease_facade.response.v1",
            "policy": {
                "schema": "ryeos.modal_lease_policy.v2",
                "cpu": 1.0,
                "memory_mib": 1024,
                "image": "image",
                "command": ["/bin/sh"],
                "stage": {
                    "max_slots": 4,
                    "max_chunk_bytes": 33554432,
                    "max_slot_bytes": 4295032832u64,
                },
            },
        }))
        .unwrap()
    }

    pub enum Step {
        Reply(RawReply),
        Fail(TransportFailure),
    }

    #[derive(Default)]
    pub struct Scripted {
        steps: RefCell<VecDeque<Step>>,
        pub requests: RefCell<Vec<(RouteName, Value)>>,
    }

    impl Scripted {
        pub fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: RefCell::new(steps.into()),
                requests: RefCell::default(),
            }
        }

        pub fn routes(&self) -> Vec<RouteName> {
            self.requests.borrow().iter().map(|(route, _)| *route).collect()
        }

        pub fn exhausted(&self) -> bool {
            self.steps.borrow().is_empty()
        }
    }

    impl FacadeTransport for Scripted {
        fn post(&self, route: RouteName, body: &[u8]) -> Result<RawReply, TransportFailure> {
            self.requests
                .borrow_mut()
                .push((route, serde_json::from_slice(body).unwrap()));
            match self.steps.borrow_mut().pop_front().expect("unscripted request") {
                Step::Reply(raw) => Ok(raw),
                Step::Fail(failure) => Err(failure),
            }
        }
    }

    pub fn envelope(operation: &str, status: u16, fields: Value) -> Step {
        envelope_with(operation, status, PIN_SHA, fields)
    }

    pub fn envelope_with(operation: &str, status: u16, sha: &str, fields: Value) -> Step {
        let mut body = json!({
            "schema": "ryeos.modal_lease_facade.response.v1",
            "facade_sha256": sha,
            "facade_app": "ryeos-lease-facade",
            "guest_image": "image",
            "modal_client": "1.0",
            "operation": operation,
        });
        for (key, value) in fields.as_object().unwrap() {
            body[key] = value.clone();
        }
        Step::Reply(RawReply {
            status,
            json_content_type: true,
            body: serde_json::to_vec(&body).unwrap(),
        })
    }

    pub fn refusal(operation: &str, code: &str) -> Step {
        envelope(operation, 422, json!({"ok": false, "refusal": code, "detail": "fixed"}))
    }

    pub fn provider_error(operation: &str, class: &str) -> Step {
        envelope(operation, 502, json!({"ok": false, "provider_error": class}))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn client_for(steps: Vec<Step>) -> (Scripted, FacadePin) {
        (Scripted::new(steps), pin())
    }

    #[test]
    fn refusal_codes_are_unique_and_documented_shape() {
        let mut sorted = KNOWN_REFUSALS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), KNOWN_REFUSALS.len());
        for code in KNOWN_REFUSALS {
            assert!(code.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'));
        }
    }

    #[test]
    fn every_documented_refusal_maps_and_unknown_ones_are_unattested() {
        for code in KNOWN_REFUSALS {
            let (fake, pin) = client_for(vec![refusal("get", code)]);
            let reply = FacadeClient::new(&fake, &pin).get_by_name("lease-a").unwrap();
            assert_eq!(reply.refusal(), Some(code));
        }
        let (fake, pin) = client_for(vec![refusal("get", "made_up_refusal")]);
        assert_eq!(
            FacadeClient::new(&fake, &pin).get_by_name("lease-a").unwrap_err(),
            CallError::Unattested("unrecognised refusal")
        );
    }

    #[test]
    fn pin_mismatch_fails_the_evidence() {
        let (fake, pin) = client_for(vec![envelope_with(
            "get",
            200,
            &"b".repeat(64),
            json!({"ok": true, "found": false}),
        )]);
        assert_eq!(
            FacadeClient::new(&fake, &pin).get_by_name("lease-a").unwrap_err(),
            CallError::PinMismatch
        );
    }

    #[test]
    fn replies_that_are_not_attested_envelopes_settle_nothing() {
        let plain = |status: u16, json: bool, body: &str| {
            Step::Reply(RawReply {
                status,
                json_content_type: json,
                body: body.as_bytes().to_vec(),
            })
        };
        for (step, reason) in [
            (plain(401, true, "{\"detail\":\"proxy auth\"}"), "reply is not a facade envelope"),
            (plain(200, false, "{}"), "reply is not JSON"),
            (plain(200, true, "[1]"), "reply is not a JSON object"),
            (plain(200, true, "not json"), "reply is not strict JSON"),
            (
                plain(200, true, "{\"a\":1,\"a\":2}"),
                "reply is not strict JSON",
            ),
            (
                envelope("create", 200, json!({"ok": true})),
                "envelope names another app or operation",
            ),
            (
                envelope("get", 200, json!({"ok": false, "refusal": "invalid_name"})),
                "status contradicts the envelope",
            ),
            (
                envelope("get", 422, json!({"ok": true})),
                "status contradicts the envelope",
            ),
            (
                envelope("get", 502, json!({"ok": false, "provider_error": "bad class!"})),
                "provider failure without a class",
            ),
        ] {
            let (fake, pin) = client_for(vec![step]);
            assert_eq!(
                FacadeClient::new(&fake, &pin).get_by_name("lease-a").unwrap_err(),
                CallError::Unattested(reason)
            );
        }
        let oversized = Step::Reply(RawReply {
            status: 200,
            json_content_type: true,
            body: vec![b' '; MAX_REPLY_BYTES + 1],
        });
        let (fake, pin) = client_for(vec![oversized]);
        assert!(FacadeClient::new(&fake, &pin).get_by_name("lease-a").is_err());
    }

    #[test]
    fn transport_failures_keep_their_contact_state() {
        let (fake, pin) = client_for(vec![
            Step::Fail(TransportFailure::NoRequestSent),
            Step::Fail(TransportFailure::MayHaveBeenSent),
        ]);
        let client = FacadeClient::new(&fake, &pin);
        assert_eq!(client.get_by_name("lease-a").unwrap_err(), CallError::NoRequestSent);
        assert_eq!(client.get_by_name("lease-a").unwrap_err(), CallError::Ambiguous);
    }

    #[test]
    fn requests_are_fixed_shapes_over_fixed_routes() {
        let (fake, pin) = client_for(vec![
            envelope("get", 200, json!({"ok": true, "found": false})),
            envelope("get", 200, json!({"ok": true, "found": false})),
            envelope("terminate", 200, json!({"ok": true, "found": false})),
            envelope("terminate", 200, json!({"ok": true, "found": false})),
            envelope("upload", 200, json!({"ok": true, "sandbox_id": "sb-1", "bytes": 3, "sha256": lillux::sha256_hex(b"abc")})),
            envelope("run_once", 200, json!({"ok": true, "sandbox_id": "sb-1", "released": true, "stdin_bytes": 1, "stdin_sha256": lillux::sha256_hex(b"x")})),
        ]);
        let client = FacadeClient::new(&fake, &pin);
        client.get_by_name("lease-a").unwrap();
        client.get_by_id("sb-1").unwrap();
        client.terminate_by_id("sb-1").unwrap();
        client.terminate_by_name("lease-a").unwrap();
        let upload = client.upload("sb-1", b"abc").unwrap();
        let run = client.run_once("sb-1", b"x").unwrap();
        let requests = fake.requests.borrow();
        assert_eq!(requests[0].1, json!({"name": "lease-a"}));
        assert_eq!(requests[1].1, json!({"sandbox_id": "sb-1"}));
        assert_eq!(requests[2].1, json!({"sandbox_id": "sb-1"}));
        assert_eq!(requests[3].1, json!({"name": "lease-a"}));
        assert_eq!(requests[4].1["content_b64"], "YWJj");
        assert_eq!(requests[4].1["sha256"], lillux::sha256_hex(b"abc"));
        assert_eq!(requests[5].1["stdin_b64"], "eA==");
        let uploaded: UploadObservation = upload.observation().unwrap();
        assert_eq!((uploaded.bytes, uploaded.sandbox_id.as_str()), (3, "sb-1"));
        let released: RunOnceObservation = run.observation().unwrap();
        assert!(released.released && released.stdin_bytes == 1);
    }

    #[test]
    fn stage_requests_are_fixed_shapes_over_the_stage_routes() {
        let (fake, pin) = client_for(vec![
            envelope("stage", 200, json!({"ok": true, "sandbox_id": "sb-1", "slot": "pkg", "offset": 0, "bytes": 3, "slot_bytes": 3, "sha256": lillux::sha256_hex(b"abc")})),
            envelope("stage_seal", 200, json!({"ok": true, "sandbox_id": "sb-1", "slot": "pkg", "sealed": true, "already_sealed": false, "bytes": 3, "sha256": lillux::sha256_hex(b"abc")})),
            refusal("stage", "offset_mismatch"),
            refusal("stage_seal", "seal_digest_mismatch"),
        ]);
        let client = FacadeClient::new(&fake, &pin);
        let staged = client.stage("sb-1", "pkg", 0, b"abc").unwrap();
        let sealed = client.stage_seal("sb-1", "pkg", 3, &lillux::sha256_hex(b"abc")).unwrap();
        assert_eq!(
            client.stage("sb-1", "pkg", 9, b"x").unwrap().refusal(),
            Some("offset_mismatch")
        );
        assert_eq!(
            client.stage_seal("sb-1", "pkg", 3, &"0".repeat(64)).unwrap().refusal(),
            Some("seal_digest_mismatch")
        );
        let requests = fake.requests.borrow();
        assert_eq!(requests[0].0, RouteName::Stage);
        assert_eq!(
            requests[0].1,
            json!({"sandbox_id": "sb-1", "slot": "pkg", "offset": 0, "content_b64": "YWJj", "chunk_sha256": lillux::sha256_hex(b"abc")})
        );
        assert_eq!(requests[1].0, RouteName::StageSeal);
        assert_eq!(
            requests[1].1,
            json!({"sandbox_id": "sb-1", "slot": "pkg", "total_bytes": 3, "sha256": lillux::sha256_hex(b"abc")})
        );
        assert_eq!(staged.observation::<StageObservation>().unwrap().slot_bytes, 3);
        assert!(sealed.observation::<SealObservation>().unwrap().sealed);
    }

    #[test]
    fn a_stage_reply_from_another_pin_is_not_attested() {
        let (fake, pin) = client_for(vec![envelope_with(
            "stage",
            200,
            &"b".repeat(64),
            json!({"ok": true}),
        )]);
        assert_eq!(
            FacadeClient::new(&fake, &pin).stage("sb-1", "pkg", 0, b"a").unwrap_err(),
            CallError::PinMismatch
        );
    }

    #[test]
    fn sorted_compact_digest_matches_a_python_style_canonical_form() {
        let value = json!({"b": [1, {"z": true, "a": null}], "a": "x\"y", "c": 3});
        let expected = "{\"a\":\"x\\\"y\",\"b\":[1,{\"a\":null,\"z\":true}],\"c\":3}";
        assert_eq!(sorted_compact_sha256(&value), lillux::sha256_hex(expected.as_bytes()));
    }

    #[test]
    fn policy_digest_matches_the_python_facade_form() {
        // Digest computed once with Python's
        // json.dumps(record, sort_keys=True, separators=(',', ':')) over the
        // record the facade builds from the pinned spec inputs.
        let schema = lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json"));
        let spec = crate::provider_spec::ProviderSpec::parse(
            include_bytes!("../fixtures/provider-spec.json"),
            &schema,
        )
        .unwrap();
        let record = expected_policy_record(
            spec.facade(),
            "node-a",
            600,
            false,
            &["b.example.com", "a.example.com"],
        );
        assert_eq!(
            sorted_compact_sha256(&record),
            "87ec993c0f85617625df04d078bebb0d3c70ee5ade309709ae57333e868143aa"
        );
    }
}
