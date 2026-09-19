//! Renderer-neutral wire coordinates for daemon-compiled UI bindings.
//!
//! These DTOs identify an entry in a session-bound compiled table. They do not
//! carry an executable item ref, capability grant, or client-lowered command.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Informational posture derived by the daemon from the compiled binding.
/// It is presentation data, never an authorization input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiEffectivePosture {
    #[default]
    ObservationOnly,
    Interactive,
}

/// Public projection of one server-admitted binding. This is not a grant:
/// every request is independently checked against the cookie-authenticated
/// session's retained attachment. Display paths cannot admit a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBindingAttachment {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
    pub surface_ref: String,
    pub surface_generation: String,
    pub effective_surface: Value,
    pub project_path: Option<String>,
    pub posture: UiEffectivePosture,
    pub binding_request_bounds: UiBindingRequestBounds,
}

impl UiBindingAttachment {
    /// Validate the public coordinate before retaining it. This establishes
    /// shape only; the daemon remains the authority for admission.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.binding_attachment_id.is_empty()
            || self.binding_generation == 0
            || self.binding_digest.is_empty()
            || self.surface_ref.is_empty()
            || self.surface_generation.is_empty()
        {
            return Err("binding attachment has an incomplete coordinate");
        }
        if self.binding_request_bounds.max_request_bytes == 0
            || self.binding_request_bounds.max_input_bytes == 0
        {
            return Err("binding attachment has invalid request bounds");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiBindingCoordinate {
    Source {
        view_ref: String,
        channel: String,
    },
    Affordance {
        view_ref: String,
        affordance_id: String,
    },
    /// The one input route authored by the effective surface. Its executable
    /// template is retained in the compiled session binding.
    SurfaceRoute,
}

/// Reserved channel coordinates for sources declared by an input block rather
/// than in the view's ordinary `sources` map. These remain coordinates inside
/// the compiled view binding; they are not executable refs supplied by a
/// renderer.
pub fn input_mentions_channel(input_id: &str) -> String {
    format!("input.{input_id}.mentions")
}

pub fn input_completion_channel(input_id: &str) -> String {
    format!("input.{input_id}.completion")
}

/// One session-bound source fetch or affordance invocation. The daemon proves
/// that payload kind matches the coordinate and signed producer before it
/// substitutes or dispatches anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBindingRequest {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
    pub coordinate: UiBindingCoordinate,
    pub payload: UiBindingPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiBindingPayload {
    SourceParameters {
        params: Value,
    },
    Selection {
        record: Value,
    },
    Tokens {
        tokens: Vec<String>,
        #[serde(default)]
        arguments: Value,
    },
    Input {
        value: String,
        /// Dynamic delivery coordinate from the session's seat braid. This is
        /// accepted only for `SurfaceRoute`; it can select a continuation but
        /// cannot alter the signed executable route.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route: Option<UiBindingRouteContext>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBindingRouteContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_root_id: Option<String>,
    #[serde(default)]
    pub interrupt: bool,
}

impl UiBindingRequest {
    /// Enforce the bounds compiled into the active session binding. The daemon
    /// independently enforces the same authority; this client check prevents
    /// avoidable allocation and transport work, but never grants permission.
    pub fn validate_bounds(
        &self,
        bounds: UiBindingRequestBounds,
    ) -> Result<(), UiBindingRequestError> {
        if self.binding_attachment_id.is_empty()
            || self.binding_generation == 0
            || self.binding_digest.is_empty()
        {
            return Err(UiBindingRequestError::InvalidAttachment);
        }
        if bounds.max_request_bytes == 0 || bounds.max_input_bytes == 0 {
            return Err(UiBindingRequestError::InvalidBounds);
        }
        let encoded = serde_json::to_vec(self).map_err(|_| UiBindingRequestError::NotJson)?;
        let encoded_bytes = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
        if encoded_bytes > bounds.max_request_bytes {
            return Err(UiBindingRequestError::TooLarge {
                actual: encoded_bytes,
                maximum: bounds.max_request_bytes,
            });
        }
        let input_bytes = |value: &str| u64::try_from(value.len()).unwrap_or(u64::MAX);
        if let UiBindingPayload::Input { value, .. } = &self.payload
            && input_bytes(value) > bounds.max_input_bytes
        {
            return Err(UiBindingRequestError::InputTooLarge {
                actual: input_bytes(value),
                maximum: bounds.max_input_bytes,
            });
        }
        let coordinate_matches = matches!(
            (&self.coordinate, &self.payload),
            (
                UiBindingCoordinate::Source { .. },
                UiBindingPayload::SourceParameters { .. }
            ) | (
                UiBindingCoordinate::Affordance { .. },
                UiBindingPayload::Selection { .. } | UiBindingPayload::Tokens { .. }
            ) | (
                UiBindingCoordinate::Affordance { .. },
                UiBindingPayload::Input { route: None, .. }
            ) | (
                UiBindingCoordinate::SurfaceRoute,
                UiBindingPayload::Input { .. }
            )
        );
        if !coordinate_matches {
            return Err(UiBindingRequestError::PayloadMismatch);
        }
        Ok(())
    }
}

/// Limits compiled for the active binding/session by existing authority.
/// These are inputs to validation, not client-selected defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBindingRequestBounds {
    pub max_request_bytes: u64,
    pub max_input_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UiBindingRequestError {
    #[error("UI binding request requires an exact admitted attachment coordinate")]
    InvalidAttachment,
    #[error("UI binding request bounds must be non-zero")]
    InvalidBounds,
    #[error("UI binding request is not canonical JSON")]
    NotJson,
    #[error("UI binding request is {actual} bytes; maximum is {maximum}")]
    TooLarge { actual: u64, maximum: u64 },
    #[error("UI binding input is {actual} bytes; maximum is {maximum}")]
    InputTooLarge { actual: u64, maximum: u64 },
    #[error("UI binding coordinate and producer payload do not match")]
    PayloadMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_descriptor_requires_complete_coordinate_and_bounds() {
        let valid = serde_json::json!({
            "binding_attachment_id": "attachment-one",
            "binding_generation": 1,
            "binding_digest": "digest-one",
            "surface_ref": "surface:test",
            "surface_generation": "generation-one",
            "effective_surface": {"name": "test"},
            "project_path": null,
            "posture": "observation_only",
            "binding_request_bounds": {"max_request_bytes": 4096, "max_input_bytes": 1024}
        });
        assert!(
            serde_json::from_value::<UiBindingAttachment>(valid.clone())
                .unwrap()
                .validate()
                .is_ok()
        );
        for field in [
            "binding_attachment_id",
            "binding_digest",
            "surface_ref",
            "surface_generation",
        ] {
            let mut invalid = valid.clone();
            invalid[field] = Value::String(String::new());
            assert!(
                serde_json::from_value::<UiBindingAttachment>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        for path in [
            "/binding_generation",
            "/binding_request_bounds/max_request_bytes",
            "/binding_request_bounds/max_input_bytes",
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(path).unwrap() = Value::from(0);
            assert!(
                serde_json::from_value::<UiBindingAttachment>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
    }

    #[test]
    fn request_requires_the_complete_attachment_coordinate() {
        let request = UiBindingRequest {
            binding_attachment_id: "attachment-one".into(),
            binding_generation: 1,
            binding_digest: "fixture-digest".into(),
            coordinate: UiBindingCoordinate::Source {
                view_ref: "view:test/read".into(),
                channel: "default".into(),
            },
            payload: UiBindingPayload::SourceParameters {
                params: serde_json::json!({}),
            },
        };
        let bounds = UiBindingRequestBounds {
            max_request_bytes: 4096,
            max_input_bytes: 4096,
        };
        assert!(request.validate_bounds(bounds).is_ok());
        for field in [
            "binding_attachment_id",
            "binding_generation",
            "binding_digest",
        ] {
            let mut encoded = serde_json::to_value(&request).unwrap();
            encoded.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<UiBindingRequest>(encoded).is_err());
        }
        for bad in [
            UiBindingRequest {
                binding_attachment_id: String::new(),
                ..request.clone()
            },
            UiBindingRequest {
                binding_generation: 0,
                ..request.clone()
            },
            UiBindingRequest {
                binding_digest: String::new(),
                ..request
            },
        ] {
            assert_eq!(
                bad.validate_bounds(bounds),
                Err(UiBindingRequestError::InvalidAttachment)
            );
        }
    }

    #[test]
    fn coordinate_cannot_smuggle_an_execution_target() {
        let encoded = serde_json::to_value(UiBindingCoordinate::Affordance {
            view_ref: "view:example/work".to_string(),
            affordance_id: "resume".to_string(),
        })
        .unwrap();
        assert!(encoded.get("item_ref").is_none());
        assert!(encoded.get("tokens").is_none());
        assert!(encoded.get("capabilities").is_none());
    }

    #[test]
    fn request_bounds_and_coordinate_payload_pair_are_closed() {
        let mismatch = UiBindingRequest {
            binding_attachment_id: "fixture-attachment".into(),
            binding_generation: 1,
            binding_digest: "sha256:fixture".to_string(),
            coordinate: UiBindingCoordinate::Source {
                view_ref: "view:example/work".to_string(),
                channel: "default".to_string(),
            },
            payload: UiBindingPayload::Input {
                value: "hello".to_string(),
                route: None,
            },
        };
        assert_eq!(
            mismatch.validate_bounds(UiBindingRequestBounds {
                max_request_bytes: 1024,
                max_input_bytes: 16,
            }),
            Err(UiBindingRequestError::PayloadMismatch)
        );

        let too_large = UiBindingRequest {
            binding_attachment_id: "fixture-attachment".into(),
            binding_generation: 1,
            binding_digest: "sha256:fixture".to_string(),
            coordinate: UiBindingCoordinate::Affordance {
                view_ref: "view:example/work".to_string(),
                affordance_id: "submit".to_string(),
            },
            payload: UiBindingPayload::Input {
                value: "xxxxx".to_string(),
                route: None,
            },
        };
        assert!(matches!(
            too_large.validate_bounds(UiBindingRequestBounds {
                max_request_bytes: 1024,
                max_input_bytes: 4,
            }),
            Err(UiBindingRequestError::InputTooLarge { .. })
        ));
    }
}
