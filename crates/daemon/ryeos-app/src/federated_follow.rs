//! Typed authority for delivering a followed child's terminal result across sites.
//!
//! The parent follow waiter remains private runtime authority on the parent
//! node. A source-node reservation attests the exact waiter coordinate before
//! child handoff; a target-node terminal attestation later binds the exact
//! signed child-chain head and managed terminal envelope. Neither object grants
//! general chain-writing or parent execution authority.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_state::objects::Attestation;
use ryeos_state::signer::Signer;

pub const REMOTE_FOLLOW_RESERVATION_POLICY: &str = "remote-follow-delivery-reservation-v1";
pub const REMOTE_FOLLOW_RESERVATION_CLAIM: &str = "reserved";
pub const REMOTE_FOLLOW_TERMINAL_POLICY: &str = "remote-follow-terminal-v1";
pub const REMOTE_FOLLOW_TERMINAL_CLAIM: &str = "terminal";
pub const REMOTE_FOLLOW_DELIVERY_OPERATION: &str = "remote_follow_terminal_delivery";
pub const REMOTE_FOLLOW_DELIVERY_SERVICE: &str = "service:federation/follow-terminal-deliver";

const RESERVATION_SCHEMA: &str = "ryeos.remote_follow_delivery_reservation.v1";
const TERMINAL_SCHEMA: &str = "ryeos.remote_follow_terminal.v1";
const JOB_SCHEMA: &str = "ryeos.remote_follow_terminal_delivery_job.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFollowReservationEvidence {
    pub schema: String,
    pub reservation_id: String,
    pub owner_principal: String,
    pub parent_site_id: String,
    pub parent_node_signer_fingerprint: String,
    pub parent_chain_root_id: String,
    pub parent_chain_head_hash: String,
    pub parent_thread_id: String,
    pub parent_successor_thread_id: String,
    pub follow_key: String,
    pub child_item_index: u32,
    pub child_item_ref: String,
    pub child_spec_hash: String,
    pub child_initial_thread_id: String,
    pub child_chain_root_id: String,
}

impl RemoteFollowReservationEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner_principal: String,
        parent_site_id: String,
        parent_node_signer_fingerprint: String,
        parent_chain_root_id: String,
        parent_chain_head_hash: String,
        parent_thread_id: String,
        parent_successor_thread_id: String,
        follow_key: String,
        child_item_index: u32,
        child_item_ref: String,
        child_spec_hash: String,
        child_initial_thread_id: String,
        child_chain_root_id: String,
    ) -> anyhow::Result<Self> {
        let reservation_id = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "schema":RESERVATION_SCHEMA,
            "owner_principal":owner_principal,
            "parent_site_id":parent_site_id,
            "parent_node_signer_fingerprint":parent_node_signer_fingerprint,
            "parent_chain_root_id":parent_chain_root_id,
            "parent_chain_head_hash":parent_chain_head_hash,
            "parent_thread_id":parent_thread_id,
            "parent_successor_thread_id":parent_successor_thread_id,
            "follow_key":follow_key,
            "child_item_index":child_item_index,
            "child_item_ref":child_item_ref,
            "child_spec_hash":child_spec_hash,
            "child_initial_thread_id":child_initial_thread_id,
            "child_chain_root_id":child_chain_root_id,
        }))?;
        let evidence = Self {
            schema: RESERVATION_SCHEMA.to_owned(),
            reservation_id,
            owner_principal,
            parent_site_id,
            parent_node_signer_fingerprint,
            parent_chain_root_id,
            parent_chain_head_hash,
            parent_thread_id,
            parent_successor_thread_id,
            follow_key,
            child_item_index,
            child_item_ref,
            child_spec_hash,
            child_initial_thread_id,
            child_chain_root_id,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != RESERVATION_SCHEMA {
            bail!("remote follow reservation is not the exact current contract");
        }
        for (label, hash) in [
            ("reservation", self.reservation_id.as_str()),
            ("parent chain head", self.parent_chain_head_hash.as_str()),
            ("child specification", self.child_spec_hash.as_str()),
        ] {
            canonical_hash(label, hash)?;
        }
        canonical_fingerprint("parent node signer", &self.parent_node_signer_fingerprint)?;
        for (label, value) in [
            ("owner", self.owner_principal.as_str()),
            ("parent site", self.parent_site_id.as_str()),
            ("parent chain", self.parent_chain_root_id.as_str()),
            ("parent thread", self.parent_thread_id.as_str()),
            ("parent successor", self.parent_successor_thread_id.as_str()),
            ("follow key", self.follow_key.as_str()),
            ("child item", self.child_item_ref.as_str()),
            (
                "child initial thread",
                self.child_initial_thread_id.as_str(),
            ),
            ("child chain", self.child_chain_root_id.as_str()),
        ] {
            canonical_label(label, value)?;
        }
        let expected_id = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "schema":self.schema,
            "owner_principal":self.owner_principal,
            "parent_site_id":self.parent_site_id,
            "parent_node_signer_fingerprint":self.parent_node_signer_fingerprint,
            "parent_chain_root_id":self.parent_chain_root_id,
            "parent_chain_head_hash":self.parent_chain_head_hash,
            "parent_thread_id":self.parent_thread_id,
            "parent_successor_thread_id":self.parent_successor_thread_id,
            "follow_key":self.follow_key,
            "child_item_index":self.child_item_index,
            "child_item_ref":self.child_item_ref,
            "child_spec_hash":self.child_spec_hash,
            "child_initial_thread_id":self.child_initial_thread_id,
            "child_chain_root_id":self.child_chain_root_id,
        }))?;
        if self.reservation_id != expected_id {
            bail!("remote follow reservation identity changed");
        }
        Ok(())
    }

    pub fn sign_attestation(&self, signer: &dyn Signer) -> anyhow::Result<Attestation> {
        self.validate()?;
        Attestation::unsigned(
            self.parent_chain_head_hash.clone(),
            REMOTE_FOLLOW_RESERVATION_CLAIM.to_owned(),
            REMOTE_FOLLOW_RESERVATION_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::to_value(self)?,
        )
        .sign(signer)
    }

    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        if attestation.policy != REMOTE_FOLLOW_RESERVATION_POLICY
            || attestation.claim != REMOTE_FOLLOW_RESERVATION_CLAIM
        {
            bail!("attestation is not a remote follow delivery reservation");
        }
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())?;
        evidence.validate()?;
        if attestation.subject_hash != evidence.parent_chain_head_hash
            || attestation.issuer_fingerprint()? != evidence.parent_node_signer_fingerprint
        {
            bail!("remote follow reservation signer or subject changed");
        }
        Ok(evidence)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFollowTerminalEvidence {
    pub schema: String,
    pub operation_id: String,
    pub reservation_attestation_hash: String,
    pub child_chain_root_id: String,
    pub child_terminal_thread_id: String,
    pub terminal_status: String,
    pub target_site_id: String,
    pub target_node_signer_fingerprint: String,
    pub target_chain_head_hash: String,
    pub target_last_event_hash: String,
    pub terminal_envelope_digest: String,
    pub terminal_envelope: Value,
}

