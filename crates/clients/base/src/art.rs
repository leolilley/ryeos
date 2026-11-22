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

/// The item-ref prefix of a picture record.
pub const PICTURE_REF_PREFIX: &str = "picture:";

/// The row word a subject carries when its picture binding failed to resolve:
/// the card draws the flat mark and no other tier is consulted.
pub const PICTURE_STATE_UNAVAILABLE: &str = "unavailable";

/// Why a picture binding is malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PictureBindingError {
    /// The ref does not name an item of the picture kind.
    NotAPictureRef,
    /// The ref's id is empty or holds a character outside `[a-z0-9_-]` and
    /// `/` separators between non-empty segments.
    MalformedRef,
    /// The role is empty or holds a character outside `[a-z0-9_-]`.
    MalformedRole,
}

impl std::fmt::Display for PictureBindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAPictureRef => "a picture binding must name an item of kind picture",
            Self::MalformedRef => "the picture ref's id is malformed",
            Self::MalformedRole => "the picture role must be a short lowercase word",
        })
    }
}

impl std::error::Error for PictureBindingError {}

fn is_word(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

/// A picture named by item ref, never by hash: the daemon resolves the ref to
/// the signed record and supplies the hash, so a re-signed picture follows
/// the record. `role` picks one rendition by its opaque role word; absent
/// means the record's first rendition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PictureBinding {
    pub picture: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl PictureBinding {
    /// Check the ref is a well-formed picture item ref and the role a word.
    pub fn validate(&self) -> Result<(), PictureBindingError> {
        let id = self
            .picture
            .strip_prefix(PICTURE_REF_PREFIX)
            .ok_or(PictureBindingError::NotAPictureRef)?;
        if id.len() > 200 || id.split('/').any(|segment| !is_word(segment, 100)) {
            return Err(PictureBindingError::MalformedRef);
        }
        if let Some(role) = &self.role
            && !is_word(role, 64)
        {
            return Err(PictureBindingError::MalformedRole);
        }
        Ok(())
    }

    /// Path of the signed record under a bundle's `.ai`, as the picture index
    /// names it: `pictures/<id>.yaml`. `None` for a malformed binding.
    pub fn record_path(&self) -> Option<String> {
        self.validate().ok()?;
        Some(format!(
            "pictures/{}.yaml",
            self.picture.strip_prefix(PICTURE_REF_PREFIX)?
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPictureBinding {
    picture: String,
    #[serde(default)]
    role: Option<String>,
}

impl<'de> Deserialize<'de> for PictureBinding {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawPictureBinding::deserialize(deserializer)?;
        let binding = PictureBinding {
            picture: raw.picture,
            role: raw.role,
        };
        binding.validate().map_err(serde::de::Error::custom)?;
        Ok(binding)
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

    #[test]
    fn a_picture_binding_names_a_picture_item_and_an_optional_role_word() {
        let ok = serde_json::json!({"picture": "picture:ryeos/atlas-grey", "role": "hero"});
        let binding = serde_json::from_value::<PictureBinding>(ok).unwrap();
        assert_eq!(
            binding.record_path().as_deref(),
            Some("pictures/ryeos/atlas-grey.yaml")
        );
        let bare = serde_json::json!({"picture": "picture:a"});
        assert!(serde_json::from_value::<PictureBinding>(bare).is_ok());
        for bad in [
            serde_json::json!({"picture": "view:ryeos/x"}),
            serde_json::json!({"picture": "ryeos/atlas-grey"}),
            serde_json::json!({"picture": "picture:"}),
            serde_json::json!({"picture": "picture:../etc"}),
            serde_json::json!({"picture": "picture:a//b"}),
            serde_json::json!({"picture": "picture:/a"}),
            serde_json::json!({"picture": "picture:A"}),
            serde_json::json!({"picture": "picture:a", "role": ""}),
            serde_json::json!({"picture": "picture:a", "role": "Hero"}),
            serde_json::json!({"picture": "picture:a", "sha256": "x"}),
            serde_json::json!({"picture": "picture:a", "url": "x"}),
        ] {
            assert!(serde_json::from_value::<PictureBinding>(bad).is_err());
        }
    }
}
