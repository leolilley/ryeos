use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::subprocess_spec::SubprocessBuildRequest;

/// Stdin shape declared by a protocol descriptor.
///
/// The builder (protocols/builder.rs) consumes this enum to decide what
/// bytes to place on the child's stdin. Vocabulary-level `build_stdin`
/// handles the `ParametersJson` case; `Opaque` and `LaunchEnvelopeV1`
/// are handled by the builder directly (see builder.rs for details).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StdinShape {
    /// JSON serialization of dispatch parameters (tool path).
    /// `params` from DispatchRequest is written verbatim as the stdin body.
    ParametersJson,

    /// No stdin data. The caller feeds stdin out of band (or not at all).
    /// Builder produces `Vec::new()` for this shape.
    Opaque,

    /// LaunchEnvelope v1 (runtime path). The daemon constructs a
    /// `LaunchEnvelope` (see `launch_envelope_types.rs`) and the builder
    /// serializes it onto stdin. The vocabulary `build_stdin` function
    /// does NOT handle this case — it returns an error if called with
    /// this shape, because envelope construction requires daemon-level
    /// context that the vocabulary module does not have.
    LaunchEnvelopeV1,
}

/// Build stdin bytes for a shape that the vocabulary layer can handle.
///
/// Returns an error for `LaunchEnvelopeV1` — the builder handles that
/// case directly with a pre-constructed `LaunchEnvelope`.
/// Returns an error for `Opaque` — the builder produces empty bytes.
pub fn build_stdin(
    shape: StdinShape,
    request: &SubprocessBuildRequest,
) -> Result<Vec<u8>, EngineError> {
    match shape {
        StdinShape::ParametersJson => {
            Ok(serde_json::to_vec(&request.params)
                .map_err(|e| EngineError::Internal(format!("stdin serialize failed: {e}")))?)
        }
        StdinShape::Opaque => {
            // Opaque stdin: no data. The builder produces empty bytes.
            Ok(Vec::new())
        }
        StdinShape::LaunchEnvelopeV1 => {
            // Launch envelope construction requires daemon-level context.
            // The builder (protocols/builder.rs) handles this directly by
            // accepting a pre-constructed LaunchEnvelope reference.
            Err(EngineError::Internal(
                "build_stdin cannot produce LaunchEnvelopeV1; use the protocol builder".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_request(params: serde_json::Value) -> SubprocessBuildRequest {
        SubprocessBuildRequest {
            cmd: PathBuf::from("/bin/echo"),
            args: vec![],
            cwd: PathBuf::from("/tmp"),
            timeout: std::time::Duration::from_secs(30),
            item_ref: crate::canonical_ref::CanonicalRef::parse("tool:test/id").unwrap(),
            thread_id: "T-test-thread".to_string(),
            project_path: PathBuf::from("/tmp"),
            acting_principal: "fp:test".to_string(),
            cas_root: PathBuf::from("/tmp/cas"),
            callback_token: None,
            callback_socket_path: None,
            vault_handle: None,
            params,
            resolution_output: None,
        }
    }

    #[test]
    fn round_trip_all_variants() {
        for (name, shape) in [
            ("parameters_json", StdinShape::ParametersJson),
            ("opaque", StdinShape::Opaque),
            ("launch_envelope_v1", StdinShape::LaunchEnvelopeV1),
        ] {
            let yaml = serde_yaml::to_string(&shape).unwrap();
            let parsed: StdinShape = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(parsed, shape, "round-trip failed for {name}");
        }
    }

    #[test]
    fn reject_unknown() {
        let err = serde_yaml::from_str::<StdinShape>("unknown_shape");
        assert!(err.is_err(), "unknown stdin shape must be rejected");
    }

    #[test]
    fn parameters_json_builder_writes_params_verbatim() {
        let params = serde_json::json!({"key": "value", "num": 42});
        let req = make_request(params.clone());
        let bytes = build_stdin(StdinShape::ParametersJson, &req).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, params);
    }

    #[test]
    fn opaque_builder_produces_empty_bytes() {
        let req = make_request(serde_json::json!({}));
        let bytes = build_stdin(StdinShape::Opaque, &req).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn launch_envelope_v1_builder_errors() {
        let req = make_request(serde_json::json!({}));
        let result = build_stdin(StdinShape::LaunchEnvelopeV1, &req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("protocol builder"));
    }
}
