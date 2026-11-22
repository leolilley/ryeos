//! Shared loader and typed-value conversion for the vector tests.
//!
//! The vector files tag values JSON cannot hold: `{"$int": "<digits>"}` is an
//! exact integer, `{"$number": "..."}` a JavaScript number special, `{"$utf16":
//! [units]}` a string by code units, and `$undefined` / `$cycle` /
//! `$nonrecord` / `$entries` values with no `serde_json::Value` form.

#![allow(dead_code)]

pub mod sim;

use std::path::PathBuf;

use ryeos_client_runtime::RuntimeError;
use ryeos_client_runtime::codec;
use serde_json::{Map, Value};

/// A tagged input that cannot become a `Value`.
#[derive(Debug)]
pub enum Untyped {
    /// The protocol refuses it; carries the refusal.
    Refused(RuntimeError),
    /// `serde_json::Value` cannot express it (lone surrogate, cycle, ...).
    Unrepresentable(String),
}

/// Load a vector file. The files nest deeper than the wire limit, so the
/// loader raises it.
pub fn load(name: &str) -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/vectors")
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    match codec::decode_exact_with_depth(&text, 4096) {
        Ok(Value::Array(cases)) => cases,
        Ok(_) => panic!("{name}.json is not an array"),
        Err(error) => panic!("{name}.json: {error}"),
    }
}

/// Cases of one `group` of a grouped vector file.
pub fn group(name: &str, group: &str) -> Vec<Value> {
    load(name)
        .into_iter()
        .filter(|case| case["group"] == group)
        .collect()
}

pub fn name_of(case: &Value) -> &str {
    case["name"].as_str().expect("case has a name")
}

fn tag(map: &Map<String, Value>) -> Option<(&str, &Value)> {
    if map.len() != 1 {
        return None;
    }
    let (key, value) = map.iter().next()?;
    key.starts_with('$').then_some((key.as_str(), value))
}

fn from_utf16(units: &Value) -> Result<String, Untyped> {
    let units: Vec<u16> = units
        .as_array()
        .expect("$utf16 holds an array")
        .iter()
        .map(|unit| u16::try_from(unit.as_u64().expect("code unit")).expect("code unit range"))
        .collect();
    String::from_utf16(&units).map_err(|_| Untyped::Unrepresentable("lone surrogate".to_owned()))
}

/// Convert a tagged vector value into a plain `Value`.
pub fn typed(value: &Value) -> Result<Value, Untyped> {
    match value {
        Value::Object(map) => {
            if let Some((key, inner)) = tag(map) {
                return match key {
                    "$int" => codec::exact_integer(inner.as_str().expect("$int digits"))
                        .map_err(Untyped::Refused),
                    "$number" => match inner.as_str().expect("$number text") {
                        "NaN" => codec::finite_number(f64::NAN).map_err(Untyped::Refused),
                        "-0" => codec::finite_number(-0.0).map_err(Untyped::Refused),
                        text => codec::finite_number(text.parse().expect("number text"))
                            .map_err(Untyped::Refused),
                    },
                    "$utf16" => from_utf16(inner).map(Value::String),
                    "$entries" => {
                        let mut out = Map::new();
                        for pair in inner.as_array().expect("$entries pairs") {
                            let pair = pair.as_array().expect("entry pair");
                            let key = match &pair[0] {
                                Value::String(text) => text.clone(),
                                other => match typed(other)? {
                                    Value::String(text) => text,
                                    _ => panic!("entry key is not a string"),
                                },
                            };
                            out.insert(key, typed(&pair[1])?);
                        }
                        Ok(Value::Object(out))
                    }
                    other => Err(Untyped::Unrepresentable(other.to_owned())),
                };
            }
            let mut out = Map::new();
            for (key, item) in map {
                out.insert(key.clone(), typed(item)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .iter()
            .map(typed)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

/// Equality that tolerates an integer-valued token read as an integer on one
/// side and as an f64 on the other (a JavaScript number cannot tell them
/// apart) but keeps exact integers exact and distinguishes negative zero.
pub fn same(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => {
            let exact = |n: &serde_json::Number| n.is_i64() || n.is_u64();
            if exact(a) && exact(b) {
                a == b
            } else {
                match (a.as_f64(), b.as_f64()) {
                    (Some(x), Some(y)) if !exact(a) && !exact(b) => x.to_bits() == y.to_bits(),
                    (Some(x), Some(y)) => x == y,
                    _ => false,
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, x)| b.get(key).is_some_and(|y| same(x, y)))
        }
        _ => actual == expected,
    }
}
