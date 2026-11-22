//! Exact JSON for the seat protocol.
//!
//! The daemon's value domain is integers in `[-2^63, 2^64-1]` plus finite f64.
//! `serde_json::Value` already keeps integers exact as `i64`/`u64` (the
//! workspace does not enable arbitrary precision), so this module adds the
//! protocol's refusals around it:
//!
//! * [`encode_exact`] writes a request body, refusing integral floats beyond
//!   the safe range and spelling negative zero as `0`.
//! * [`decode_exact`] is a strict RFC 8259 parser: duplicate object keys,
//!   leading zeros, bad escapes, trailing input, out-of-domain integers and
//!   nesting beyond 128 containers are refused, with offsets counted in
//!   UTF-16 code units.
//! * [`seat_payload_digest`] is the shared commitment, re-exported so every
//!   client digests with the daemon's own function.
//!
//! Differences from a JavaScript encoder that are inherent to the value type:
//! `serde_json::Value` objects do not preserve insertion order (they are
//! sorted unless some crate in the build enables `preserve_order`), integral
//! floats such as `1.0` stay floats (JavaScript cannot tell them from `1`),
//! and float spelling follows ryu, which switches to exponent form earlier
//! than JavaScript does. Digests are unaffected: they commit to f64 bits.

use serde_json::{Map, Number, Value};

use crate::error::RuntimeError;

pub use ryeos_ui_presentation_contract::payload_digest::{
    SeatPayloadDigestError, seat_payload_digest,
};

/// Maximum container nesting accepted by [`decode_exact`].
pub const MAX_JSON_DEPTH: usize = 128;

/// Refusal text for an integer outside `[-2^63, 2^64-1]`.
pub const INTEGER_DOMAIN_MESSAGE: &str = "JSON integer is outside the daemon i64/u64 value domain";

/// Largest integer a JavaScript number holds exactly (2^53 - 1).
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// An exact integer value from its decimal digits.
pub fn exact_integer(decimal: &str) -> Result<Value, RuntimeError> {
    if !decimal.starts_with('+') {
        if let Ok(value) = decimal.parse::<i64>() {
            return Ok(Value::from(value));
        }
        if let Ok(value) = decimal.parse::<u64>() {
            return Ok(Value::from(value));
        }
    }
    Err(RuntimeError::Decode(INTEGER_DOMAIN_MESSAGE.to_owned()))
}

/// A JSON number from a finite f64.
pub fn finite_number(value: f64) -> Result<Value, RuntimeError> {
    Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| RuntimeError::Decode("JSON body contains a non-finite number".to_owned()))
}

/// Replace every f64 negative zero with the integer zero. The wire encoder
/// does this implicitly; callers that digest a value they also send must
/// normalise first so both sides commit to the same bytes.
pub fn normalize_negative_zero(value: &mut Value) {
    match value {
        Value::Number(number) => {
            if number
                .as_f64()
                .is_some_and(|f| f == 0.0 && f.is_sign_negative())
                && !number.is_i64()
                && !number.is_u64()
            {
                *value = Value::from(0_u64);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_negative_zero),
        Value::Object(map) => map.values_mut().for_each(normalize_negative_zero),
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

/// Encode a request body as compact JSON.
pub fn encode_exact(value: &Value) -> Result<String, RuntimeError> {
    let mut out = String::new();
    write_exact(value, &mut out)?;
    Ok(out)
}

fn write_exact(value: &Value, out: &mut String) -> Result<(), RuntimeError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::String(text) => out.push_str(&serde_json::to_string(text)?),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                out.push_str(&number.to_string());
            } else {
                let float = number.as_f64().ok_or_else(|| {
                    RuntimeError::Decode("JSON body contains a non-finite number".to_owned())
                })?;
                if !float.is_finite() {
                    return Err(RuntimeError::Decode(
                        "JSON body contains a non-finite number".to_owned(),
                    ));
                }
                if float.fract() == 0.0 && float.abs() > MAX_SAFE_INTEGER as f64 {
                    return Err(RuntimeError::Decode(
                        "JSON body contains an unsafe integer; use BigInt for exact identity"
                            .to_owned(),
                    ));
                }
                if float == 0.0 && float.is_sign_negative() {
                    out.push('0');
                } else {
                    out.push_str(&number.to_string());
                }
            }
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                write_exact(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key)?);
                out.push(':');
                write_exact(item, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// A permissive exact u64 reader for stream frames: a JSON integer, or a
/// canonical decimal string (`0` or digits without a leading zero).
pub fn exact_u64(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) => {
            let canonical = text == "0"
                || (!text.starts_with('0')
                    && !text.is_empty()
                    && text.bytes().all(|byte| byte.is_ascii_digit()));
            if canonical { text.parse().ok() } else { None }
        }
        Value::Number(number) => number.as_u64(),
        _ => None,
    }
}

