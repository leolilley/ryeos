//! Versioned attachment object: opaque bytes that a thread event names.
//!
//! The object carries a media type, the byte length and the hash of the blob
//! that holds the bytes. It names no thread, so identical bytes with the same
//! media type are one object wherever they are referenced from. The state
//! layer reads the media type as a bounded token and never interprets it.

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};

use super::thread_snapshot::validate_canonical_hash;

pub const THREAD_ATTACHMENT_KIND: &str = "thread_attachment";
pub const THREAD_ATTACHMENT_SCHEMA: u32 = 1;

/// Longest media type token the object accepts.
pub const MAX_THREAD_ATTACHMENT_MEDIA_TYPE_BYTES: usize = 127;
/// Largest attachment the object may declare. Matches the default per-blob
/// ceiling of an object closure, so a declared attachment is never one the
/// closure walker would refuse by default.
pub const MAX_THREAD_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

/// Immutable attachment facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadAttachment {
    pub media_type: String,
    pub byte_length: u64,
    pub blob_hash: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadAttachmentWire {
    kind: String,
    schema: u32,
    media_type: String,
    byte_length: u64,
    blob_hash: String,
}

/// A media type token: `type/subtype` in lowercase ASCII with the characters
/// registered media types use, bounded in length.
pub fn validate_media_type(media_type: &str) -> anyhow::Result<()> {
    if media_type.is_empty() || media_type.len() > MAX_THREAD_ATTACHMENT_MEDIA_TYPE_BYTES {
        anyhow::bail!(
            "thread_attachment media_type must be 1..={MAX_THREAD_ATTACHMENT_MEDIA_TYPE_BYTES} bytes"
        );
    }
    let Some((top, sub)) = media_type.split_once('/') else {
        anyhow::bail!("thread_attachment media_type must be type/subtype");
    };
    let token = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'+' | b'-' | b'_')
            })
    };
    if !token(top) || !token(sub) {
        anyhow::bail!(
            "thread_attachment media_type must be lowercase ASCII type/subtype tokens: {media_type}"
        );
    }
    Ok(())
}

impl ThreadAttachment {
    pub const SCHEMA: u32 = THREAD_ATTACHMENT_SCHEMA;

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_media_type(&self.media_type)?;
        validate_canonical_hash("thread_attachment blob_hash", &self.blob_hash)?;
        if self.byte_length > MAX_THREAD_ATTACHMENT_BYTES {
            anyhow::bail!(
                "thread_attachment byte_length {} exceeds {MAX_THREAD_ATTACHMENT_BYTES}",
                self.byte_length
            );
        }
        Ok(())
    }

    pub fn to_value(&self) -> Value {
        json!({
            "kind": THREAD_ATTACHMENT_KIND,
            "schema": Self::SCHEMA,
            "media_type": self.media_type,
            "byte_length": self.byte_length,
            "blob_hash": self.blob_hash,
        })
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let wire: ThreadAttachmentWire = serde_json::from_value(value.clone())
            .context("failed to deserialize thread_attachment schema 1")?;
        if wire.kind != THREAD_ATTACHMENT_KIND {
            anyhow::bail!(
                "thread_attachment kind mismatch: expected {THREAD_ATTACHMENT_KIND}, got {}",
                wire.kind
            );
        }
        if wire.schema != Self::SCHEMA {
            anyhow::bail!(
                "thread_attachment schema mismatch: expected {}, got {}",
                Self::SCHEMA,
                wire.schema
            );
        }
        let attachment = Self {
            media_type: wire.media_type,
            byte_length: wire.byte_length,
            blob_hash: wire.blob_hash,
        };
        attachment.validate()?;
        Ok(attachment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ThreadAttachment {
        ThreadAttachment {
            media_type: "application/json".to_owned(),
            byte_length: 12,
            blob_hash: "ab".repeat(32),
        }
    }

    #[test]
    fn a_valid_attachment_round_trips() {
        let attachment = sample();
        assert_eq!(
            ThreadAttachment::from_value(&attachment.to_value()).unwrap(),
            attachment
        );
    }

    #[test]
    fn the_object_refuses_unknown_fields_and_other_kinds() {
        let mut value = sample().to_value();
        value["extra"] = json!(1);
        assert!(ThreadAttachment::from_value(&value).is_err());
        let mut value = sample().to_value();
        value["kind"] = json!("project_file");
        assert!(ThreadAttachment::from_value(&value).is_err());
        let mut value = sample().to_value();
        value["schema"] = json!(2);
        assert!(ThreadAttachment::from_value(&value).is_err());
    }

    #[test]
    fn media_types_are_bounded_lowercase_tokens() {
        for bad in ["", "image", "Image/PNG", "image/", "/png", "image/p ng"] {
            assert!(validate_media_type(bad).is_err(), "{bad:?}");
        }
        assert!(validate_media_type("image/svg+xml").is_ok());
        assert!(validate_media_type(&format!("a/{}", "b".repeat(200))).is_err());
    }

    #[test]
    fn hashes_and_lengths_are_checked() {
        let mut attachment = sample();
        attachment.blob_hash = "AB".repeat(32);
        assert!(attachment.validate().is_err());
        let mut attachment = sample();
        attachment.byte_length = MAX_THREAD_ATTACHMENT_BYTES + 1;
        assert!(attachment.validate().is_err());
    }
}
