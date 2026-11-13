//! Replays `boot-attachment.json` against the admission selection functions.
//!
//! The seat bound given as a JavaScript `Number` rather than a `BigInt` is
//! skipped by name: a JSON document has only one integer type.

mod common;

use common::{load, name_of, typed};
use ryeos_client_runtime::admission::{
    prepared_presentation_selection, query_values, retained_presentation_url,
    session_root_attachment, session_seat_append_bound,
};
use ryeos_client_runtime::codec::decode_exact;
use serde_json::{Value, json};

fn default_session() -> Value {
    json!({
        "root_attachment_id": "surface",
        "binding_attachments": [{
            "binding_attachment_id": "surface",
            "binding_generation": 1,
            "binding_digest": "digest",
            "project_path": null,
            "subject": {"kind": "projectless"},
        }],
    })
}

fn session_of(input: &Value) -> Value {
    match input.get("session_json").and_then(Value::as_str) {
        Some(text) => decode_exact(text).expect("fixture session"),
        None => default_session(),
    }
}

fn attachment_mut(session: &mut Value) -> &mut Value {
    &mut session["binding_attachments"][0]
}

fn expected_error(case: &Value) -> Option<&str> {
    case["expected"]["error"]["message"].as_str()
}

#[test]
fn boot_attachment_vectors() {
    let mut checked = 0;
    for case in load("boot-attachment") {
        let name = name_of(&case).to_owned();
        let function = case["function"].as_str().unwrap().to_owned();
        let input = &case["input"];
        if function.starts_with("retainedPresentationUrl") {
            let url = retained_presentation_url(
                input["url"].as_str().unwrap(),
                input["presentation_id"].as_str().unwrap(),
            )
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            let (before_hash, hash) = url.split_at(url.find('#').unwrap_or(url.len()));
            let search = before_hash.find('?').map_or("", |at| &before_hash[at..]);
            assert_eq!(search, case["expected"]["ok"]["search"], "{name}");
            assert_eq!(hash, case["expected"]["ok"]["hash"], "{name}");
            checked += 1;
            continue;
        }
        let mut session = session_of(input);
        if input["mutation"].as_str().is_some() {
            let copy = session["binding_attachments"][0].clone();
            session["binding_attachments"]
                .as_array_mut()
                .unwrap()
                .push(copy);
        }
        if function.starts_with("preparedPresentationSelection")
            && input.get("presentations").is_some()
        {
            let mut memberships = input["presentations"].clone();
            for membership in memberships.as_array_mut().unwrap() {
                membership["root_binding"] = session["binding_attachments"][0].clone();
            }
            session["presentations"] = memberships;
        }
        if input.get("same_as").is_some() {
            // The previous case's session with the membership's generation
            // set to 1.
            let previous = load("boot-attachment")
                .into_iter()
                .find(|c| name_of(c) == "prepared link selects exact membership")
                .unwrap();
            session = session_of(&previous["input"]);
            let mut memberships = previous["input"]["presentations"].clone();
            for membership in memberships.as_array_mut().unwrap() {
                membership["root_binding"] = session["binding_attachments"][0].clone();
                membership["root_binding"]["binding_generation"] = json!(1);
            }
            session["presentations"] = memberships;
        }
        if let Some(project_path) = input.get("project_path") {
            attachment_mut(&mut session)["project_path"] = project_path.clone();
        }
        if let Some(subject) = input.get("subject") {
            match typed(subject) {
                Ok(subject) => attachment_mut(&mut session)["subject"] = subject,
                Err(_) => {
                    attachment_mut(&mut session)
                        .as_object_mut()
                        .unwrap()
                        .remove("subject");
                }
            }
        }
        if function.starts_with("sessionSeatAppendBound")
            && let Some(bound) = input.get("max_seat_append_bytes")
        {
            if bound.is_number() {
                continue;
            }
            // An `$undefined` bound is a record without the field.
            attachment_mut(&mut session)["binding_request_bounds"] = match typed(bound) {
                Ok(value) => json!({ "max_seat_append_bytes": value }),
                Err(_) => json!({}),
            };
        }

        let result: Result<Value, String> = if function.starts_with("sessionRootAttachment") {
            session_root_attachment(&session)
                .map(|root| {
                    if function.ends_with(".binding_generation") {
                        json!(root.binding_generation)
                    } else {
                        json!(root.binding_attachment_id)
                    }
                })
                .map_err(|error| error.to_string())
        } else if function.starts_with("preparedPresentationSelection") {
            let url = input["url"].as_str().unwrap();
            prepared_presentation_selection(&session, &query_values(url, "presentation_id"))
                .map(|selection| match selection {
                    None => Value::Null,
                    Some(selection) => json!({
                        "generation": selection.root_attachment.binding_generation,
                        "presentation": {
                            "presentation_id": selection.presentation.presentation_id,
                            "thread_id": selection.presentation.thread_id,
                            "chain_root_id": selection.presentation.chain_root_id,
                        },
                    }),
                })
                .map_err(|error| error.to_string())
        } else {
            let root = input["root_attachment_id"].as_str().unwrap();
            session_seat_append_bound(&session, root)
                .map(|bound| json!(bound))
                .map_err(|error| error.to_string())
        };

        match (&result, expected_error(&case)) {
            (Err(actual), Some(message)) => assert_eq!(actual, message, "{name}"),
            (Ok(actual), None) => {
                let wanted = typed(&case["expected"]["ok"]).unwrap_or(Value::Null);
                let wanted = match wanted.get("generation") {
                    Some(_) => json!({
                        "generation": typed(&case["expected"]["ok"]["generation"]).unwrap(),
                        "presentation": case["expected"]["ok"]["presentation"],
                    }),
                    None => wanted,
                };
                assert_eq!(*actual, wanted, "{name}");
            }
            (other, wanted) => panic!("{name}: got {other:?}, wanted error {wanted:?}"),
        }
        checked += 1;
    }
    assert!(checked >= 31, "only {checked} admission cases ran");
}
