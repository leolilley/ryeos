use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CallbackChannel {
    None,
    HttpV1,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_variants() {
        for ch in [CallbackChannel::None, CallbackChannel::HttpV1] {
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
}
