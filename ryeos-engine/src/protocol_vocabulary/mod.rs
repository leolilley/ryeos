pub mod error;
pub use error::VocabularyError;

mod stdin_shape;
pub use stdin_shape::{build_stdin, StdinShape};

mod stdout_shape;
pub use stdout_shape::{
    decode_stdout_frame, decode_stdout_terminal,
    DecodedFrame, DecodedStdout, StreamingChunk, StreamingChunkKind, StdoutShape,
};

mod stdout_mode;
pub use stdout_mode::{is_compatible_shape_mode, StdoutMode};

mod env_injection;
pub use env_injection::{
    is_reserved_env_name, produce_env_value, validate_env_name,
    EnvInjection, EnvInjectionSource, RESERVED_ENV_NAMES,
};

mod lifecycle;
pub use lifecycle::{is_compatible_lifecycle_detached, LifecycleMode};

mod callback_channel;
pub use callback_channel::CallbackChannel;

mod capabilities;
pub use capabilities::ProtocolCapabilities;
