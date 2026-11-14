//! The compiled operation table.
//!
//! Application runs before node configuration can load, and configuration may
//! be exactly what is broken, so the set of operations is compiled into the
//! binary. The handlers are the existing stopped-node cores: a closed,
//! mechanical set. Which operations a remote signer may request is policy and
//! comes from the node's signed profile; nothing here widens it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail, ensure};
use ryeos_node_runtime::identity::{NodeIdentity, OFFLINE_OPERATION_SCOPE_PREFIX};
use ryeos_node_runtime::state_lock::StateLock;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::contract::RequestBody;
use super::facts::{
    FACT_AUTHORIZED_KEYS, FACT_BUNDLE_REGISTRATIONS, FACT_CAPACITY_ACCOUNT,
    FACT_EXTERNAL_EXECUTION, FACT_POLICY_GENERATION, FACT_REMOTES, FACT_RESTART_INHIBITION,
    FACT_RUNTIME_SNAPSHOTS, FACT_TRUST_REGISTRY,
};
use super::handlers;
use super::host::OfflineHost;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// One named input of an operation and the most bytes it may hold.
#[derive(Debug, Clone, Copy)]
pub struct InputSlot {
    pub name: &'static str,
    pub max_bytes: u64,
    pub required: bool,
}

/// How a handler finds that its effect was already made, when the node died
/// after the effect and before the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rerun {
    /// The handler finds the request's digest, or a value derived from it,
    /// in state it owns and returns without acting again.
    RecognisesRecordedDigest,
    /// Acting again over an effect already made changes nothing further.
    ConvergesOverEffect,
}

/// The recovery rule of one handler. A handler without one is not
/// registered: the field is not optional, and a test refuses empty text.
#[derive(Debug, Clone, Copy)]
pub struct Recovery {
    /// Where the handler's own durable state records the request.
    pub record: &'static str,
    pub rerun: Rerun,
    /// Why the rerun is safe.
    pub why: &'static str,
}

/// What a handler is given. It carries no path into the ledger and no way to
/// read the process environment.
pub struct HandlerContext<'a> {
    pub app_root: &'a Path,
    /// Held by the application for the whole call.
    pub state_lock: &'a StateLock,
    pub request: &'a RequestBody,
    pub request_digest: &'a str,
    pub inputs: &'a VerifiedInputs,
    pub host: &'a dyn OfflineHost,
    pub node: &'a NodeIdentity,
}

impl HandlerContext<'_> {
    pub fn arguments<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_value(self.request.arguments.clone())?)
    }

    pub fn expected(&self, fact: &str) -> Option<&str> {
        self.request.expected_result.get(fact).map(String::as_str)
    }

    /// The label a handler writes into the evidence it keeps, so the request
    /// can be found from the state it changed.
    pub fn command_label(&self) -> String {
        format!(
            "offline-operation {} {}",
            self.request.operation, self.request_digest
        )
    }
}

/// Input bytes, each read once from its descriptor and checked against the
/// digest the request pinned. A handler is handed these bytes and never
/// re-reads a path.
#[derive(Debug, Default)]
pub struct VerifiedInputs {
    slots: BTreeMap<String, Vec<u8>>,
}

impl VerifiedInputs {
    pub fn new(slots: BTreeMap<String, Vec<u8>>) -> Self {
        Self { slots }
    }

    pub fn get(&self, slot: &str) -> Option<&[u8]> {
        self.slots.get(slot).map(Vec::as_slice)
    }

    pub fn require(&self, slot: &str) -> Result<&[u8]> {
        self.get(slot)
            .ok_or_else(|| anyhow::anyhow!("input `{slot}` is absent"))
    }
}

/// The receipt-first shape: the receipt lists what the effect will discard,
/// and is appended before the effect runs.
pub struct Prepared {
    pub discarded_grant_digests: Vec<String>,
}

