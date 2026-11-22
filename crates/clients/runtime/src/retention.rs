//! The pending-append retention record.
//!
//! A seat append is written to durable storage before it is sent and cleared
//! only after a verified acknowledgement. The record carries the exact request
//! string so a restart resends identical bytes. The key and the record layout
//! are shared by every client, so a pending append survives a switch between
//! clients only if both agree on them.
//!
//! Two keys name the same record. The web `Store` (browser local storage) uses
//! [`storage_key`], which embeds the percent-encoded session id; that store is
//! private to the page's origin. A native `Store` maps keys to file names, and
//! the session id is a bearer credential that must not appear in a path, so a
//! native host uses [`storage_key_hashed`]. The record itself carries the
//! session id; a loader compares it with the live session after reading, so a
//! hashed key never has to be reversed.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::codec::MAX_SAFE_INTEGER;
use crate::error::RuntimeError;
use crate::platform::Store;

/// Schema tag of the stored record. Any other tag is refused and the stored
/// record is left in place as recovery evidence.
pub const PENDING_APPEND_SCHEMA: &str = "ryeos.ui.seat.pending-append.v3";

const KEY_PREFIX: &str = "ryeos.ui.seat.pending.v2:";

/// Percent-encode like JavaScript's `encodeURIComponent`: every byte except
/// ASCII letters, digits and `- _ . ! ~ * ' ( )` becomes `%XX` (uppercase).
pub fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            write!(out, "%{byte:02X}").expect("String write");
        }
    }
    out
}

/// Storage key of the pending append for one session and presentation.
pub fn storage_key(session_id: &str, presentation_id: &str) -> String {
    format!(
        "{KEY_PREFIX}{}:{}",
        encode_uri_component(session_id),
        encode_uri_component(presentation_id)
    )
}

/// Storage key for a native `Store`: the session id appears only as its
/// SHA-256 (lowercase hex), so the bearer credential never reaches a file
/// name. The presentation id is percent-encoded as in [`storage_key`].
pub fn storage_key_hashed(session_id: &str, presentation_id: &str) -> String {
    format!(
        "{KEY_PREFIX}{:x}:{}",
        Sha256::digest(session_id.as_bytes()),
        encode_uri_component(presentation_id)
    )
}

/// The stored record. Field order is the serialized order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedPendingAppend {
    pub schema_version: String,
    pub session_id: String,
    pub presentation_id: String,
    pub chain_root_id: String,
    pub thread_id: String,
    pub producer_incarnation: String,
    pub operation_id: String,
    /// Decimal digits of a u64; kept textual so no client rounds it.
    pub first_engine_seq: String,
    pub last_engine_seq: String,
    /// A positive integer no larger than 2^53 - 1.
    pub event_count: u64,
    pub payload_digest: String,
    /// The exact request body that was or will be sent.
    pub encoded_request: String,
}

