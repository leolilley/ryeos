//! Deserializer that accepts either a scalar `T` or a `Vec<T>` and
//! always returns a `Vec<T>`.
//!
//! Use this on handler `Request` struct fields whose schema is
//! `string[]` (or any homogeneous array) so the CLI can pass a single
//! value naturally:
//!
//!   --scopes ryeos.execute.service.bundle/install
//!
//! produces the JSON scalar `"ryeos..."`, which deserializes to a
//! one-element `Vec<String>`. Repeated flags accumulate into an array
//! via the [`crate::arg_binder`] and deserialize directly.
//!
//! ```
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct Req {
//!     #[serde(deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize")]
//!     scopes: Vec<String>,
//! }
//!
//! let scalar: Req = serde_json::from_str(r#"{"scopes":"a"}"#).unwrap();
//! assert_eq!(scalar.scopes, vec!["a"]);
//! let vector: Req = serde_json::from_str(r#"{"scopes":["a","b"]}"#).unwrap();
//! assert_eq!(vector.scopes, vec!["a", "b"]);
//! ```

use serde::de::{Deserialize, DeserializeOwned, Deserializer};
use serde_json::Value;

/// Custom `#[serde(deserialize_with = ...)]` that accepts a scalar
/// `T` or a `Vec<T>`. The scalar form is wrapped in a single-element
/// `Vec`; the vec form is returned unchanged.
///
/// `T` requires [`DeserializeOwned`] because we round-trip through
/// `serde_json::Value`, which owns its data.
pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    // Buffer through `Value` so we can probe the shape. Handler payloads
    // are small JSON objects, so this is not a hot path.
    let v = Value::deserialize(deserializer)?;
    match v {
        Value::Array(_) => serde_json::from_value(v).map_err(serde::de::Error::custom),
        _ => {
            let one: T = serde_json::from_value(v).map_err(serde::de::Error::custom)?;
            Ok(vec![one])
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Req {
        #[serde(deserialize_with = "super::deserialize")]
        scopes: Vec<String>,
    }

    #[test]
    fn scalar_becomes_one_element_vec() {
        let r: Req = serde_json::from_str(r#"{"scopes":"a"}"#).unwrap();
        assert_eq!(r.scopes, vec!["a".to_string()]);
    }

    #[test]
    fn array_is_passed_through() {
        let r: Req = serde_json::from_str(r#"{"scopes":["a","b"]}"#).unwrap();
        assert_eq!(r.scopes, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn empty_array_is_empty_vec() {
        let r: Req = serde_json::from_str(r#"{"scopes":[]}"#).unwrap();
        assert!(r.scopes.is_empty());
    }

    #[test]
    fn non_string_scalar_errors_with_type_mismatch() {
        let r: Result<Req, _> = serde_json::from_str(r#"{"scopes":42}"#);
        assert!(r.is_err(), "integer is not a string, must reject");
    }
}
