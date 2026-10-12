//! Exact UUID-format commitment encoding shared by presentation data contracts.
//! Neither a valid shape nor an exact commitment is an authority credential.

use serde::{Deserialize, Serialize};

/// Exact frozen transport batch, independent of the renderer and the daemon.
/// Its commitment is an idempotency identity, never a writer credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSeatAppendIdentity {
    pub presentation_id: String,
    pub chain_root_id: String,
    pub thread_id: String,
    pub producer_incarnation: String,
    pub first_engine_seq: u64,
    pub last_engine_seq: u64,
    pub event_count: u64,
    pub payload_digest: String,
}

impl UiSeatAppendIdentity {
    pub fn derive_identity(&self) -> Result<String, &'static str> {
        let hash = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if !canonical_operation_id(&self.presentation_id)
            || self.chain_root_id.is_empty()
            || self.thread_id.is_empty()
            || !hash(&self.producer_incarnation)
            || !hash(&self.payload_digest)
            || self.event_count == 0
            || self.first_engine_seq.checked_add(self.event_count - 1) != Some(self.last_engine_seq)
            || self.last_engine_seq.checked_add(1).is_none()
        {
            return Err("seat append identity has an invalid exact interval or subject");
        }
        #[derive(Serialize)]
        struct AppendIdentity<'a> {
            domain: &'static str,
            #[serde(flatten)]
            batch: &'a UiSeatAppendIdentity,
        }
        identity_from_serializable(&AppendIdentity {
            domain: "ryeos.ui.seat-append.v2",
            batch: self,
        })
    }
}

pub fn canonical_operation_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

pub fn identity_from_serializable(value: &impl Serialize) -> Result<String, &'static str> {
    let value = serde_json::to_value(value).map_err(|_| "identity preimage encoding failed")?;
    let commitment = crate::payload_digest::seat_payload_digest(&value)
        .map_err(|_| "identity commitment failed")?;
    identity_from_commitment(&commitment).ok_or("identity UUID encoding failed")
}

fn identity_from_commitment(commitment: &str) -> Option<String> {
    let mut bytes = commitment.get(..32)?.as_bytes().to_vec();
    // UUID custom/version-8 namespace; variant 10xx. These are display bytes,
    // not a bearer token or an additional authority coordinate.
    bytes[12] = b'8';
    bytes[16] = b"89ab"[((bytes[16] as char).to_digit(16)? as usize) & 3];
    let value = String::from_utf8(bytes).ok()?;
    Some(format!(
        "{}-{}-{}-{}-{}",
        &value[..8],
        &value[8..12],
        &value[12..16],
        &value[16..20],
        &value[20..]
    ))
}

#[cfg(test)]
mod tests {
    use super::UiSeatAppendIdentity;

    #[test]
    fn exact_append_identity_binds_subject_interval_and_payload() {
        let original = UiSeatAppendIdentity {
            presentation_id: "22222222-2222-4222-8222-222222222222".into(),
            chain_root_id: "root".into(),
            thread_id: "seat".into(),
            producer_incarnation: "ab".repeat(32),
            first_engine_seq: 30,
            last_engine_seq: 31,
            event_count: 2,
            payload_digest: "cd".repeat(32),
        };
        let id = original.derive_identity().unwrap();
        assert_eq!(original.clone().derive_identity().unwrap(), id);
        let mut changed = original.clone();
        changed.chain_root_id = "another-root".into();
        assert_ne!(changed.derive_identity().unwrap(), id);
        changed = original.clone();
        changed.payload_digest = "ef".repeat(32);
        assert_ne!(changed.derive_identity().unwrap(), id);
        changed = original;
        changed.last_engine_seq += 1;
        assert!(changed.derive_identity().is_err());
    }
}
