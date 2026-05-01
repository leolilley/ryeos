#[derive(Debug, thiserror::Error)]
pub enum VocabularyError {
    #[error("unknown stdin shape `{0}`; known: [{1}]")]
    UnknownStdinShape(String, String),
    #[error("unknown stdout shape `{0}`; known: [{1}]")]
    UnknownStdoutShape(String, String),
    #[error("unknown stdout mode `{0}`; known: [{1}]")]
    UnknownStdoutMode(String, String),
    #[error("unknown env injection source `{0}`; known: [{1}]")]
    UnknownEnvInjection(String, String),
    #[error("unknown lifecycle mode `{0}`; known: [{1}]")]
    UnknownLifecycleMode(String, String),
    #[error("unknown callback channel `{0}`; known: [{1}]")]
    UnknownCallbackChannel(String, String),
    #[error("env injection name `{name}` declared twice in protocol")]
    DuplicateEnvInjection { name: String },
    #[error("env injection name `{name}` is reserved (collides with daemon env)")]
    ReservedEnvName { name: String },
    #[error("env injection name `{name}` is not a valid POSIX env identifier")]
    InvalidEnvName { name: String },
    #[error("incompatible (stdout_shape={shape:?}, stdout_mode={mode:?}); see compatibility matrix")]
    StdoutShapeModeMismatch { shape: String, mode: String },
    #[error("lifecycle `{lifecycle:?}` requires allows_detached={expected}, got {actual}")]
    LifecycleDetachedMismatch { lifecycle: String, expected: bool, actual: bool },
    #[error("env injection `{name}` source=callback_token_url requires callback_channel != none")]
    CallbackInjectionWithoutChannel { name: String },
    #[error("callback_channel=http_v1 requires at least one env injection with source=callback_token_url")]
    HttpV1WithoutCallbackInjection,
    #[error("streaming protocol violation: {detail}")]
    StreamingProtocolViolation { detail: String },
}
