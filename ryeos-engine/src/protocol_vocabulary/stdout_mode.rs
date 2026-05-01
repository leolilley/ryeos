use serde::{Deserialize, Serialize};

use crate::protocol_vocabulary::error::VocabularyError;
use crate::protocol_vocabulary::StdoutShape;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum StdoutMode {
    Terminal,
    Streaming,
}

/// Compatibility matrix: which (StdoutShape, StdoutMode) pairs are valid.
///
/// | StdoutShape           | Terminal | Streaming |
/// | --------------------- | -------- | --------- |
/// | opaque_bytes          | YES      | NO        |
/// | runtime_result_v1     | YES      | NO        |
/// | streaming_chunks_v1   | NO       | YES       |
pub fn is_compatible_shape_mode(shape: StdoutShape, mode: StdoutMode) -> Result<(), VocabularyError> {
    match (shape, mode) {
        (StdoutShape::OpaqueBytes, StdoutMode::Terminal) => Ok(()),
        (StdoutShape::RuntimeResultV1, StdoutMode::Terminal) => Ok(()),
        (StdoutShape::StreamingChunksV1, StdoutMode::Streaming) => Ok(()),
        (shape, mode) => Err(VocabularyError::StdoutShapeModeMismatch {
            shape: format!("{:?}", shape),
            mode: format!("{:?}", mode),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_variants() {
        for mode in [StdoutMode::Terminal, StdoutMode::Streaming] {
            let yaml = serde_yaml::to_string(&mode).unwrap();
            let parsed: StdoutMode = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn reject_unknown() {
        let err = serde_yaml::from_str::<StdoutMode>("unknown");
        assert!(err.is_err());
    }

    #[test]
    fn compatibility_matrix_table_driven() {
        let cases: Vec<(StdoutShape, StdoutMode, bool)> = vec![
            (StdoutShape::OpaqueBytes, StdoutMode::Terminal, true),
            (StdoutShape::OpaqueBytes, StdoutMode::Streaming, false),
            (StdoutShape::RuntimeResultV1, StdoutMode::Terminal, true),
            (StdoutShape::RuntimeResultV1, StdoutMode::Streaming, false),
            (StdoutShape::StreamingChunksV1, StdoutMode::Terminal, false),
            (StdoutShape::StreamingChunksV1, StdoutMode::Streaming, true),
        ];
        for (shape, mode, compatible) in cases {
            let result = is_compatible_shape_mode(shape, mode);
            assert_eq!(
                result.is_ok(),
                compatible,
                "({:?}, {:?}) expected compatible={}, got {:?}",
                shape, mode, compatible, result
            );
        }
    }
}
