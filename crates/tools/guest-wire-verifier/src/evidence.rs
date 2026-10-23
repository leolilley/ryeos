//! Guest evidence schema and its independent verification by the origin role.
//! The guest reports what it observed; this module decides whether those
//! observations agree with everything the origin measured or was handed.
//!
//! What the launcher fields prove and do not prove. `launcher_spec.expected_digest`
//! is the digest of the launcher specification the controller itself derived
//! from its binding, admitted program and guest inputs; it is the controller's
//! expectation, not a value the launcher reported. The launcher's applied
//! report carries an occurrence digest computed from the held launcher process
//! identity, the launcher artifact digest and the launch challenge; it is not
//! derivable from the specification, so it is recorded as
//! `applied_launch.occurrence_digest` and checked only for form. The claim that
//! the launcher accepted the specification therefore rests on a signed,
//! sequence-checked applied-launch frame naming the expected candidate program
//! arriving after Ready, and on the owner exiting successfully; it does not rest
//! on the launcher echoing a specification digest, because it never does.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const EVIDENCE_SCHEMA: &str = "ryeos.guest_wire_evidence.v1";
pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectEvidence {
    pub owner_executable_sha256: String,
    pub manifest_hash: String,
    pub controller_public_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeEvidence {
    pub manifest_hash: String,
    pub public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaDigest {
    pub schema: u32,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LauncherSpecEvidence {
    pub schema: u32,
    pub expected_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedLaunchEvidence {
    pub occurrence_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerExit {
    pub success: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaPins {
    pub guest_inputs: u32,
    pub launcher_spec: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestWireEvidence {
    pub schema: String,
    pub challenge_digest: String,
    pub euid: u32,
    pub subject: SubjectEvidence,
    pub probe: ProbeEvidence,
    pub projection: SchemaDigest,
    pub launcher_spec: LauncherSpecEvidence,
    pub applied_launch: AppliedLaunchEvidence,
    pub exchange_transcript_digest: String,
    pub exchange_frame_count: u64,
    pub applied_launch_receipt_digest: String,
    pub candidate_program_digest: String,
    pub owner_exit: OwnerExit,
    pub member_digests: BTreeMap<String, String>,
    pub schema_pins: SchemaPins,
}

/// Everything the origin knows independently of the guest.
#[derive(Debug, Clone)]
pub struct Expected {
    pub challenge_digest: String,
    pub subject: SubjectEvidence,
    pub member_digests: BTreeMap<String, String>,
    pub guest_inputs_schema: u32,
    pub launcher_spec_schema: u32,
}

impl GuestWireEvidence {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        ryeos_external_execution_contract::canonical_json(self)
    }

    /// Strict, canonical parse followed by every cross-check.
    pub fn parse_and_verify(bytes: &[u8], expected: &Expected) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_EVIDENCE_BYTES,
            "guest wire evidence exceeds its bound"
        );
        let evidence: Self =
            ryeos_external_execution_contract::from_json_slice_strict(bytes, MAX_EVIDENCE_BYTES)?;
        ensure!(
            evidence.canonical_bytes()? == bytes,
            "guest wire evidence is not canonical"
        );
        evidence.verify(expected)?;
        Ok(evidence)
    }

    pub fn verify(&self, expected: &Expected) -> Result<()> {
        ensure!(
            self.schema == EVIDENCE_SCHEMA,
            "unsupported guest evidence schema"
        );
        ensure!(
            self.challenge_digest == expected.challenge_digest,
            "guest evidence answers a different challenge"
        );
        ensure!(
            self.euid == 0,
            "guest probe did not run as the administrator"
        );
        ensure!(
            self.subject == expected.subject,
            "guest evidence names a different subject than the origin measured"
        );
        ensure!(
            self.schema_pins.guest_inputs == expected.guest_inputs_schema
                && self.schema_pins.launcher_spec == expected.launcher_spec_schema
                && self.projection.schema == expected.guest_inputs_schema
                && self.launcher_spec.schema == expected.launcher_spec_schema,
            "guest evidence schema pins differ from the signed configuration"
        );
        for digest in [
            &self.probe.manifest_hash,
            &self.projection.digest,
            &self.launcher_spec.expected_digest,
            &self.applied_launch.occurrence_digest,
            &self.exchange_transcript_digest,
            &self.applied_launch_receipt_digest,
            &self.candidate_program_digest,
        ] {
            ensure!(
                lillux::valid_hash(digest),
                "guest evidence carries a malformed digest"
            );
        }
        ensure!(
            self.exchange_frame_count >= 2,
            "guest exchange transcript is too short to include Ready and an applied launch"
        );
        ensure!(
            self.owner_exit.success,
            "guest owner did not exit successfully"
        );
        ensure!(
            !self.member_digests.is_empty()
                && self.member_digests == expected.member_digests
                && self
                    .member_digests
                    .values()
                    .all(|digest| lillux::valid_hash(digest)),
            "guest member digests differ from the registered archive members"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn expected() -> Expected {
        Expected {
            challenge_digest: "1".repeat(64),
            subject: SubjectEvidence {
                owner_executable_sha256: "2".repeat(64),
                manifest_hash: "3".repeat(64),
                controller_public_root: "ed25519:root".into(),
            },
            member_digests: BTreeMap::from([
                ("launcher".to_owned(), "4".repeat(64)),
                ("supervisor".to_owned(), "5".repeat(64)),
            ]),
            guest_inputs_schema: 5,
            launcher_spec_schema: 4,
        }
    }

    pub fn evidence() -> GuestWireEvidence {
        let expected = expected();
        GuestWireEvidence {
            schema: EVIDENCE_SCHEMA.into(),
            challenge_digest: expected.challenge_digest,
            euid: 0,
            subject: expected.subject,
            probe: ProbeEvidence {
                manifest_hash: "6".repeat(64),
                public_key: "ed25519:probe".into(),
            },
            projection: SchemaDigest {
                schema: 5,
                digest: "7".repeat(64),
            },
            launcher_spec: LauncherSpecEvidence {
                schema: 4,
                expected_digest: "8".repeat(64),
            },
            applied_launch: AppliedLaunchEvidence {
                occurrence_digest: "c".repeat(64),
            },
            exchange_transcript_digest: "9".repeat(64),
            exchange_frame_count: 4,
            applied_launch_receipt_digest: "a".repeat(64),
            candidate_program_digest: "b".repeat(64),
            owner_exit: OwnerExit {
                success: true,
                detail: "code 0".into(),
            },
            member_digests: expected.member_digests,
            schema_pins: SchemaPins {
                guest_inputs: 5,
                launcher_spec: 4,
            },
        }
    }

    #[test]
    fn exact_evidence_round_trips_and_verifies() {
        let bytes = evidence().canonical_bytes().unwrap();
        assert_eq!(
            GuestWireEvidence::parse_and_verify(&bytes, &expected()).unwrap(),
            evidence()
        );
        let mut padded = bytes;
        padded.push(b' ');
        assert!(GuestWireEvidence::parse_and_verify(&padded, &expected()).is_err());
    }

    #[test]
    fn every_disagreement_is_refused() {
        let expected = expected();
        let mutations: Vec<(&str, Box<dyn Fn(&mut GuestWireEvidence)>)> = vec![
            (
                "subject owner",
                Box::new(|e| e.subject.owner_executable_sha256 = "0".repeat(64)),
            ),
            (
                "subject manifest",
                Box::new(|e| e.subject.manifest_hash = "0".repeat(64)),
            ),
            (
                "subject root",
                Box::new(|e| e.subject.controller_public_root = "x".into()),
            ),
            (
                "projection pin",
                Box::new(|e| e.schema_pins.guest_inputs = 4),
            ),
            (
                "launcher pin",
                Box::new(|e| e.schema_pins.launcher_spec = 5),
            ),
            (
                "occurrence digest",
                Box::new(|e| e.applied_launch.occurrence_digest = "zz".into()),
            ),
            ("projection schema", Box::new(|e| e.projection.schema = 4)),
            ("euid", Box::new(|e| e.euid = 1000)),
            (
                "challenge",
                Box::new(|e| e.challenge_digest = "0".repeat(64)),
            ),
            (
                "receipt",
                Box::new(|e| e.applied_launch_receipt_digest = String::new()),
            ),
            (
                "member digest",
                Box::new(|e| {
                    e.member_digests.insert("launcher".into(), "0".repeat(64));
                }),
            ),
            (
                "member absent",
                Box::new(|e| {
                    e.member_digests.clear();
                }),
            ),
            ("owner exit", Box::new(|e| e.owner_exit.success = false)),
            ("schema", Box::new(|e| e.schema = "other".into())),
        ];
        for (name, mutate) in mutations {
            let mut changed = evidence();
            mutate(&mut changed);
            assert!(changed.verify(&expected).is_err(), "{name} was accepted");
        }
    }

    #[test]
    fn unknown_fields_are_refused() {
        let mut value = serde_json::to_value(evidence()).unwrap();
        value["extra"] = serde_json::json!(1);
        let bytes = ryeos_external_execution_contract::canonical_json(&value).unwrap();
        assert!(GuestWireEvidence::parse_and_verify(&bytes, &expected()).is_err());
    }
}
