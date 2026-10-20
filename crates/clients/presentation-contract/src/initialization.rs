//! Shared data-only identity witness for the signed presentation initialization.
//!
//! This validates the immutable predecessor subject used by lifecycle readers.
//! Browser composition owners additionally validate the full initialization
//! manifest; this witness alone is not UI authority or manifest admission.

use serde_json::Value;

use crate::identity::canonical_operation_id;

/// One clean-cut version shared by lifecycle witnesses and the UI manifest.
pub const PRESENTATION_INITIALIZATION_SCHEMA: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiPresentationInitializationIdentity {
    pub operation_id: String,
    pub creation_request_id: String,
    pub presentation_id: String,
}

/// Data-only signed initialization bound. This authenticates the unique root
/// descriptor's original request ceiling, not its composition or authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiPresentationInitializationBound {
    pub identity: UiPresentationInitializationIdentity,
    pub max_seat_append_bytes: u64,
}

impl UiPresentationInitializationBound {
    pub fn from_payload(payload: &Value, presentation_id: &str) -> Result<Self, &'static str> {
        let identity = UiPresentationInitializationIdentity::from_payload(payload)?;
        identity.validate_subject(presentation_id)?;
        let initialization = payload
            .get("initialization")
            .and_then(Value::as_object)
            .ok_or("initialization manifest is not an object")?;
        let root = initialization
            .get("root_binding")
            .and_then(Value::as_object)
            .ok_or("initialization root coordinate is absent")?;
        fn coordinate(
            object: &serde_json::Map<String, Value>,
        ) -> Result<(&str, u64, &str), &'static str> {
            // Match the admitted attachment coordinate predicate: nonempty ID
            // and digest plus positive generation. The digest is a lookup
            // coordinate here; browser admission does not require SHA syntax.
            let id = object
                .get("binding_attachment_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("initialization binding ID is absent")?;
            let generation = object
                .get("binding_generation")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .ok_or("initialization binding generation is invalid")?;
            let digest = object
                .get("binding_digest")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("initialization binding digest is invalid")?;
            Ok::<_, &'static str>((id, generation, digest))
        }
        if root.len() != 3 {
            return Err("initialization root coordinate has unexpected fields");
        }
        let root_coordinate = coordinate(root)?;
        let descriptors = initialization
            .get("binding_attachments")
            .and_then(Value::as_array)
            .ok_or("initialization descriptors are absent")?;
        let mut previous_id = None;
        let mut root_bound = None;
        for descriptor in descriptors {
            let object = descriptor
                .as_object()
                .ok_or("initialization descriptor is not an object")?;
            let candidate = coordinate(object)?;
            if previous_id.is_some_and(|previous: &str| previous >= candidate.0) {
                return Err("initialization descriptors are not unique canonical order");
            }
            previous_id = Some(candidate.0);
            let bounds = object
                .get("binding_request_bounds")
                .and_then(Value::as_object)
                .ok_or("initialization descriptor bounds are absent")?;
            if bounds.len() != 3 {
                return Err("initialization descriptor bounds have unexpected fields");
            }
            let positive = |name| {
                bounds
                    .get(name)
                    .and_then(Value::as_u64)
                    .filter(|value| *value > 0)
                    .ok_or("initialization descriptor request bound is invalid")
            };
            positive("max_request_bytes")?;
            positive("max_input_bytes")?;
            let append_bound = positive("max_seat_append_bytes")?;
            if candidate == root_coordinate {
                root_bound = Some(append_bound);
            } else if candidate.0 == root_coordinate.0 {
                return Err("initialization root binding coordinate differs from descriptor");
            }
        }
        Ok(Self {
            identity,
            max_seat_append_bytes: root_bound
                .ok_or("initialization root binding descriptor is absent")?,
        })
    }
}

