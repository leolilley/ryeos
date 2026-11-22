//! The existing exact seat payload commitment, shared by UI clients and owners.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("seat digest contains a number outside the protocol value domain")]
pub struct SeatPayloadDigestError;

pub fn seat_payload_digest(value: &Value) -> Result<String, SeatPayloadDigestError> {
    fn encode(value: &Value, output: &mut String) -> Result<(), SeatPayloadDigestError> {
        match value {
            Value::Null => output.push('n'),
            Value::Bool(false) => output.push('f'),
            Value::Bool(true) => output.push('t'),
            Value::String(value) => {
                write!(output, "s{}:{value}", value.len()).expect("String write");
            }
            Value::Number(number) => {
                if let Some(value) = number.as_i64() {
                    write!(output, "i{value};").expect("String write");
                } else if let Some(value) = number.as_u64() {
                    write!(output, "i{value};").expect("String write");
                } else if let Some(value) = number.as_f64() {
                    write!(output, "d{:016x};", value.to_bits()).expect("String write");
                } else {
                    return Err(SeatPayloadDigestError);
                }
            }
            Value::Array(values) => {
                write!(output, "a{}:{{", values.len()).expect("String write");
                for value in values {
                    encode(value, output)?;
                }
                output.push('}');
            }
            Value::Object(values) => {
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort();
                write!(output, "o{}:{{", keys.len()).expect("String write");
                for key in keys {
                    encode(&Value::String(key.clone()), output)?;
                    encode(&values[key], output)?;
                }
                output.push('}');
            }
        }
        Ok(())
    }
    let mut typed = String::from("ryeos.seat.payload.v1|");
    encode(value, &mut typed)?;
    Ok(format!("{:x}", Sha256::digest(typed.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commitment_preserves_integer_identity_and_key_order_independence() {
        let a: Value = serde_json::from_str(r#"{"z":18446744073709551615,"a":1}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":1,"z":18446744073709551615}"#).unwrap();
        assert_eq!(seat_payload_digest(&a), seat_payload_digest(&b));
        assert_ne!(
            seat_payload_digest(&a),
            seat_payload_digest(&serde_json::json!({"a":1,"z":18446744073709551614_u64}))
        );
    }
}
