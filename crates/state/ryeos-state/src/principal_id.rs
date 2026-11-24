//! Principal identity: what owns a project namespace.
//!
//! A principal is not a key. Keys authenticate requests and sign items; a
//! principal is what those keys speak for when they act in a project
//! namespace. Its id is the sha256 of the canonical bytes of an inception
//! record, written `pid:v1:<64 lowercase hex>` in text and
//! `pid-v1-<64 lowercase hex>` as a storage component (a ref path segment or
//! a directory name).
//!
//! Every key has a solo principal, a pure function of its fingerprint with no
//! record stored anywhere ([`PrincipalId::solo`]). Which principal a key
//! speaks for on a node is decided only by the node's resolver over its own
//! node-signed signer registry; nothing in this module decides it, and a
//! principal id never authorizes anything by itself.
//!
//! A principal id can never equal a key fingerprint: the text forms differ
//! by prefix and the value is a digest of a structured record, not of a key.

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Text prefix of a version 1 principal id.
pub const PRINCIPAL_ID_V1_PREFIX: &str = "pid:v1:";

/// Storage-component prefix of a version 1 principal id.
pub const PRINCIPAL_STORAGE_V1_PREFIX: &str = "pid-v1-";

/// Schema string of a version 1 principal inception record.
pub const PRINCIPAL_INCEPTION_V1_SCHEMA: &str = "ryeos/principal-inception/v1";

/// A principal id, `pid:v1:<64 lowercase hex>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PrincipalId {
    digest: String,
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl PrincipalId {
    /// The solo principal of the key whose fingerprint is `fingerprint`
    /// (`fp:<64 lowercase hex>`).
    ///
    /// The inception body is exactly
    /// `{"key_fingerprint":"<hex>","kind":"key","schema":"ryeos/principal-inception/v1"}`:
    /// keys in that order, no whitespace, the fingerprint as bare lowercase
    /// hex. The id is the sha256 of those UTF-8 bytes. This encoding is a
    /// format for ever; the golden vectors in the tests pin it.
    ///
    /// Only the node's ownership resolver may call this. Every other site
    /// obtains an owner from the resolver, so "the owner is the
    /// authenticating key" cannot reappear as a convention; a guard test
    /// enforces the caller list.
    pub fn solo(fingerprint: &str) -> Result<Self> {
        let hex = crate::refs::principal_storage_key(fingerprint)
            .with_context(|| format!("solo principal of {fingerprint:?}"))?;
        Ok(Self {
            digest: lillux::cas::sha256_hex(&solo_inception_bytes(hex)),
        })
    }

    /// The text form, `pid:v1:<64 lowercase hex>`.
    pub fn as_text(&self) -> String {
        format!("{PRINCIPAL_ID_V1_PREFIX}{}", self.digest)
    }

    /// The storage component, `pid-v1-<64 lowercase hex>`.
    pub fn storage_component(&self) -> String {
        format!("{PRINCIPAL_STORAGE_V1_PREFIX}{}", self.digest)
    }

    /// Parse a storage component, `pid-v1-<64 lowercase hex>`.
    pub fn from_storage_component(value: &str) -> Result<Self> {
        let Some(digest) = value.strip_prefix(PRINCIPAL_STORAGE_V1_PREFIX) else {
            bail!("principal storage component must be pid-v1-<64 lowercase hex>: {value:?}");
        };
        if !is_lower_hex_64(digest) {
            bail!("principal storage component must be pid-v1-<64 lowercase hex>: {value:?}");
        }
        Ok(Self {
            digest: digest.to_owned(),
        })
    }
}

/// The canonical inception bytes of a solo principal for bare fingerprint
/// hex. Private: the only producer of a solo id is [`PrincipalId::solo`].
fn solo_inception_bytes(fingerprint_hex: &str) -> Vec<u8> {
    format!(
        "{{\"key_fingerprint\":\"{fingerprint_hex}\",\"kind\":\"key\",\"schema\":\"{PRINCIPAL_INCEPTION_V1_SCHEMA}\"}}"
    )
    .into_bytes()
}

impl FromStr for PrincipalId {
    type Err = anyhow::Error;

