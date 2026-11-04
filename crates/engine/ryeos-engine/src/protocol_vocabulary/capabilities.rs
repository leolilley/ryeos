use serde::{Deserialize, Serialize};

/// Protocol dispatch capability bits. Sourced from the verified
/// protocol descriptor, not a hardcoded table.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct ProtocolCapabilities {
    pub allows_pushed_head: bool,
    pub allows_target_site: bool,
    pub allows_detached: bool,
    /// When set together with `allows_pushed_head`, a pushed-head project
    /// source is admitted only for an item whose signed kind-declared
    /// projection declares the pinned-snapshot project source. The protocol
    /// then opens the capability only to items that ask for it, instead of to
    /// every item that selects the protocol.
    #[serde(default)]
    pub pushed_head_requires_declaration: bool,
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
            pushed_head_requires_declaration: false,
        };
        let yaml = serde_yaml::to_string(&caps).unwrap();
        let parsed: ProtocolCapabilities = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, caps);
    }
}
