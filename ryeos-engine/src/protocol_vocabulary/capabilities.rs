use serde::{Deserialize, Serialize};

/// Protocol dispatch capability bits. Replaces the old
/// `DispatchCapabilities` — same field names, same semantics,
/// but sourced from the verified protocol descriptor, not a
/// hardcoded table.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProtocolCapabilities {
    pub allows_pushed_head: bool,
    pub allows_target_site: bool,
    pub allows_detached: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let caps = ProtocolCapabilities {
            allows_pushed_head: true,
            allows_target_site: false,
            allows_detached: true,
        };
        let yaml = serde_yaml::to_string(&caps).unwrap();
        let parsed: ProtocolCapabilities = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, caps);
    }
}
