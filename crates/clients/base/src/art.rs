//! A reference to a signed picture by content hash.
//!
//! Signed Surfaces and Views name pictures this way. The reference carries no
//! URL and no path: a client turns the hash into a request for the daemon's
//! picture route, and the daemon serves it only when the session's compiled
//! binding names the hash.

use serde::{Deserialize, Serialize};

/// Media types a picture reference may declare.
pub const ART_MEDIA_TYPES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];

/// Whether `value` is a lowercase hex SHA-256.
pub fn is_art_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One picture, by the SHA-256 of its bytes and its declared media type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtRef {
    pub sha256: String,
    pub media_type: String,
}

impl ArtRef {
    /// Check the hash is lowercase hex SHA-256 and the type is a picture type.
    pub fn validate(&self) -> Result<(), String> {
        if !is_art_hash(&self.sha256) {
            return Err("art sha256 must be 64 lowercase hex characters".to_string());
        }
        if !ART_MEDIA_TYPES.contains(&self.media_type.as_str()) {
            return Err(format!(
                "art media_type must be one of {}",
                ART_MEDIA_TYPES.join(", ")
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArtRef {
    sha256: String,
    media_type: String,
}

impl<'de> Deserialize<'de> for ArtRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawArtRef::deserialize(deserializer)?;
        let art = ArtRef {
            sha256: raw.sha256,
            media_type: raw.media_type,
        };
        art.validate().map_err(serde::de::Error::custom)?;
        Ok(art)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_needs_a_lowercase_hash_and_a_picture_type() {
        let ok = serde_json::json!({"sha256": "a".repeat(64), "media_type": "image/webp"});
        assert!(serde_json::from_value::<ArtRef>(ok).is_ok());
        for bad in [
            serde_json::json!({"sha256": "A".repeat(64), "media_type": "image/webp"}),
            serde_json::json!({"sha256": "a".repeat(63), "media_type": "image/webp"}),
            serde_json::json!({"sha256": "../etc/passwd", "media_type": "image/webp"}),
            serde_json::json!({"sha256": "a".repeat(64), "media_type": "image/svg+xml"}),
            serde_json::json!({"sha256": "a".repeat(64), "media_type": "image/gif"}),
            serde_json::json!({"sha256": "a".repeat(64), "media_type": "image/webp", "url": "x"}),
        ] {
            assert!(serde_json::from_value::<ArtRef>(bad).is_err());
        }
    }
}
