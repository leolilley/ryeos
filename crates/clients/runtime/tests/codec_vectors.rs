//! Replays `json-exactness.json` against the runtime codec.

mod common;

use common::{Untyped, load, name_of, same, typed};
use ryeos_client_runtime::RuntimeError;
use ryeos_client_runtime::codec;
use serde_json::Value;

/// Cases whose expected text depends on how a JavaScript runtime spells or
/// orders things, with the reason each cannot be asserted here.
fn skip_reason(name: &str) -> Option<&'static str> {
    match name {
        "insertion key order kept by encodeJsonBody" => Some(
            "serde_json::Value objects are not insertion ordered; the retained append body is \
             written by api::encode_append_request instead",
        ),
        "canonical sorts keys by Unicode scalar value"
        | "canonical escapes non-ASCII as lowercase \\u (surrogate pairs above BMP)"
        | "canonical keeps bigint tokens"
        | "canonical empty containers" => Some("canonical encoding not part of the runtime"),
        "float 0.000001" => Some(
            "ryu spells 0.000001 as 1e-6 where JavaScript writes 0.000001; digests commit to f64 \
             bits and are unaffected",
        ),
        "lone surrogate escape accepted by decode (JSON.parse)" => {
            Some("a Rust String cannot hold a lone surrogate; the decoder refuses it explicitly")
        }
        _ => None,
    }
}

/// Run one case. `None` means the input has no `Value` form.
fn run(function: &str, input: &Value) -> Option<Result<Value, RuntimeError>> {
    let encode = |encoder: fn(&Value) -> Result<String, RuntimeError>| match typed(input) {
        Ok(value) => Some(encoder(&value).map(Value::String)),
        Err(Untyped::Refused(error)) => Some(Err(error)),
        Err(Untyped::Unrepresentable(_)) => None,
    };
    match function {
        "encodeJsonBody" => encode(codec::encode_exact),
        "encodeCanonicalJsonBody" => None,
        "decodeJsonBody" => Some(codec::decode_exact(input.as_str().expect("text input"))),
        "decodeJsonBody+encodeJsonBody" => Some(
            codec::decode_exact(input.as_str().expect("text input"))
                .and_then(|value| codec::encode_exact(&value))
                .map(Value::String),
        ),
        other => panic!("unknown vector function {other}"),
    }
}

#[test]
fn json_exactness_vectors() {
    let mut checked = 0;
    for case in load("json-exactness") {
        let name = name_of(&case);
        if skip_reason(name).is_some() {
            continue;
        }
        let function = case["function"].as_str().expect("function");
        let Some(outcome) = run(function, &case["input"]) else {
            continue;
        };
        let expected = &case["expected"];
        if let Some(message) = expected.pointer("/error/message") {
            let error = outcome.expect_err(name);
            assert_eq!(
                Value::String(error.to_string()),
                *message,
                "{name}: error text"
            );
        } else {
            let actual = outcome.unwrap_or_else(|error| panic!("{name}: {error}"));
            let want = typed(&expected["ok"]).unwrap_or_else(|_| panic!("{name}: expected form"));
            if function == "decodeJsonBody" {
                assert!(same(&actual, &want), "{name}: {actual} != {want}");
            } else {
                assert_eq!(actual, want, "{name}");
            }
        }
        checked += 1;
    }
    assert!(checked >= 30, "only {checked} cases were checked");
}

#[test]
fn decode_reads_every_vector_file() {
    // The loader itself decodes each file with the strict parser, so a parser
    // regression fails here before any assertion about a case.
    for file in [
        "json-exactness",
        "seat-digest",
        "pending-append",
        "seat-history",
        "seat-touch",
        "thread-tail",
        "layout-preferences",
        "boot-attachment",
    ] {
        assert!(!load(file).is_empty(), "{file}");
    }
}
