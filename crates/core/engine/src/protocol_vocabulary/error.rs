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
    #[error("callback_channel=http_v1 requires at least one env injection with a callback source (callback_token_url, callback_socket_path, or callback_token)")]
    HttpV1WithoutCallbackInjection,
    #[error("env injection `{name}` has a callback source but callback_channel is none")]
    CallbackInjectionWithoutChannel { name: String },
}
