//! The owner of a run's work, stamped into the chain at admission.
//!
//! The first event of every thread (`thread_created`) carries the principal
//! id of the owner of the work, fixed when the work was admitted. Children,
//! detached children, worker turns and continuations copy the chain root's
//! stamp; a chain that arrives from another node never supplies it: the
//! receiver stamps its own at admission and the sender's stamp is evidence
//! only. The stamp is a field of the event payload, never of the thread
//! snapshot, so no snapshot version moves and no existing chain becomes
//! unreadable: an absent stamp only ever means the chain predates stamping,
//! and a cut counts it as another principal's work (fail closed).
//!
//! `requested_by` is the acting key's fingerprint and stays what it is: it
//! says which key acted, never whose work it was.

use std::fmt;

use serde_json::Value;

use crate::principal_id::PrincipalId;

/// The version of the stamp rule. A build that stamps carries this constant;
/// the node records the epoch from which every `thread_created` it appends
/// carries an owner, and membership admission reads that fact before it admits
/// a second principal.
pub const OWNER_STAMP_SCHEMA: u32 = 1;

/// The payload key that holds the owner's principal id (`pid:v1:<hex>`).
pub const OWNER_PAYLOAD_KEY: &str = "owner";

/// Why a payload's owner stamp is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerStampError {
    /// The payload is not a JSON object.
    NotAnObject,
    /// The stamp is present and is not a principal id (a key fingerprint is
    /// refused: a key is never an owner).
    Malformed(String),
    /// A different owner is already stamped.
    Conflict,
    /// No stamp, where one is required.
    Missing,
}

impl fmt::Display for OwnerStampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => write!(f, "owner_stamp_payload_not_an_object"),
            Self::Malformed(why) => write!(f, "owner_stamp_malformed: {why}"),
            Self::Conflict => write!(f, "owner_stamp_conflict: a different owner is stamped"),
            Self::Missing => write!(f, "thread_created_without_owner"),
        }
    }
}

impl std::error::Error for OwnerStampError {}

/// Stamp `owner` into a `thread_created` payload. Stamping the same owner
/// twice is a no-op; a different owner is refused.
pub fn stamp(payload: &mut Value, owner: &PrincipalId) -> Result<(), OwnerStampError> {
    let map = payload
        .as_object_mut()
        .ok_or(OwnerStampError::NotAnObject)?;
    let text = Value::String(owner.as_text());
    match map.get(OWNER_PAYLOAD_KEY) {
        Some(existing) if existing != &text => Err(OwnerStampError::Conflict),
        _ => {
            map.insert(OWNER_PAYLOAD_KEY.to_owned(), text);
            Ok(())
        }
    }
}

/// The stamped owner, or None for a payload that predates stamping.
pub fn read(payload: &Value) -> Result<Option<PrincipalId>, OwnerStampError> {
    match payload.get(OWNER_PAYLOAD_KEY) {
        None => Ok(None),
        Some(Value::String(text)) => text
            .parse::<PrincipalId>()
            .map(Some)
            .map_err(|error| OwnerStampError::Malformed(error.to_string())),
        Some(_) => Err(OwnerStampError::Malformed(
            "the owner is not a string".to_owned(),
        )),
    }
}

/// The stamped owner of a payload from an epoch that stamps: a missing stamp
/// is refused.
pub fn require(payload: &Value) -> Result<PrincipalId, OwnerStampError> {
    read(payload)?.ok_or(OwnerStampError::Missing)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn owner(byte: char) -> PrincipalId {
        format!("pid:v1:{}", byte.to_string().repeat(64))
            .parse()
            .expect("a principal id")
    }

    #[test]
    fn a_stamped_owner_reads_back_and_an_unstamped_payload_reads_as_none() {
        let mut payload = json!({"kind": "directive"});
        assert_eq!(read(&payload), Ok(None));
        stamp(&mut payload, &owner('a')).unwrap();
        assert_eq!(read(&payload), Ok(Some(owner('a'))));
        assert_eq!(payload["kind"], "directive");
    }

    #[test]
    fn stamping_twice_is_a_no_op_and_a_different_owner_is_a_conflict() {
        let mut payload = json!({});
        stamp(&mut payload, &owner('a')).unwrap();
        stamp(&mut payload, &owner('a')).unwrap();
        assert_eq!(
            stamp(&mut payload, &owner('b')),
            Err(OwnerStampError::Conflict)
        );
        assert_eq!(read(&payload), Ok(Some(owner('a'))));
    }

    #[test]
    fn a_key_fingerprint_is_never_an_owner() {
        let payload = json!({"owner": format!("fp:{}", "a".repeat(64))});
        assert!(matches!(read(&payload), Err(OwnerStampError::Malformed(_))));
        let payload = json!({"owner": 7});
        assert!(matches!(read(&payload), Err(OwnerStampError::Malformed(_))));
    }

    #[test]
    fn a_stamping_epoch_refuses_a_missing_stamp_by_its_typed_code() {
        let payload = json!({"kind": "directive"});
        let error = require(&payload).unwrap_err();
        assert_eq!(error, OwnerStampError::Missing);
        assert!(
            error
                .to_string()
                .starts_with("thread_created_without_owner")
        );
        let mut stamped = payload;
        stamp(&mut stamped, &owner('c')).unwrap();
        assert_eq!(require(&stamped), Ok(owner('c')));
    }

    #[test]
    fn only_an_object_payload_can_be_stamped() {
        let mut payload = json!([1, 2]);
        assert_eq!(
            stamp(&mut payload, &owner('a')),
            Err(OwnerStampError::NotAnObject)
        );
    }
}
