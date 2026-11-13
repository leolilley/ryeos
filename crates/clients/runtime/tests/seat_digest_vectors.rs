//! Replays `seat-digest.json` against the shared payload digest.

mod common;

use common::{Untyped, load, name_of, typed};
use ryeos_client_runtime::codec::{normalize_negative_zero, seat_payload_digest};

/// Cases where the browser's number type, not the digest, decides the
/// outcome.
fn skip_reason(name: &str) -> Option<&'static str> {
    match name {
        "unsafe integer Number refused" => {
            Some("the file token is an exact integer in Rust; only a JavaScript Number is unsafe")
        }
        "1e21 refused (integer-valued f64 above safe range)" => {
            Some("an f64 digests by its bits in Rust; only a JavaScript Number is integer-valued")
        }
        _ => None,
    }
}

#[test]
fn seat_digest_vectors() {
    let mut checked = 0;
    for case in load("seat-digest") {
        let name = name_of(&case);
        if skip_reason(name).is_some() {
            continue;
        }
        match typed(&case["input"]) {
            Ok(mut value) => {
                // The wire encoder writes -0 as 0; a value that is both sent
                // and digested is normalised once so both commit to the same.
                normalize_negative_zero(&mut value);
                let expected = case["expected"]["sha256_hex"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: digest value expected"));
                assert_eq!(
                    seat_payload_digest(&value).unwrap_or_else(|_| panic!("{name}: refused")),
                    expected,
                    "{name}"
                );
            }
            Err(Untyped::Refused(_)) => {
                assert!(
                    case["expected"].get("error").is_some(),
                    "{name}: refused but a digest was expected"
                );
            }
            Err(Untyped::Unrepresentable(_)) => {
                continue;
            }
        }
        checked += 1;
    }
    assert!(checked >= 18, "only {checked} cases were checked");
}