impl UiPresentationInitializationIdentity {
    pub fn from_payload(payload: &Value) -> Result<Self, &'static str> {
        let record = payload
            .as_object()
            .ok_or("initialization record is not an object")?;
        if record.len() != 2 {
            return Err("initialization record has unexpected fields");
        }
        let initialization = record
            .get("initialization")
            .and_then(Value::as_object)
            .ok_or("initialization manifest is not an object")?;
        let field = |object: &serde_json::Map<String, Value>, name| {
            object
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or("initialization identity field is absent")
        };
        let witness = Self {
            operation_id: field(record, "operation_id")?,
            creation_request_id: field(initialization, "creation_request_id")?,
            presentation_id: field(initialization, "presentation_id")?,
        };
        if initialization.get("schema").and_then(Value::as_u64)
            != Some(u64::from(PRESENTATION_INITIALIZATION_SCHEMA))
            || !canonical_operation_id(&witness.operation_id)
            || !canonical_operation_id(&witness.creation_request_id)
            || !canonical_operation_id(&witness.presentation_id)
            || witness.operation_id != witness.creation_request_id
        {
            return Err("initialization identity is invalid");
        }
        Ok(witness)
    }

    pub fn validate_subject(&self, presentation_id: &str) -> Result<(), &'static str> {
        if self.presentation_id != presentation_id {
            return Err("initialization presentation differs from exact seat");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_subject_and_operation_are_required() {
        let id = "11111111-1111-4111-8111-111111111111";
        let presentation = "22222222-2222-4222-8222-222222222222";
        let payload = json!({"operation_id":id,"initialization": {
            "schema":2,"selection":{"kind":"configured_defaults"},"creation_request_id":id,"presentation_id":presentation
        }});
        let witness = UiPresentationInitializationIdentity::from_payload(&payload).unwrap();
        assert!(witness.validate_subject(presentation).is_ok());
        assert!(witness.validate_subject(id).is_err());
        let mut wrong = payload.clone();
        wrong["initialization"]["creation_request_id"] = json!(presentation);
        assert!(UiPresentationInitializationIdentity::from_payload(&wrong).is_err());
        wrong = payload;
        wrong["initialization"]["schema"] = json!(1);
        assert!(UiPresentationInitializationIdentity::from_payload(&wrong).is_err());
    }

    #[test]
    fn root_bound_requires_unique_exact_descriptor_and_positive_structural_bounds() {
        let id = "11111111-1111-4111-8111-111111111111";
        let presentation = "22222222-2222-4222-8222-222222222222";
        let descriptor = json!({"binding_attachment_id":"root","binding_generation":1,
            "binding_digest":"a".repeat(64),"binding_request_bounds":{
                "max_request_bytes":4096,"max_input_bytes":1024,"max_seat_append_bytes":65536}});
        let mut payload = json!({"operation_id":id,"initialization":{
            "schema":2,"selection":{"kind":"configured_defaults"},"creation_request_id":id,"presentation_id":presentation,
            "root_binding":{"binding_attachment_id":"root","binding_generation":1,
                "binding_digest":"a".repeat(64)},
            "binding_attachments":[descriptor]}});
        assert_eq!(
            UiPresentationInitializationBound::from_payload(&payload, presentation)
                .unwrap()
                .max_seat_append_bytes,
            65536
        );
        payload["initialization"]["binding_attachments"][0]["binding_request_bounds"]["max_seat_append_bytes"] =
            json!(0);
        assert!(UiPresentationInitializationBound::from_payload(&payload, presentation).is_err());
        payload["initialization"]["binding_attachments"][0]["binding_request_bounds"]["max_seat_append_bytes"] =
            json!(65536);
        payload["initialization"]["root_binding"]["binding_generation"] = json!(2);
        assert!(UiPresentationInitializationBound::from_payload(&payload, presentation).is_err());
        payload["initialization"]["root_binding"]["binding_generation"] = json!(1);
        let duplicate = payload["initialization"]["binding_attachments"][0].clone();
        payload["initialization"]["binding_attachments"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert!(UiPresentationInitializationBound::from_payload(&payload, presentation).is_err());
    }
}
