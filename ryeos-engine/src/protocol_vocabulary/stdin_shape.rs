use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::subprocess_spec::SubprocessBuildRequest;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum StdinShape {
    /// JSON serialization of dispatch parameters (today's tool path).
    /// `params` from DispatchRequest is written verbatim as the stdin body.
    ParametersJson,

    /// LaunchEnvelope v1 (today's runtime path). Constructs an envelope
    /// with: resolution snapshot, callback token, CAS root, thread metadata.
    /// Wire shape is `LaunchEnvelope` in `launch_envelope_types.rs`.
    LaunchEnvelopeV1,
}

pub fn build_stdin(
    shape: StdinShape,
    request: &SubprocessBuildRequest,
) -> Result<Vec<u8>, EngineError> {
    match shape {
        StdinShape::ParametersJson => {
            Ok(serde_json::to_vec(&request.params)
                .map_err(|e| EngineError::Internal(format!("stdin serialize failed: {e}")))?)
        }
        StdinShape::LaunchEnvelopeV1 => {
            // The full LaunchEnvelope construction requires daemon-level
            // context (roots, callback, policy, resolution). In commit β
            // we provide the builder signature; the daemon wires the actual
            // construction in ζ. For now, use params as a placeholder — the
            // vocabulary module defines the shape, the daemon fills it.
            let envelope = serde_json::json!({
                "invocation_id": format!("inv-{}", &request.thread_id[..8.min(request.thread_id.len())]),
                "thread_id": request.thread_id,
                "params": request.params,
            });
            Ok(serde_json::to_vec(&envelope)
                .map_err(|e| EngineError::Internal(format!("stdin serialize failed: {e}")))?)
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
            vault_handle: None,
            params,
            resolution_output: None,
        }
    }

    #[test]
    fn round_trip_all_variants() {
        for (name, shape) in [
            ("parameters_json", StdinShape::ParametersJson),
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
}