pub type EffectFn = fn(&HandlerContext<'_>) -> Result<Value>;
pub type PrepareFn = fn(&HandlerContext<'_>) -> Result<Prepared>;

#[derive(Clone, Copy)]
pub enum Handler {
    /// Make the effect, then write the receipt.
    EffectThenReceipt(EffectFn),
    /// Write the receipt, then make the effect. The effect is convergent, so
    /// a node that dies between the two completes it at its next startup.
    ReceiptThenEffect {
        prepare: PrepareFn,
        effect: EffectFn,
    },
}

pub struct OperationSpec {
    pub id: &'static str,
    /// Changes what the node trusts or runs. Granting one is equivalent to
    /// granting all, so a remote signer may request it only when the node's
    /// signed profile delegates it.
    pub authority_bearing: bool,
    /// Only the node's local operator may sign it.
    pub local_signer_only: bool,
    /// Applied alone in its startup: nothing is applied before it in the same
    /// startup, and nothing after.
    pub alone: bool,
    pub inputs: &'static [InputSlot],
    /// Operation-specific facts the request must pin, beyond the mandatory
    /// `init_completion` and `ledger_head`.
    pub preconditions: &'static [&'static str],
    /// Facts the request may pin about the result.
    pub expected_result: &'static [&'static str],
    /// Expected-result facts the request must pin.
    pub required_expected_result: &'static [&'static str],
    /// Checks the arguments against the operation's schema, together with the
    /// inputs they depend on.
    pub validate_request: fn(&RequestBody) -> Result<()>,
    pub handler: Handler,
    pub recovery: Recovery,
}

impl OperationSpec {
    /// The exact scope a grant must name to authorize this operation.
    pub fn scope(&self) -> String {
        format!("{OFFLINE_OPERATION_SCOPE_PREFIX}{}", self.id)
    }

    pub fn slot(&self, name: &str) -> Option<&InputSlot> {
        self.inputs.iter().find(|slot| slot.name == name)
    }
}

fn arguments_of<T: DeserializeOwned>(request: &RequestBody) -> Result<()> {
    serde_json::from_value::<T>(request.arguments.clone())
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("{error}"))
}

const NO_INPUTS: &[InputSlot] = &[];

