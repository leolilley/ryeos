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
    FACT_AUTHORIZED_KEYS, FACT_CAPACITY_ACCOUNT, FACT_EXTERNAL_EXECUTION, FACT_POLICY_GENERATION,
    FACT_RESTART_INHIBITION, FACT_RUNTIME_SNAPSHOTS,
};
use super::handlers;
use super::host::OfflineHost;

const KIB: u64 = 1024;

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
        id: "node.external-execution.snapshot-compose",
        authority_bearing: true,
        local_signer_only: false,
        alone: false,
        inputs: &[InputSlot {
            name: "provider_spec",
            max_bytes: 16 * KIB,
            required: true,
        }],
        preconditions: &[FACT_RUNTIME_SNAPSHOTS, FACT_EXTERNAL_EXECUTION],
        expected_result: &[],
        required_expected_result: &[],
        validate_request: arguments_of::<handlers::SnapshotComposeArguments>,
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
            "node.external-execution.snapshot-compose",
            "node.external-execution.snapshot-qualification-compose",
            "node.capacity.reissue",
            "node.capacity.reanchor",
            "node.allow-restart",
            "node.reset.execution-history",
            "node.reset.execution-history-scratch",
            "node.reset.replay-indexes",
            "node.reset.external-content-bindings",
            "node.reset.authorization",
        ] {
            assert!(lookup(id).is_some(), "{id}");
        }
        assert_eq!(operations().len(), 13);
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
                "node.external-execution.snapshot-compose",
                "node.external-execution.snapshot-qualification-compose",
                "node.reset.authorization",
            ]
        );
    }

    #[test]
    fn the_authorization_reset_is_local_only_and_alone() {
        let spec = lookup("node.reset.authorization").unwrap();
        assert!(spec.local_signer_only && spec.alone && spec.authority_bearing);
        assert!(operations().iter().filter(|s| s.local_signer_only).count() == 1);
    }

    #[test]
    fn every_handler_has_a_recovery_rule() {
        for spec in operations() {
            assert!(!spec.recovery.record.trim().is_empty(), "{}", spec.id);
            assert!(!spec.recovery.why.trim().is_empty(), "{}", spec.id);
        }
    }
}
