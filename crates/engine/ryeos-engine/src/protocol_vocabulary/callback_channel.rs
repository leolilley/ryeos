use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CallbackChannel {
    /// The subprocess gets no daemon callback channel.
    None,
    /// The subprocess calls back to the daemon over the runtime-only Unix
    /// socket (MessagePack frames), authenticated by the callback token.
    RuntimeSocket,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_variants() {
        for ch in [CallbackChannel::None, CallbackChannel::RuntimeSocket] {
            let yaml = serde_yaml::to_string(&ch).unwrap();
            let parsed: CallbackChannel = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(parsed, ch);
        }
    }

    #[test]
    fn reject_unknown() {
        let err = serde_yaml::from_str::<CallbackChannel>("unknown");
        assert!(err.is_err());
    }

    #[test]
    fn runtime_socket_wire_token_and_retired_http_refused() {
        assert_eq!(
            serde_yaml::from_str::<CallbackChannel>("runtime_socket").unwrap(),
            CallbackChannel::RuntimeSocket
        );
        assert!(serde_yaml::from_str::<CallbackChannel>("http").is_err());
    }
}