static OPERATIONS: &[OperationSpec] = &[
    OperationSpec {
        id: "node.reset.policy-generation",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[FACT_POLICY_GENERATION],
        expected_result: &["policy_generation"],
        required_expected_result: &["policy_generation"],
        validate_request: |request| {
            let arguments: handlers::PolicyReplaceArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::reset_policy_generation),
        recovery: Recovery {
            record: "the replaced policy generation itself: its digest is the request's pinned result",
            rerun: Rerun::ConvergesOverEffect,
            why: "the replacement is one locked initialization transaction that installs the same pinned generation and reseals the completion each time it runs",
        },
    },
    OperationSpec {
        id: "node.policy-apply",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: &[InputSlot {
            name: "policy",
            max_bytes: 256 * KIB,
            required: true,
        }],
        preconditions: &[FACT_POLICY_GENERATION],
        expected_result: &["policy_generation"],
        required_expected_result: &["policy_generation"],
        validate_request: arguments_of::<handlers::PolicyApplyArguments>,
        handler: Handler::EffectThenReceipt(handlers::policy_apply),
        recovery: Recovery {
            record: "the published policy generation: its digest is the request's pinned result",
            rerun: Rerun::ConvergesOverEffect,
            why: "publishing the same member over a generation that already carries it writes the same signed generation, and the completion is resealed",
        },
    },
    OperationSpec {
        id: "node.external-execution.compose",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: &[
            InputSlot {
                name: "settings",
                max_bytes: 256 * KIB,
                required: true,
            },
            InputSlot {
                name: "program",
                max_bytes: 256 * KIB,
                required: true,
            },
            InputSlot {
                name: "tls_roots",
                max_bytes: 256 * KIB,
                required: false,
            },
            InputSlot {
                name: "limits",
                max_bytes: 256 * KIB,
                required: false,
            },
            InputSlot {
                name: "runtime_qualification",
                max_bytes: 256 * KIB,
                required: false,
            },
            InputSlot {
                name: "runtime_content_qualification",
                max_bytes: 256 * KIB,
                required: false,
            },
        ],
        preconditions: &[FACT_EXTERNAL_EXECUTION],
        expected_result: &["settings_digest"],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::ComposeArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate(&request.inputs)
        },
        handler: Handler::EffectThenReceipt(handlers::external_compose),
        recovery: Recovery {
            record: "the installed binding's credential generation, derived from the request digest",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "a binding already installed under the id with the derived generation is the effect of this request and is reported as installed",
        },
    },
    OperationSpec {
        id: "node.external-execution.qualification-session-compose",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: &[
            InputSlot {
                name: "tls_roots",
                max_bytes: 256 * KIB,
                required: false,
            },
            InputSlot {
                name: "limits",
                max_bytes: 256 * KIB,
                required: false,
            },
        ],
        preconditions: &[FACT_EXTERNAL_EXECUTION],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::QualificationSessionComposeArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate(&request.inputs)
        },
        handler: Handler::EffectThenReceipt(handlers::qualification_session_compose),
        recovery: Recovery {
            record: "the installed binding's credential generation, derived from the request digest",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "a qualification binding already installed under the id with the derived generation is the effect of this request and is reported as installed",
        },
    },
    OperationSpec {
        id: "node.external-execution.snapshot-compose",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: &[
            InputSlot {
                name: "provider_spec",
                max_bytes: 16 * KIB,
                required: true,
            },
            InputSlot {
                name: "tls_roots",
                max_bytes: 256 * KIB,
                required: false,
            },
        ],
        preconditions: &[FACT_RUNTIME_SNAPSHOTS, FACT_EXTERNAL_EXECUTION],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::SnapshotComposeArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate(&request.inputs)
        },
        handler: Handler::EffectThenReceipt(handlers::snapshot_compose),
        recovery: Recovery {
            record: "the installed production binding's credential generation, derived from the request digest",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "a production binding already installed under the id with the derived generation is the effect of this request and is reported as installed",
        },
    },
    OperationSpec {
        id: "node.external-execution.snapshot-qualification-compose",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: &[
            InputSlot {
                name: "provider_spec",
                max_bytes: 256 * KIB,
                required: true,
            },
            InputSlot {
                name: "consumer_verifiers",
                max_bytes: 256 * KIB,
                required: false,
            },
        ],
        preconditions: &[FACT_RUNTIME_SNAPSHOTS],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::QualificationComposeArguments>,
        handler: Handler::EffectThenReceipt(handlers::snapshot_qualification_compose),
        recovery: Recovery {
            record: "the intent record the application keeps for the request; the binding itself carries no field for it",
            rerun: Rerun::ConvergesOverEffect,
            why: "the binding is a pure function of the pinned inputs and the installed production binding, so composing again publishes an equivalent document under the same id",
        },
    },
    OperationSpec {
        id: "node.external-execution.recompose",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: &[InputSlot {
            name: "consumer_verifiers",
            max_bytes: 256 * KIB,
            required: false,
        }],
        preconditions: &[FACT_RUNTIME_SNAPSHOTS, FACT_EXTERNAL_EXECUTION],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::RecomposeArguments>,
        handler: Handler::EffectThenReceipt(handlers::recompose),
        recovery: Recovery {
            record: "the installed bindings themselves: a recomposed binding pins exactly what is installed",
            rerun: Rerun::ConvergesOverEffect,
            why: "a re-pin that changes no pinned identity publishes nothing, so a rerun re-pins only the bindings an interrupted run had not yet published and carries every other fact and the credential generation unchanged",
        },
    },
    OperationSpec {
        id: "node.capacity.reissue",
        authority_bearing: false,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[FACT_CAPACITY_ACCOUNT],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::CapacityArguments>,
        handler: Handler::EffectThenReceipt(handlers::capacity_reissue),
        recovery: Recovery {
            record: "the capacity evidence log, whose command field names the request; the anchor names the signed document by digest",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "a published anchor is recognised by its document digest and only repaired, never renewed again",
        },
    },
    OperationSpec {
        id: "node.capacity.reanchor",
        authority_bearing: false,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[FACT_CAPACITY_ACCOUNT],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::CapacityArguments>,
        handler: Handler::EffectThenReceipt(handlers::capacity_reanchor),
        recovery: Recovery {
            record: "the capacity re-anchor evidence log, whose command field names the request; the binding names the signed document by digest",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "a binding that already names this place reports the move as applied and only owes the evidence line",
        },
    },
    OperationSpec {
        id: "node.allow-restart",
        authority_bearing: false,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[FACT_RESTART_INHIBITION],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::EmptyArguments>,
        handler: Handler::EffectThenReceipt(handlers::allow_restart),
        recovery: Recovery {
            record: "the restart clear log, whose command field names the request",
            rerun: Rerun::ConvergesOverEffect,
            why: "an inhibition already cleared leaves nothing to clear",
        },
    },
    OperationSpec {
        id: "node.reset.execution-history",
        authority_bearing: false,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::ExecutionHistoryResetArguments>,
        handler: Handler::EffectThenReceipt(handlers::reset_execution_history),
        recovery: Recovery {
            record: "the reset's own operation record, which settles an interrupted attempt at its next run",
            rerun: Rerun::ConvergesOverEffect,
            why: "a reset over a history already retired retires nothing further, and an interrupted attempt is settled and resumed by the reset itself",
        },
    },
    OperationSpec {
        id: "node.reset.execution-history-scratch",
        authority_bearing: false,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::EmptyArguments>,
        handler: Handler::EffectThenReceipt(handlers::reset_execution_history_scratch),
        recovery: Recovery {
            record: "none needed: the scratch it clears is the only state it touches",
            rerun: Rerun::ConvergesOverEffect,
            why: "a second run finds nothing left to remove",
        },
    },
    OperationSpec {
        id: "node.reset.replay-indexes",
        authority_bearing: false,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::EmptyArguments>,
        handler: Handler::EffectThenReceipt(handlers::reset_replay_indexes),
        recovery: Recovery {
            record: "the replay-index epoch stored with the indexes",
            rerun: Rerun::ConvergesOverEffect,
            why: "an epoch already current leaves the indexes unchanged",
        },
    },
    OperationSpec {
        id: "node.reset.external-content-bindings",
        authority_bearing: false,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::EmptyArguments>,
        handler: Handler::EffectThenReceipt(handlers::reset_external_content_bindings),
        recovery: Recovery {
            record: "the binding epoch file the cutover publishes",
            rerun: Rerun::ConvergesOverEffect,
            why: "every step of the cutover is idempotent, and a second run retires no further heads",
        },
    },
    OperationSpec {
        id: "node.large-objects.purge-quarantine",
        authority_bearing: false,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::EmptyArguments>,
        handler: Handler::EffectThenReceipt(handlers::purge_quarantined_large_objects),
        recovery: Recovery {
            record: "none needed: the quarantine it empties is the only state it touches",
            rerun: Rerun::ConvergesOverEffect,
            why: "a second run finds nothing left in quarantine to remove",
        },
    },
    OperationSpec {
        id: "node.authorize-client",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[FACT_AUTHORIZED_KEYS],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::AuthorizeClientArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::authorize_client),
        recovery: Recovery {
            record: "the node-signed grant itself, keyed by the public key's fingerprint",
            rerun: Rerun::ConvergesOverEffect,
            why: "reconciling the same scopes, class and origin over a grant that already carries them writes an equivalent grant; a merge is a union and adds nothing the second time",
        },
    },
    OperationSpec {
        id: "node.remote.pin",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: &[InputSlot {
            name: "descriptor",
            max_bytes: 64 * KIB,
            required: true,
        }],
        preconditions: &[FACT_REMOTES],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::RemotePinArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::remote_pin),
        recovery: Recovery {
            record: "the remotes file entry under the pinned name",
            rerun: Rerun::ConvergesOverEffect,
            why: "pinning the same descriptor under the same name writes the same entry over itself",
        },
    },
    OperationSpec {
        id: "node.bundle.replace",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: &[InputSlot {
            name: "archive",
            max_bytes: 2 * MIB,
            required: true,
        }],
        preconditions: &[FACT_BUNDLE_REGISTRATIONS],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::BundleReplaceArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::bundle_replace),
        recovery: Recovery {
            record: "the bundle transaction journal, which the next mutation of the name reconciles first",
            rerun: Rerun::ConvergesOverEffect,
            why: "an interrupted replacement is finished or rolled back by its journal, and replacing a bundle with the same archive again installs the same tree and registration",
        },
    },
    OperationSpec {
        id: "node.bundle-state.export",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::BundleStateExportArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::bundle_state_export),
        recovery: Recovery {
            record: "the archive at the output path, which the export creates exclusively",
            rerun: Rerun::ConvergesOverEffect,
            why: "a rerun that finds a verified archive of the same bundle at the output path, signed by this node, reports it instead of writing again; the node's state is only read",
        },
    },
    OperationSpec {
        id: "node.bundle-state.import",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: &[InputSlot {
            name: "archive",
            max_bytes: 128 * MIB,
            required: true,
        }],
        preconditions: &[FACT_BUNDLE_REGISTRATIONS, FACT_TRUST_REGISTRY],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::BundleStateImportArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::bundle_state_import),
        recovery: Recovery {
            record: "the namespaces the archive installed, which hold exactly the archive's bytes",
            rerun: Rerun::ConvergesOverEffect,
            why: "a namespace already holding exactly the archive's bytes is reported unchanged, and an interrupted write is finished by naming the namespace under --replace-namespace",
        },
    },
    OperationSpec {
        id: "node.bundle-state.remove",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::BundleStateRemoveArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::ReceiptThenEffect {
            prepare: handlers::bundle_state_remove_prepare,
            effect: handlers::bundle_state_remove_effect,
        },
        recovery: Recovery {
            record: "the ledger receipt, appended before the delete and listing the digest of the directory listing it covers",
            rerun: Rerun::ConvergesOverEffect,
            why: "removing what is already gone removes nothing further, and the intent record says the receipt exists",
        },
    },
    OperationSpec {
        id: "node.reset.authorization",
        authority_bearing: true,
        local_signer_only: true,
        alone: true,
        inputs: NO_INPUTS,
        preconditions: &[FACT_AUTHORIZED_KEYS],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::EmptyArguments>,
        handler: Handler::ReceiptThenEffect {
            prepare: handlers::authorization_reset_prepare,
            effect: handlers::authorization_reset_effect,
        },
        recovery: Recovery {
            record: "the ledger receipt, appended before the swap and listing every discarded grant digest",
            rerun: Rerun::ConvergesOverEffect,
            why: "the swap installs one local-operator grant over whatever is there, so a swap already made is made again to the same effect; the intent record says the receipt exists",
        },
    },
    OperationSpec {
        id: "node.authorization.revoke",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[FACT_AUTHORIZED_KEYS],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::AuthorizationRevokeArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::ReceiptThenEffect {
            prepare: handlers::authorization_revoke_prepare,
            effect: handlers::authorization_revoke_effect,
        },
        recovery: Recovery {
            record: "the ledger receipt, appended before the effect and listing every revoked grant digest, and the node-signed tombstones the effect writes",
            rerun: Rerun::ConvergesOverEffect,
            why: "a tombstone is written before its grant file is removed and an existing tombstone is kept, so a rerun finishes any removal an interruption left and confirms the rest; an unknown fingerprint is refused with a typed error before any receipt",
        },
    },
    OperationSpec {
        id: "node.trust.pin",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: &[InputSlot {
            name: "signer",
            max_bytes: 16 * KIB,
            required: true,
        }],
        preconditions: &[FACT_TRUST_REGISTRY],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::TrustPinArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::trust_pin),
        recovery: Recovery {
            record: "the key event whose request digest is the request's",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "an event carrying this request's digest means the pin was made; a rerun writes only a record that event names but the volume lacks",
        },
    },
    OperationSpec {
        id: "node.trust.scope",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[FACT_TRUST_REGISTRY],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::TrustScopeArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::trust_scope_change),
        recovery: Recovery {
            record: "the key event whose request digest is the request's",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "an event carrying this request's digest means the scope was changed; a rerun writes only a record that event names but the volume lacks",
        },
    },
    OperationSpec {
        id: "node.trust.retire",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[FACT_TRUST_REGISTRY],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::TrustFingerprintArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::trust_retire),
        recovery: Recovery {
            record: "the key event whose request digest is the request's",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "an event carrying this request's digest means the signer was retired; a rerun writes only a record that event names but the volume lacks",
        },
    },
    OperationSpec {
        id: "node.trust.revoke",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: NO_INPUTS,
        preconditions: &[FACT_TRUST_REGISTRY],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: |request| {
            let arguments: handlers::TrustFingerprintArguments =
                serde_json::from_value(request.arguments.clone())?;
            arguments.validate()
        },
        handler: Handler::EffectThenReceipt(handlers::trust_revoke),
        recovery: Recovery {
            record: "the key event whose request digest is the request's",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "an event carrying this request's digest means the signer was revoked; a rerun writes only a record that event names but the volume lacks",
        },
    },
    OperationSpec {
        id: "node.trust.succeed",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: &[InputSlot {
            name: "succession",
            max_bytes: 16 * KIB,
            required: true,
        }],
        preconditions: &[FACT_TRUST_REGISTRY],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::TrustSucceedArguments>,
        handler: Handler::EffectThenReceipt(handlers::trust_succeed),
        recovery: Recovery {
            record: "the key events whose request digest is the request's, one per record the succession changes",
            rerun: Rerun::RecognisesRecordedDigest,
            why: "events carrying this request's digest mean those records were changed; a rerun writes only records that events name but the volume lacks and the events still missing",
        },
    },
    OperationSpec {
        id: "node.operator.rotate",
        authority_bearing: true,
        local_signer_only: true,
        alone: true,
        inputs: &[InputSlot {
            name: "succession",
            max_bytes: 16 * KIB,
            required: true,
        }],
        preconditions: &[FACT_TRUST_REGISTRY, FACT_AUTHORIZED_KEYS],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::EmptyArguments>,
        handler: Handler::EffectThenReceipt(handlers::operator_rotate),
        recovery: Recovery {
            record: "the key event whose request digest is the request's, and the operator key at its canonical path",
            rerun: Rerun::ConvergesOverEffect,
            why: "the succession event is recognised by the request digest, the genesis, grant and completion are rewritten to the same new key, and the new key is installed last, so a request that stopped earlier finishes and one that stopped after installing it finds the new key in place",
        },
    },
    OperationSpec {
        id: "node.authoring.rotate",
        authority_bearing: true,
        local_signer_only: true,
        alone: false,
        inputs: &[InputSlot {
            name: "succession",
            max_bytes: 16 * KIB,
            required: true,
        }],
        preconditions: &[FACT_TRUST_REGISTRY],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::EmptyArguments>,
        handler: Handler::EffectThenReceipt(handlers::authoring_rotate),
        recovery: Recovery {
            record: "the key event whose request digest is the request's, and the authoring key at its canonical path",
            rerun: Rerun::ConvergesOverEffect,
            why: "the succession event is recognised by the request digest and the new key is installed last, so a request that stopped earlier finishes and one that stopped after installing it finds the new key in place",
        },
    },
];