/// Parse JSON strictly: duplicate object keys, nesting beyond
/// [`MAX_JSON_DEPTH`] containers, non-finite numbers, out-of-domain integers
/// and trailing input are refused. `serde_json::from_str` keeps the last of
/// duplicate keys silently, so this is a separate pass.
pub fn decode_strict(text: &str) -> Result<Value, RuntimeError> {
    decode_exact_with_depth(text, MAX_JSON_DEPTH)
}

/// Parse JSON strictly, with the daemon's nesting limit.
pub fn decode_exact(text: &str) -> Result<Value, RuntimeError> {
    decode_strict(text)
}

/// Parse JSON strictly with an explicit container nesting limit. Hosts that
/// read trusted fixtures deeper than the wire limit use this; request and
/// response bodies use [`decode_exact`].
pub fn decode_exact_with_depth(text: &str, max_depth: usize) -> Result<Value, RuntimeError> {
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
        depth: 0,
        max_depth,
    };
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.pos != parser.bytes.len() {
        return parser.fail("trailing input");
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
    max_depth: usize,
}

fn is_digit(byte: Option<&u8>) -> bool {
    byte.is_some_and(u8::is_ascii_digit)
}

impl Parser<'_> {
    fn fail<T>(&self, message: &str) -> Result<T, RuntimeError> {
        self.fail_at(self.pos, message)
    }

    fn fail_at<T>(&self, at: usize, message: &str) -> Result<T, RuntimeError> {
        let mut end = at.min(self.text.len());
        while !self.text.is_char_boundary(end) {
            end -= 1;
        }
        let offset = self.text[..end].encode_utf16().count();
        Err(RuntimeError::Decode(format!(
            "invalid JSON at offset {offset}: {message}"
        )))
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Value, RuntimeError> {
        self.skip_whitespace();
        match self.bytes.get(self.pos).copied() {
            Some(b'"') => self.string().map(Value::String),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => self.fail("expected a JSON value"),
        }
    }

    fn literal(&mut self, token: &str, value: Value) -> Result<Value, RuntimeError> {
        if !self.text[self.pos..].starts_with(token) {
            return self.fail(&format!("expected {token}"));
        }
        self.pos += token.len();
        Ok(value)
    }

    fn enter(&mut self) -> Result<(), RuntimeError> {
        self.pos += 1;
        self.depth += 1;
        if self.depth > self.max_depth {
            return self.fail("container nesting exceeds the daemon JSON limit");
        }
        Ok(())
    }

    fn number(&mut self) -> Result<Value, RuntimeError> {
        let start = self.pos;
        let mut end = start;
        if self.bytes.get(end) == Some(&b'-') {
            end += 1;
        }
        match self.bytes.get(end) {
            Some(b'0') => end += 1,
            Some(b'1'..=b'9') => {
                while is_digit(self.bytes.get(end)) {
                    end += 1;
                }
            }
            _ => return self.fail("invalid number"),
        }
        let mut is_float = false;
        if self.bytes.get(end) == Some(&b'.') && is_digit(self.bytes.get(end + 1)) {
            is_float = true;
            end += 1;
            while is_digit(self.bytes.get(end)) {
                end += 1;
            }
        }
        if matches!(self.bytes.get(end), Some(b'e' | b'E')) {
            let mut probe = end + 1;
            if matches!(self.bytes.get(probe), Some(b'+' | b'-')) {
                probe += 1;
            }
            if is_digit(self.bytes.get(probe)) {
                is_float = true;
                while is_digit(self.bytes.get(probe)) {
                    probe += 1;
                }
                end = probe;
            }
        }
        self.pos = end;
        let token = &self.text[start..end];
        if !is_float {
            if token == "-0" {
                return finite_number(-0.0);
            }
            return match exact_integer(token) {
                Ok(value) => Ok(value),
                Err(_) => self.fail("integer is outside the daemon i64/u64 value domain"),
            };
        }
        match token.parse::<f64>() {
            Ok(float) if float.is_finite() => finite_number(float),
            _ => self.fail("number is outside the finite f64 domain"),
        }
    }

    fn string(&mut self) -> Result<String, RuntimeError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let run_start = self.pos;
            while let Some(&byte) = self.bytes.get(self.pos) {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(&self.text[run_start..self.pos]);
            match self.bytes.get(self.pos).copied() {
                None => return self.fail("unterminated string"),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return self.fail("unescaped control character in string"),
            }
        }
    }

    fn hex4(&self, at: usize) -> Option<u16> {
        let digits = self.text.get(at..at + 4)?;
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        u16::from_str_radix(digits, 16).ok()
    }

    fn escape(&mut self, out: &mut String) -> Result<(), RuntimeError> {
        let simple = match self.bytes.get(self.pos).copied() {
            Some(b'u') => {
                let Some(unit) = self.hex4(self.pos + 1) else {
                    return self.fail("invalid Unicode escape");
                };
                self.pos += 5;
                let scalar = if (0xD800..0xDC00).contains(&unit) {
                    let low = (self.bytes.get(self.pos) == Some(&b'\\')
                        && self.bytes.get(self.pos + 1) == Some(&b'u'))
                    .then(|| self.hex4(self.pos + 2))
                    .flatten()
                    .filter(|low| (0xDC00..0xE000).contains(low));
                    let Some(low) = low else {
                        return self.fail("unpaired surrogate escape is not representable");
                    };
                    self.pos += 6;
                    0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
                } else if (0xDC00..0xE000).contains(&unit) {
                    return self.fail("unpaired surrogate escape is not representable");
                } else {
                    u32::from(unit)
                };
                let Some(character) = char::from_u32(scalar) else {
                    return self.fail("invalid Unicode escape");
                };
                out.push(character);
                return Ok(());
            }
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'/') => '/',
            Some(b'b') => '\u{8}',
            Some(b'f') => '\u{c}',
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            _ => return self.fail("invalid string escape"),
        };
        self.pos += 1;
        out.push(simple);
        Ok(())
    }

    fn array(&mut self) -> Result<Value, RuntimeError> {
        self.enter()?;
        self.skip_whitespace();
        let mut items = Vec::new();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_whitespace();
            match self.bytes.get(self.pos) {
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => {
                    self.pos += 1;
                    self.skip_whitespace();
                }
                _ => return self.fail("expected ',' or ']'"),
            }
        }
        self.depth -= 1;
        Ok(Value::Array(items))
    }

    fn object(&mut self) -> Result<Value, RuntimeError> {
        self.enter()?;
        self.skip_whitespace();
        let mut map = Map::new();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Object(map));
        }
        loop {
            if self.bytes.get(self.pos) != Some(&b'"') {
                return self.fail("expected object key");
            }
            let key = self.string()?;
            if map.contains_key(&key) {
                let quoted = serde_json::to_string(&key)?;
                return self.fail(&format!("duplicate object key {quoted}"));
            }
            self.skip_whitespace();
            if self.bytes.get(self.pos) != Some(&b':') {
                return self.fail("expected ':'");
            }
            self.pos += 1;
            let item = self.value()?;
            map.insert(key, item);
            self.skip_whitespace();
            match self.bytes.get(self.pos) {
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => {
                    self.pos += 1;
                    self.skip_whitespace();
                }
                _ => return self.fail("expected ',' or '}'"),
            }
        }
        self.depth -= 1;
        Ok(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_keep_every_digit() {
        let value = decode_exact(r#"{"a":18446744073709551615,"b":-9223372036854775808}"#).unwrap();
        assert_eq!(value["a"].as_u64(), Some(u64::MAX));
        assert_eq!(value["b"].as_i64(), Some(i64::MIN));
        assert_eq!(
            encode_exact(&value).unwrap(),
            r#"{"a":18446744073709551615,"b":-9223372036854775808}"#
        );
    }

    #[test]
    fn offsets_count_utf16_units() {
        let error = decode_exact("[\"\u{1F600}\" x]").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid JSON at offset 6: expected ',' or ']'"
        );
    }

    #[test]
    fn negative_zero_normalizes() {
        let mut value = finite_number(-0.0).unwrap();
        normalize_negative_zero(&mut value);
        assert_eq!(value, Value::from(0_u64));
    }
}
