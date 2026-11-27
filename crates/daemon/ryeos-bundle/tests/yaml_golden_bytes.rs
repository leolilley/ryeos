//! The exact YAML bytes the node signs, and the exact values it reads.
//!
//! Bundle registrations, node-signed policy documents, remote descriptors and
//! principal records are written as `serde_yaml::to_string` of a value and
//! then signed over those bytes; a sealed record or a recomputed digest
//! compares them later. Shipped signed YAML is read with `serde_yaml` and its
//! values feed digests and admission. A different YAML library, version or
//! feature selection (for example `serde_json`'s `preserve_order`, which
//! changes map order) would change these bytes or values silently. These
//! tests pin both, so such a change fails here first.
//!
//! Every case is checked and every mismatch reported in one run.

use serde_json::{Value, json};

fn report(mismatches: Vec<String>) {
    assert!(
        mismatches.is_empty(),
        "{} case(s) differ from the pinned bytes or values:\n\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

/// Value shapes the production emitters pass, with the bytes pinned for each.
fn emitted_cases() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        (
            "node registration",
            json!({ "kind": "node", "path": "/var/lib/node/.ai/bundles/core" }),
            "kind: node\npath: /var/lib/node/.ai/bundles/core\n",
        ),
        (
            "map keys are written sorted, not in insertion order",
            json!({ "zeta": 1, "alpha": 2, "mid": 3 }),
            "alpha: 2\nmid: 3\nzeta: 1\n",
        ),
        (
            "registration with a digest, a list, an empty map and an empty list",
            json!({
                "name": "ryeos-docs",
                "digest": "9045e7fd57fbc0b5e4b60a454830b627b674d97270f73acc4bdf82065cacf73b",
                "provides_kinds": ["knowledge", "tool"],
                "grants": {},
                "requires": [],
                "count": 7,
                "enabled": true,
                "previous": null,
            }),
            "count: 7\n\
             digest: 9045e7fd57fbc0b5e4b60a454830b627b674d97270f73acc4bdf82065cacf73b\n\
             enabled: true\n\
             grants: {}\n\
             name: ryeos-docs\n\
             previous: null\n\
             provides_kinds:\n\
             - knowledge\n\
             - tool\n\
             requires: []\n",
        ),
        (
            "nested maps and a list of maps",
            json!({
                "section": { "schema": 1, "named_bundles": {} },
                "items": [{ "id": "a", "n": 1 }, { "id": "b", "n": 2 }],
            }),
            "items:\n\
             - id: a\n  n: 1\n\
             - id: b\n  n: 2\n\
             section:\n  named_bundles: {}\n  schema: 1\n",
        ),
        (
            "strings that would read back as another scalar are quoted; `yes` is not (a YAML 1.1 reader would read it as true)",
            json!({
                "a_true": "true",
                "b_null": "null",
                "c_tilde": "~",
                "d_int": "123",
                "e_float": "1e5",
                "f_digits_digest": "000123",
                "g_hex": "0x1F",
                "h_yes": "yes",
                "i_empty": "",
            }),
            "a_true: 'true'\n\
             b_null: 'null'\n\
             c_tilde: '~'\n\
             d_int: '123'\n\
             e_float: '1e5'\n\
             f_digits_digest: '000123'\n\
             g_hex: '0x1F'\n\
             h_yes: yes\n\
             i_empty: ''\n",
        ),
        (
            "strings with YAML syntax in them",
            json!({
                "colon": "key: value",
                "hash": "a #comment",
                "leading_dash": "- item",
                "quote": "it's",
                "fingerprint": "fp:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea",
            }),
            "colon: 'key: value'\n\
             fingerprint: fp:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea\n\
             hash: 'a #comment'\n\
             leading_dash: '- item'\n\
             quote: it's\n",
        ),
        (
            "a multi-line string and a non-ASCII string",
            json!({ "text": "line one\nline two", "word": "café" }),
            "text: |-\n  line one\n  line two\nword: café\n",
        ),
        (
            "numbers",
            json!({ "neg": -3, "big": 18446744073709551615u64, "float": 0.5 }),
            "big: 18446744073709551615\nfloat: 0.5\nneg: -3\n",
        ),
    ]
}

#[test]
fn emitted_bytes_are_pinned() {
    let mut mismatches = Vec::new();
    for (case, value, pinned) in emitted_cases() {
        let emitted = serde_yaml::to_string(&value).expect("emit");
        if emitted != pinned {
            mismatches.push(format!(
                "{case}\n  pinned:  {pinned:?}\n  emitted: {emitted:?}"
            ));
        }
    }
    report(mismatches);
}

/// Every pinned emission reads back as the value it was written from.
#[test]
fn emitted_bytes_read_back_as_the_same_value() {
    let mut mismatches = Vec::new();
    for (case, value, pinned) in emitted_cases() {
        let read: Value = serde_yaml::from_str(pinned).expect("parse");
        if read != value {
            mismatches.push(format!("{case}\n  wrote: {value}\n  read:  {read}"));
        }
    }
    report(mismatches);
}

/// Plain scalars whose reading differs between YAML 1.1 and 1.2, or between
/// libraries, with the value this node reads pinned for each.
#[test]
fn parsed_values_are_pinned() {
    let cases: Vec<(&str, Value)> = vec![
        ("yes", json!("yes")),
        ("no", json!("no")),
        ("on", json!("on")),
        ("off", json!("off")),
        ("y", json!("y")),
        ("True", json!(true)),
        ("~", Value::Null),
        ("null", Value::Null),
        ("", Value::Null),
        ("0o17", json!(15)),
        ("017", json!("017")),
        ("0x1F", json!(31)),
        ("+12", json!(12)),
        ("1_000", json!("1_000")),
        ("12e3", json!(12000.0)),
        ("000123", json!("000123")),
        ("2026-11-27", json!("2026-11-27")),
        ("2026-11-27T04:25:49Z", json!("2026-11-27T04:25:49Z")),
        ("fp:abc", json!("fp:abc")),
    ];
    let mut mismatches = Vec::new();
    for (scalar, pinned) in cases {
        let document = format!("v: {scalar}\n");
        let read: Value = serde_yaml::from_str(&document).expect("parse");
        if read["v"] != pinned {
            mismatches.push(format!(
                "{document:?}\n  pinned: {pinned}\n  read:   {}",
                read["v"]
            ));
        }
    }
    report(mismatches);
}