/// Every operation this binary can apply.
pub fn operations() -> &'static [OperationSpec] {
    OPERATIONS
}

pub fn lookup(id: &str) -> Option<&'static OperationSpec> {
    OPERATIONS.iter().find(|spec| spec.id == id)
}

/// Checks that hold for the table itself; run by a test and before the first
/// request is applied, so a malformed table cannot apply anything.
pub fn validate_table() -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for spec in OPERATIONS {
        ensure!(
            seen.insert(spec.id),
            "operation `{}` is listed twice",
            spec.id
        );
        ensure!(
            !spec.id.is_empty()
                && spec.id.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'.'
                    || b == b'-'),
            "operation id `{}` is not a plain dotted name",
            spec.id
        );
        ensure!(
            !spec.recovery.record.trim().is_empty() && !spec.recovery.why.trim().is_empty(),
            "operation `{}` has no recovery rule",
            spec.id
        );
        for required in spec.required_expected_result {
            ensure!(
                spec.expected_result.contains(required),
                "operation `{}` requires result fact `{required}` it does not allow",
                spec.id
            );
        }
        for fact in spec.preconditions {
            ensure!(
                super::facts::ALL_FACTS.contains(fact),
                "operation `{}` pins unknown fact `{fact}`",
                spec.id
            );
        }
        if spec.local_signer_only && !spec.authority_bearing {
            bail!(
                "operation `{}` is local-signer-only without being authority-bearing",
                spec.id
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_well_formed() {
        validate_table().unwrap();
    }

    #[test]
    fn the_first_cut_operations_are_registered() {
        for id in [
            "node.reset.policy-generation",
            "node.policy-apply",
            "node.external-execution.compose",
            "node.external-execution.qualification-session-compose",
            "node.external-execution.snapshot-compose",
            "node.external-execution.snapshot-qualification-compose",
            "node.external-execution.recompose",
            "node.capacity.reissue",
            "node.capacity.reanchor",
            "node.allow-restart",
            "node.reset.execution-history",
            "node.reset.execution-history-scratch",
            "node.reset.replay-indexes",
            "node.reset.external-content-bindings",
            "node.large-objects.purge-quarantine",
            "node.authorize-client",
            "node.remote.pin",
            "node.bundle.replace",
            "node.bundle-state.export",
            "node.bundle-state.import",
            "node.bundle-state.remove",
            "node.reset.authorization",
            "node.authorization.revoke",
            "node.trust.pin",
            "node.trust.scope",
            "node.trust.retire",
            "node.trust.revoke",
            "node.trust.succeed",
            "node.operator.rotate",
            "node.authoring.rotate",
        ] {
            assert!(lookup(id).is_some(), "{id}");
        }
        assert_eq!(operations().len(), 30);
    }

    #[test]
    fn scopes_are_exact_and_carry_the_offline_prefix() {
        for spec in operations() {
            let scope = spec.scope();
            assert!(scope.starts_with("ryeos.offline."), "{scope}");
            assert!(!scope.contains('*'), "{scope}");
            assert!(
                scope.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'.'
                    || b == b'-'),
                "{scope} is not a canonical scope spelling"
            );
        }
    }

    #[test]
    fn only_authority_bearing_operations_are_flagged_so() {
        let flagged: Vec<&str> = operations()
            .iter()
            .filter(|spec| spec.authority_bearing)
            .map(|spec| spec.id)
            .collect();
        assert_eq!(
            flagged,
            [
                "node.reset.policy-generation",
                "node.policy-apply",
                "node.external-execution.compose",
                "node.external-execution.qualification-session-compose",
                "node.external-execution.snapshot-compose",
                "node.external-execution.snapshot-qualification-compose",
                "node.external-execution.recompose",
                "node.authorize-client",
                "node.remote.pin",
                "node.bundle.replace",
                "node.bundle-state.export",
                "node.bundle-state.import",
                "node.bundle-state.remove",
                "node.reset.authorization",
                "node.authorization.revoke",
                "node.trust.pin",
                "node.trust.scope",
                "node.trust.retire",
                "node.trust.revoke",
                "node.trust.succeed",
                "node.operator.rotate",
                "node.authoring.rotate",
            ]
        );
    }

    #[test]
    fn trust_operations_are_local_signer_only_and_authority_bearing() {
        for id in [
            "node.trust.pin",
            "node.trust.scope",
            "node.trust.retire",
            "node.trust.revoke",
            "node.trust.succeed",
        ] {
            let spec = lookup(id).unwrap();
            assert!(spec.local_signer_only && spec.authority_bearing, "{id}");
            assert!(spec.preconditions.contains(&FACT_TRUST_REGISTRY), "{id}");
        }
    }

    #[test]
    fn key_rotations_are_local_signer_only_and_the_operator_one_is_alone() {
        for id in ["node.operator.rotate", "node.authoring.rotate"] {
            let spec = lookup(id).unwrap();
            assert!(spec.local_signer_only && spec.authority_bearing, "{id}");
            assert!(spec.slot("succession").is_some_and(|s| s.required), "{id}");
            assert!(spec.preconditions.contains(&FACT_TRUST_REGISTRY), "{id}");
        }
        assert!(lookup("node.operator.rotate").unwrap().alone);
        assert!(
            lookup("node.operator.rotate")
                .unwrap()
                .preconditions
                .contains(&FACT_AUTHORIZED_KEYS)
        );
    }

    #[test]
    fn the_authorization_reset_is_local_only_and_alone() {
        let spec = lookup("node.reset.authorization").unwrap();
        assert!(spec.local_signer_only && spec.alone && spec.authority_bearing);
        assert!(operations().iter().filter(|s| s.local_signer_only).count() == 12);
    }

    #[test]
    fn every_handler_has_a_recovery_rule() {
        for spec in operations() {
            assert!(!spec.recovery.record.trim().is_empty(), "{}", spec.id);
            assert!(!spec.recovery.why.trim().is_empty(), "{}", spec.id);
        }
    }
}
