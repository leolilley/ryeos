//! The lane marker a node records on a bundle registration.
//!
//! A bundle that arrived through a publication pull sits in the publication
//! lane: the publisher signed its bytes and the node pinned that key, but no
//! release node built, qualified or attested it. The marker is part of the
//! node-signed registration, so it cannot be added or removed without the node
//! key. It states where the bytes came from; it grants nothing.

use serde::{Deserialize, Serialize};

/// The only lane name this build writes.
pub const BUNDLE_LANE_PUBLICATION: &str = "publication";

/// Provenance of a bundle registered through the publication pull.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleLane {
    /// Lane name; always [`BUNDLE_LANE_PUBLICATION`].
    pub name: String,
    /// Fingerprint of the key that signed the pulled bytes.
    pub publisher_fingerprint: String,
    /// Hash of the publication record the bundle was pulled from.
    pub record_hash: String,
    /// Fingerprint of the node key that served the bytes, as the puller pinned it.
    pub node_fingerprint: String,
    /// Bundle version the publication manifest declared.
    pub bundle_version: String,
    /// Position in a publication index, when the pull came through one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_sequence: Option<u64>,
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl BundleLane {
    /// A publication-lane marker with no index position.
    pub fn publication(
        publisher_fingerprint: impl Into<String>,
        record_hash: impl Into<String>,
        node_fingerprint: impl Into<String>,
        bundle_version: impl Into<String>,
    ) -> Self {
        Self {
            name: BUNDLE_LANE_PUBLICATION.to_string(),
            publisher_fingerprint: publisher_fingerprint.into(),
            record_hash: record_hash.into(),
            node_fingerprint: node_fingerprint.into(),
            bundle_version: bundle_version.into(),
            index_sequence: None,
        }
    }

    /// Check the marker's shape. The error names the offending field.
    pub fn validate_shape(&self) -> Result<(), String> {
        if self.name != BUNDLE_LANE_PUBLICATION {
            return Err(format!(
                "bundle lane name must be \"{BUNDLE_LANE_PUBLICATION}\", got {:?}",
                self.name
            ));
        }
        for (field, value) in [
            ("publisher_fingerprint", &self.publisher_fingerprint),
            ("record_hash", &self.record_hash),
            ("node_fingerprint", &self.node_fingerprint),
        ] {
            if !is_hex64(value) {
                return Err(format!(
                    "bundle lane {field} must be 64 lowercase hex characters"
                ));
            }
        }
        if !(1..=64).contains(&self.bundle_version.len())
            || !self.bundle_version.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err("bundle lane bundle_version must be 1 to 64 printable bytes".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane() -> BundleLane {
        BundleLane::publication("a".repeat(64), "b".repeat(64), "c".repeat(64), "1.2.3")
    }

    #[test]
    fn publication_lane_is_valid() {
        let lane = lane();
        assert_eq!(lane.name, BUNDLE_LANE_PUBLICATION);
        assert_eq!(lane.index_sequence, None);
        lane.validate_shape().unwrap();
    }

    #[test]
    fn wrong_name_is_refused() {
        let mut lane = lane();
        lane.name = "release".to_string();
        assert!(lane.validate_shape().unwrap_err().contains("name"));
    }

    #[test]
    fn bad_hex_is_refused() {
        for field in 0..3 {
            for bad in ["a".repeat(63), "A".repeat(64), "g".repeat(64)] {
                let mut lane = lane();
                match field {
                    0 => lane.publisher_fingerprint = bad,
                    1 => lane.record_hash = bad,
                    _ => lane.node_fingerprint = bad,
                }
                assert!(lane.validate_shape().is_err());
            }
        }
    }

    #[test]
    fn bad_version_is_refused() {
        for bad in [String::new(), "has space".to_string(), "v".repeat(65)] {
            let mut lane = lane();
            lane.bundle_version = bad;
            assert!(
                lane.validate_shape()
                    .unwrap_err()
                    .contains("bundle_version")
            );
        }
    }

    #[test]
    fn json_round_trip_omits_absent_index_and_denies_unknown_fields() {
        let lane = lane();
        let json = serde_json::to_value(&lane).unwrap();
        assert!(json.get("index_sequence").is_none());
        assert_eq!(
            serde_json::from_value::<BundleLane>(json.clone()).unwrap(),
            lane
        );
        let mut extra = json;
        extra["surprise"] = serde_json::json!(1);
        assert!(serde_json::from_value::<BundleLane>(extra).is_err());
    }
}
