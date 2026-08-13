pub mod error;
pub use error::VocabularyError;

mod stdin_shape;
pub use stdin_shape::{StdinShape, build_stdin};

mod stdout_shape;
pub use stdout_shape::{
    DecodedFrame, DecodedStdout, FrameReadError, MAX_FRAME_BYTES, StdoutShape, StreamingChunk,
    StreamingChunkKind, decode_stdout_frame, decode_stdout_terminal, read_all_frames,
};

mod stdout_mode;
pub use stdout_mode::{StdoutMode, is_compatible_shape_mode};

mod env_injection;
pub use env_injection::{
    EnvInjection, EnvInjectionSource, RESERVED_ENV_NAMES, is_reserved_env_name, produce_env_value,
    validate_env_name,
};

mod lifecycle;
pub use lifecycle::{LifecycleMode, is_compatible_lifecycle_detached};

mod callback_channel;
pub use callback_channel::CallbackChannel;

mod capabilities;
pub use capabilities::ProtocolCapabilities;

/// Validate the canonical bundle identifier shared by signed manifest
/// authoring and qualified binary resolution.
pub fn validate_bundle_name(name: &str) -> Result<(), VocabularyError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains("..")
        || name.starts_with('.')
        || name.contains(' ')
        || name
            .chars()
            .any(|character| character.is_control() || character == '\0')
    {
        return Err(VocabularyError::InvalidBundleName {
            detail: "must be a single non-hidden identifier without spaces, slashes, `..`, or control characters".to_owned(),
        });
    }
    Ok(())
}