impl RemoteFollowTerminalEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reservation_attestation_hash: String,
        child_chain_root_id: String,
        child_terminal_thread_id: String,
        terminal_status: String,
        target_site_id: String,
        target_node_signer_fingerprint: String,
        target_chain_head_hash: String,
        target_last_event_hash: String,
        terminal_envelope: Value,
    ) -> anyhow::Result<Self> {
        let terminal_envelope_digest =
            ryeos_state::objects::canonical_value_digest(&terminal_envelope)?;
        let operation_id = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "schema":TERMINAL_SCHEMA,
            "reservation_attestation_hash":reservation_attestation_hash,
            "child_chain_root_id":child_chain_root_id,
            "child_terminal_thread_id":child_terminal_thread_id,
            "terminal_status":terminal_status,
            "target_site_id":target_site_id,
            "target_node_signer_fingerprint":target_node_signer_fingerprint,
            "target_chain_head_hash":target_chain_head_hash,
            "target_last_event_hash":target_last_event_hash,
            "terminal_envelope_digest":terminal_envelope_digest,
        }))?;
        let evidence = Self {
            schema: TERMINAL_SCHEMA.to_owned(),
            operation_id,
            reservation_attestation_hash,
            child_chain_root_id,
            child_terminal_thread_id,
            terminal_status,
            target_site_id,
            target_node_signer_fingerprint,
            target_chain_head_hash,
            target_last_event_hash,
            terminal_envelope_digest,
            terminal_envelope,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != TERMINAL_SCHEMA
            || self.terminal_status == "continued"
            || !matches!(
                self.terminal_status.as_str(),
                "completed" | "failed" | "cancelled" | "killed" | "timed_out"
            )
        {
            bail!("remote follow terminal is not the exact current terminal contract");
        }
        for (label, hash) in [
            ("terminal operation", self.operation_id.as_str()),
            (
                "follow reservation",
                self.reservation_attestation_hash.as_str(),
            ),
            ("target chain head", self.target_chain_head_hash.as_str()),
            ("target last event", self.target_last_event_hash.as_str()),
            ("terminal envelope", self.terminal_envelope_digest.as_str()),
        ] {
            canonical_hash(label, hash)?;
        }
        canonical_fingerprint("target node signer", &self.target_node_signer_fingerprint)?;
        for (label, value) in [
            ("child chain", self.child_chain_root_id.as_str()),
            (
                "child terminal thread",
                self.child_terminal_thread_id.as_str(),
            ),
            ("target site", self.target_site_id.as_str()),
        ] {
            canonical_label(label, value)?;
        }
        if ryeos_state::objects::canonical_value_digest(&self.terminal_envelope)?
            != self.terminal_envelope_digest
        {
            bail!("remote follow terminal envelope changed digest");
        }
        ryeos_runtime::envelope::decode_managed_runtime_terminal_envelope(&self.terminal_envelope)
            .map_err(anyhow::Error::msg)
            .context("decode remote managed terminal envelope")?;
        let expected_id = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "schema":self.schema,
            "reservation_attestation_hash":self.reservation_attestation_hash,
            "child_chain_root_id":self.child_chain_root_id,
            "child_terminal_thread_id":self.child_terminal_thread_id,
            "terminal_status":self.terminal_status,
            "target_site_id":self.target_site_id,
            "target_node_signer_fingerprint":self.target_node_signer_fingerprint,
            "target_chain_head_hash":self.target_chain_head_hash,
            "target_last_event_hash":self.target_last_event_hash,
            "terminal_envelope_digest":self.terminal_envelope_digest,
        }))?;
        if self.operation_id != expected_id {
            bail!("remote follow terminal operation identity changed");
        }
        Ok(())
    }

    pub fn sign_attestation(&self, signer: &dyn Signer) -> anyhow::Result<Attestation> {
        self.validate()?;
        Attestation::unsigned(
            self.target_chain_head_hash.clone(),
            REMOTE_FOLLOW_TERMINAL_CLAIM.to_owned(),
            REMOTE_FOLLOW_TERMINAL_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::to_value(self)?,
        )
        .sign(signer)
    }

    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        if attestation.policy != REMOTE_FOLLOW_TERMINAL_POLICY
            || attestation.claim != REMOTE_FOLLOW_TERMINAL_CLAIM
        {
            bail!("attestation is not a remote follow terminal receipt");
        }
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())?;
        evidence.validate()?;
        if attestation.subject_hash != evidence.target_chain_head_hash
            || attestation.issuer_fingerprint()? != evidence.target_node_signer_fingerprint
        {
            bail!("remote follow terminal signer or subject changed");
        }
        Ok(evidence)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFollowDeliveryJobRole {
    Parent,
    Target,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFollowDeliveryJobOperation {
    pub schema: String,
    pub operation_type: String,
    pub role: RemoteFollowDeliveryJobRole,
    pub operation_id: String,
    pub reservation_attestation_hash: String,
    pub owner_principal: String,
    pub child_chain_root_id: String,
    pub parent_site_id: String,
    pub target_site_id: String,
}

impl RemoteFollowDeliveryJobOperation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        role: RemoteFollowDeliveryJobRole,
        operation_id: String,
        reservation_attestation_hash: String,
        owner_principal: String,
        child_chain_root_id: String,
        parent_site_id: String,
        target_site_id: String,
    ) -> anyhow::Result<Self> {
        let operation = Self {
            schema: JOB_SCHEMA.to_owned(),
            operation_type: REMOTE_FOLLOW_DELIVERY_OPERATION.to_owned(),
            role,
            operation_id,
            reservation_attestation_hash,
            owner_principal,
            child_chain_root_id,
            parent_site_id,
            target_site_id,
        };
        operation.validate()?;
        Ok(operation)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != JOB_SCHEMA || self.operation_type != REMOTE_FOLLOW_DELIVERY_OPERATION {
            bail!("remote follow delivery job is not the exact current contract");
        }
        canonical_hash("terminal operation", &self.operation_id)?;
        canonical_hash("follow reservation", &self.reservation_attestation_hash)?;
        for (label, value) in [
            ("owner", self.owner_principal.as_str()),
            ("child chain", self.child_chain_root_id.as_str()),
            ("parent site", self.parent_site_id.as_str()),
            ("target site", self.target_site_id.as_str()),
        ] {
            canonical_label(label, value)?;
        }
        if self.parent_site_id == self.target_site_id {
            bail!("remote follow delivery job does not cross sites");
        }
        Ok(())
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn from_value(value: Value) -> anyhow::Result<Self> {
        let operation: Self = serde_json::from_value(value)?;
        operation.validate()?;
        Ok(operation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFollowTerminalDeliveryRequest {
    pub operation_id: String,
    pub reservation_attestation_hash: String,
    pub terminal_attestation_hash: String,
    pub child_chain_root_id: String,
    pub target_chain_head_hash: String,
    pub parent_site_id: String,
    pub target_site_id: String,
}

impl RemoteFollowTerminalDeliveryRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, hash) in [
            ("terminal operation", self.operation_id.as_str()),
            (
                "follow reservation",
                self.reservation_attestation_hash.as_str(),
            ),
            (
                "terminal attestation",
                self.terminal_attestation_hash.as_str(),
            ),
            ("target chain head", self.target_chain_head_hash.as_str()),
        ] {
            canonical_hash(label, hash)?;
        }
        for (label, value) in [
            ("child chain", self.child_chain_root_id.as_str()),
            ("parent site", self.parent_site_id.as_str()),
            ("target site", self.target_site_id.as_str()),
        ] {
            canonical_label(label, value)?;
        }
        if self.parent_site_id == self.target_site_id {
            bail!("remote follow terminal request does not cross sites");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFollowTerminalDeliveryResponse {
    pub operation_id: String,
    pub child_chain_root_id: String,
    pub parent_chain_root_id: String,
    pub parent_successor_thread_id: String,
    pub delivery: String,
}

impl RemoteFollowTerminalDeliveryResponse {
    pub fn validate_against(
        &self,
        request: &RemoteFollowTerminalDeliveryRequest,
    ) -> anyhow::Result<()> {
        request.validate()?;
        if self.operation_id != request.operation_id
            || self.child_chain_root_id != request.child_chain_root_id
            || self.delivery != "settled"
        {
            bail!("remote follow terminal response contradicts its request");
        }
        for (label, value) in [
            ("parent chain", self.parent_chain_root_id.as_str()),
            ("parent successor", self.parent_successor_thread_id.as_str()),
        ] {
            canonical_label(label, value)?;
        }
        Ok(())
    }
}

fn canonical_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(value) {
        bail!("{label} is not a canonical SHA-256 digest");
    }
    Ok(())
}

fn canonical_fingerprint(label: &str, value: &str) -> anyhow::Result<()> {
    canonical_hash(label, value)
}

fn canonical_label(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 4096
        || value.trim() != value
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        bail!("{label} is not a bounded canonical label");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_identity_covers_parent_and_child_coordinates() {
        let reservation = RemoteFollowReservationEvidence::new(
            "fp:owner".into(),
            "site:a".into(),
            "1".repeat(64),
            "T-parent-root".into(),
            "2".repeat(64),
            "T-parent".into(),
            "T-parent-successor".into(),
            "follow-key".into(),
            0,
            "worker_execution:test".into(),
            "3".repeat(64),
            "T-child".into(),
            "T-child".into(),
        )
        .unwrap();
        let mut changed = reservation.clone();
        changed.parent_successor_thread_id = "T-other".into();
        assert!(changed.validate().is_err());
    }

    #[test]
    fn terminal_identity_covers_chain_head_and_complete_envelope() {
        let envelope = serde_json::json!({
            "success":true,
            "child_thread_id":"T-child-terminal",
            "status":"completed",
            "result":{"answer":42},
            "outputs":{},
            "warnings":[],
            "cost":null,
        });
        let terminal = RemoteFollowTerminalEvidence::new(
            "1".repeat(64),
            "T-child".into(),
            "T-child-terminal".into(),
            "completed".into(),
            "site:b".into(),
            "2".repeat(64),
            "3".repeat(64),
            "4".repeat(64),
            envelope,
        )
        .unwrap();
        let mut changed = terminal.clone();
        changed.target_chain_head_hash = "5".repeat(64);
        assert!(changed.validate().is_err());
    }

    #[test]
    fn delivery_response_is_bound_to_request() {
        let request = RemoteFollowTerminalDeliveryRequest {
            operation_id: "1".repeat(64),
            reservation_attestation_hash: "2".repeat(64),
            terminal_attestation_hash: "3".repeat(64),
            child_chain_root_id: "T-child".into(),
            target_chain_head_hash: "4".repeat(64),
            parent_site_id: "site:a".into(),
            target_site_id: "site:b".into(),
        };
        let mut response = RemoteFollowTerminalDeliveryResponse {
            operation_id: request.operation_id.clone(),
            child_chain_root_id: request.child_chain_root_id.clone(),
            parent_chain_root_id: "T-parent".into(),
            parent_successor_thread_id: "T-parent-successor".into(),
            delivery: "settled".into(),
        };
        response.validate_against(&request).unwrap();
        response.delivery = "accepted".into();
        assert!(response.validate_against(&request).is_err());
    }
}
