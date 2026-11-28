//! The secret floor under what a node tells a caller that is not its operator.
//!
//! A value the node holds in its secret store must never reach another key
//! through a service answer. [`SecretFloor`] carries those values for one
//! decision: an answer that contains one, byte for byte (raw, JSON-escaped or
//! ASCII-escaped), is refused whole with a typed code that names neither the
//! value nor where it was. The operator's answers never pass through it.
//! The match is exact: it does not see an encoded, hashed, split or shortened
//! secret, nor one the node does not hold.

use serde_json::Value;
use zeroize::Zeroizing;

use crate::handler_error::{HandlerError, typed_refusal};
use crate::state::AppState;

/// The stable code of an answer withheld by the floor.
pub const SECRET_FLOOR_WITHHELD: &str = "secret_floor_withheld";

/// The shortest secret value the floor looks for, in bytes. A shorter value
/// is not matched at all, because it would match ordinary text by chance.
pub const MINIMUM_MATCHED_BYTES: usize =
    ryeos_runtime::workload_client::MIN_COLLECT_SECRET_FLOOR_BYTES;

const WITHHELD_SENTENCE: &str = "the answer was withheld from this caller";

/// The secret values the node holds, for one floor decision.
pub struct SecretFloor {
    values: Vec<Zeroizing<String>>,
}

impl SecretFloor {
    /// The floor over every secret value the node holds: the sealed operator
    /// store and the provider environment captured at startup. A store that
    /// cannot be read is an error, so the caller fails closed.
    pub fn for_node(state: &AppState) -> anyhow::Result<Self> {
        let values = crate::vault::node_held_secret_values(&state.secret_sources())?;
        Ok(Self { values })
    }

    /// A floor over the given values.
    pub fn from_values(values: Vec<String>) -> Self {
        Self {
            values: values.into_iter().map(Zeroizing::new).collect(),
        }
    }

    /// The value itself when it holds none of the floor's secrets, otherwise
    /// the typed refusal. The refusal names no value and no place.
    pub fn withhold(&self, value: Value) -> Result<Value, HandlerError> {
        if ryeos_runtime::workload_client::value_contains_secret(&value, &self.values) {
            return Err(typed_refusal(SECRET_FLOOR_WITHHELD, 403, WITHHELD_SENTENCE));
        }
        Ok(value)
    }

    /// Replace every occurrence of a floor secret in `value` with the fixed
    /// marker; whether anything was replaced.
    pub fn mask(&self, value: &mut Value) -> bool {
        ryeos_runtime::workload_client::mask_secrets_in_place(value, &self.values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-live-0123456789";

    #[test]
    fn an_answer_holding_a_secret_is_refused_without_naming_it() {
        let floor = SecretFloor::from_values(vec![SECRET.to_owned()]);
        let refused = floor
            .withhold(serde_json::json!({"a": [format!("x {SECRET} y")]}))
            .unwrap_err();
        match &refused {
            HandlerError::Structured { code, status, body } => {
                assert_eq!((code.as_str(), *status), (SECRET_FLOOR_WITHHELD, 403));
                assert!(!body.to_string().contains(SECRET), "{body}");
            }
            other => panic!("not a typed refusal: {other:?}"),
        }
        assert!(!format!("{refused:?}").contains(SECRET));
    }

    #[test]
    fn a_clean_answer_passes_unchanged() {
        let floor = SecretFloor::from_values(vec![SECRET.to_owned()]);
        let answer = serde_json::json!({"id": "x", "n": 3});
        assert_eq!(floor.withhold(answer.clone()).unwrap(), answer);
        assert_eq!(
            SecretFloor::from_values(vec![])
                .withhold(answer.clone())
                .unwrap(),
            answer
        );
    }

    #[test]
    fn masking_replaces_the_secret_and_reports_it() {
        let floor = SecretFloor::from_values(vec![SECRET.to_owned()]);
        let mut value = serde_json::json!({"a": format!("x {SECRET}")});
        assert!(floor.mask(&mut value));
        assert_eq!(
            value,
            serde_json::json!({"a": format!("x {}", ryeos_runtime::workload_client::SECRET_FLOOR_MARKER)})
        );
        assert!(!floor.mask(&mut value));
    }
}