fn is_canonical_decimal(text: &str) -> bool {
    match text.as_bytes() {
        [b'0'] => true,
        [b'1'..=b'9', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    }
}

fn string_field(record: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    record.get(name)?.as_str().map(str::to_owned)
}

fn sequence_field(record: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    string_field(record, name).filter(|text| is_canonical_decimal(text))
}

fn event_count_field(record: &serde_json::Map<String, Value>) -> Option<u64> {
    let number = record.get("event_count")?.as_number()?;
    let count = if let Some(count) = number.as_u64() {
        count
    } else {
        let float = number.as_f64()?;
        if float.fract() != 0.0 || float <= 0.0 || float > MAX_SAFE_INTEGER as f64 {
            return None;
        }
        float as u64
    };
    (1..=MAX_SAFE_INTEGER).contains(&count).then_some(count)
}

impl RetainedPendingAppend {
    /// Parse a stored value strictly. Unknown extra fields are ignored; every
    /// listed field must have its exact type.
    pub fn parse(stored: &str) -> Result<Self, RuntimeError> {
        let value: Value = serde_json::from_str(stored)
            .map_err(|error| RuntimeError::RetentionMalformed(error.to_string()))?;
        Self::from_value(&value).ok_or(RuntimeError::RetentionInvalidSchema)
    }

    fn from_value(value: &Value) -> Option<Self> {
        let record = value.as_object()?;
        if record.get("schema_version")?.as_str()? != PENDING_APPEND_SCHEMA {
            return None;
        }
        Some(Self {
            schema_version: PENDING_APPEND_SCHEMA.to_owned(),
            session_id: string_field(record, "session_id")?,
            presentation_id: string_field(record, "presentation_id")?,
            chain_root_id: string_field(record, "chain_root_id")?,
            thread_id: string_field(record, "thread_id")?,
            producer_incarnation: string_field(record, "producer_incarnation")?,
            operation_id: string_field(record, "operation_id")?,
            first_engine_seq: sequence_field(record, "first_engine_seq")?,
            last_engine_seq: sequence_field(record, "last_engine_seq")?,
            event_count: event_count_field(record)?,
            payload_digest: string_field(record, "payload_digest")?,
            encoded_request: string_field(record, "encoded_request")?,
        })
    }

    /// The text written to storage.
    pub fn to_stored(&self) -> Result<String, RuntimeError> {
        Ok(serde_json::to_string(self)?)
    }
}

/// Read the retained append at `key`; `None` when nothing is stored.
pub fn load(store: &impl Store, key: &str) -> Result<Option<RetainedPendingAppend>, RuntimeError> {
    store
        .get(key)
        .map(|stored| RetainedPendingAppend::parse(&stored))
        .transpose()
}

/// Write the record. Call before sending; a refused write must fence new
/// mutations.
pub fn retain(
    store: &impl Store,
    key: &str,
    record: &RetainedPendingAppend,
) -> Result<(), RuntimeError> {
    store.set(key, &record.to_stored()?)
}

/// Remove the record unconditionally.
pub fn clear(store: &impl Store, key: &str) {
    store.remove(key);
}

/// Remove the record only if it is still the one that was acknowledged: same
/// operation and producer and, when given, the same request bytes. A record
/// that changed meanwhile is an error and stays in place.
pub fn clear_matching(
    store: &impl Store,
    key: &str,
    operation_id: &str,
    producer_incarnation: &str,
    expected_encoded_request: Option<&str>,
) -> Result<(), RuntimeError> {
    let Some(retained) = load(store, key)? else {
        return Ok(());
    };
    let matches = retained.operation_id == operation_id
        && retained.producer_incarnation == producer_incarnation
        && expected_encoded_request.is_none_or(|expected| retained.encoded_request == expected);
    if !matches {
        return Err(RuntimeError::Fatal(
            "pending seat append changed during acknowledgement reconciliation".to_owned(),
        ));
    }
    store.remove(key);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::MemoryStore;

    fn record() -> RetainedPendingAppend {
        RetainedPendingAppend {
            schema_version: PENDING_APPEND_SCHEMA.to_owned(),
            session_id: "s".into(),
            presentation_id: "p".into(),
            chain_root_id: "c".into(),
            thread_id: "t".into(),
            producer_incarnation: "a".repeat(64),
            operation_id: "op".into(),
            first_engine_seq: "0".into(),
            last_engine_seq: "0".into(),
            event_count: 1,
            payload_digest: "d".into(),
            encoded_request: "{}".into(),
        }
    }

    #[test]
    fn encode_matches_javascript_unreserved_set() {
        assert_eq!(encode_uri_component("a b/c:d"), "a%20b%2Fc%3Ad");
        assert_eq!(encode_uri_component("-_.!~*'()"), "-_.!~*'()");
    }

    #[test]
    fn hashed_key_hides_the_session_id() {
        let key = storage_key_hashed("secret-session", "p/1");
        assert!(!key.contains("secret-session"));
        assert!(key.starts_with("ryeos.ui.seat.pending.v2:"));
        assert!(key.ends_with(":p%2F1"));
        assert_ne!(key, storage_key_hashed("other-session", "p/1"));
    }

    #[test]
    fn retain_load_and_clear_round_trip() {
        let store = MemoryStore::new();
        let key = storage_key("s", "p");
        assert_eq!(load(&store, &key).unwrap(), None);
        retain(&store, &key, &record()).unwrap();
        assert_eq!(load(&store, &key).unwrap(), Some(record()));
        clear_matching(&store, &key, "op", &"a".repeat(64), Some("{}")).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn changed_record_is_not_cleared() {
        let store = MemoryStore::new();
        let key = storage_key("s", "p");
        retain(&store, &key, &record()).unwrap();
        assert!(clear_matching(&store, &key, "other", &"a".repeat(64), None).is_err());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn refused_write_is_reported() {
        let store = MemoryStore::new();
        store.fail_writes(true);
        assert!(retain(&store, "k", &record()).is_err());
    }
}