    /// Accepts only `pid:v1:<64 lowercase hex>`. A key fingerprint
    /// (`fp:…`) is refused: a key is never an owner.
    fn from_str(value: &str) -> Result<Self> {
        if value.starts_with("fp:") {
            bail!(
                "principal_id_is_a_key_fingerprint: {value:?} is a key fingerprint; a project owner is a principal id (pid:v1:<64 lowercase hex>)"
            );
        }
        let Some(digest) = value.strip_prefix(PRINCIPAL_ID_V1_PREFIX) else {
            bail!("principal id must be pid:v1:<64 lowercase hex>: {value:?}");
        };
        if !is_lower_hex_64(digest) {
            bail!("principal id must be pid:v1:<64 lowercase hex>: {value:?}");
        }
        Ok(Self {
            digest: digest.to_owned(),
        })
    }
}

impl TryFrom<String> for PrincipalId {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl From<PrincipalId> for String {
    fn from(value: PrincipalId) -> Self {
        value.as_text()
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{PRINCIPAL_ID_V1_PREFIX}{}", self.digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vectors: fp -> pid. These pin the solo inception encoding for
    /// ever; ryeos-docs carries the same table. Never edit an expected
    /// value: a different encoding is a new version, not a fix.
    const GOLDEN: &[(&str, &str)] = &[
        (
            "fp:0000000000000000000000000000000000000000000000000000000000000000",
            "pid:v1:62089d38725f465fca99bab850bf34953900e105670ca36f65f09e264d3dd6e5",
        ),
        (
            "fp:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea",
            "pid:v1:2576bb08a06af6a1e8cdad0a44fd550c8c95f177d2992046b6da24fe4ddfe30e",
        ),
    ];

    #[test]
    fn solo_inception_bytes_are_exact() {
        assert_eq!(
            String::from_utf8(solo_inception_bytes(&"ab".repeat(32))).unwrap(),
            format!(
                "{{\"key_fingerprint\":\"{}\",\"kind\":\"key\",\"schema\":\"ryeos/principal-inception/v1\"}}",
                "ab".repeat(32)
            )
        );
    }

    #[test]
    fn golden_solo_vectors_hold() {
        for (fingerprint, expected) in GOLDEN {
            let id = PrincipalId::solo(fingerprint).unwrap();
            assert_eq!(id.to_string(), *expected, "solo({fingerprint})");
        }
    }

    #[test]
    fn text_and_storage_forms_round_trip() {
        let id = PrincipalId::solo(&format!("fp:{}", "cd".repeat(32))).unwrap();
        let text = id.to_string();
        assert!(text.starts_with("pid:v1:"));
        assert_eq!(text.parse::<PrincipalId>().unwrap(), id);
        let storage = id.storage_component();
        assert!(storage.starts_with("pid-v1-"));
        assert_eq!(PrincipalId::from_storage_component(&storage).unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{text}\""));
        assert_eq!(serde_json::from_str::<PrincipalId>(&json).unwrap(), id);
    }

    #[test]
    fn a_key_fingerprint_never_parses_as_a_principal() {
        let fingerprint = format!("fp:{}", "ab".repeat(32));
        let error = fingerprint.parse::<PrincipalId>().unwrap_err();
        assert!(
            format!("{error:#}").contains("principal_id_is_a_key_fingerprint"),
            "{error:#}"
        );
        assert!(PrincipalId::from_storage_component(&"ab".repeat(32)).is_err());
        assert!(serde_json::from_str::<PrincipalId>(&format!("\"{fingerprint}\"")).is_err());
    }

    #[test]
    fn malformed_principal_ids_are_refused() {
        for bad in [
            "pid:v1:",
            "pid:v2:0000000000000000000000000000000000000000000000000000000000000000",
            "pid:v1:00000000000000000000000000000000000000000000000000000000000000",
            "pid:v1:000000000000000000000000000000000000000000000000000000000000000G",
            "pid:v1:ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB",
            " pid:v1:0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert!(bad.parse::<PrincipalId>().is_err(), "{bad:?} parsed");
        }
        assert!(
            PrincipalId::from_storage_component(
                "pid:v1:0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );
    }

    #[test]
    fn solo_refuses_a_non_fingerprint() {
        assert!(PrincipalId::solo(&"ab".repeat(32)).is_err());
        assert!(PrincipalId::solo(&format!("fp:{}", "AB".repeat(32))).is_err());
    }
}
