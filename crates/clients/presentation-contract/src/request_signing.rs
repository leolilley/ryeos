//! The canonical string a key holder signs for an authenticated HTTP request,
//! built in one place for every signer and verifier. It is pure string and hash
//! work (no clock, randomness or I/O), so native clients, the browser client and
//! the daemon verifier all produce and check the same bytes.
//!
//! String to sign:
//!   "ryeos-request-v1\n{METHOD}\n{canonical_path}\n{sha256(body)}\n{timestamp}\n{nonce}\n{audience}"
//!
//! The signature is Ed25519 over the lowercase hex sha256 of that string.
//! Fixed vectors for every function here live in
//! `tests/vectors/request_signing.json`; the signing client and the daemon
//! verifier test against the same file.

use sha2::{Digest, Sha256};

/// First line of the string to sign; names the format.
pub const REQUEST_SIGNING_DOMAIN: &str = "ryeos-request-v1";

fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Lowercase hex sha256 of the request body.
pub fn body_digest(body: &[u8]) -> String {
    sha256_hex(body)
}

/// Request path with its query normalised: query pairs sorted bytewise as
/// `(key, value)`, repeated keys kept, a pair with an empty value written as the
/// bare key, and an absent or empty query dropped.
pub fn canonical_path(path: &str, query: Option<&str>) -> String {
    match query {
        None | Some("") => path.to_string(),
        Some(query) => {
            let mut params: Vec<(&str, &str)> = query
                .split('&')
                .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
                .collect();
            params.sort();
            let sorted: Vec<String> = params
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        (*k).to_string()
                    } else {
                        format!("{k}={v}")
                    }
                })
                .collect();
            format!("{path}?{}", sorted.join("&"))
        }
    }
}

/// [`canonical_path`] for a request target written as `path?query`.
pub fn canonical_path_and_query(path_and_query: &str) -> String {
    match path_and_query.split_once('?') {
        Some((path, query)) => canonical_path(path, Some(query)),
        None => path_and_query.to_string(),
    }
}

/// The exact string a caller signs. `canonical_path` comes from
/// [`canonical_path`], `body_digest` from [`body_digest`], and `audience` is the
/// principal id of the node the request is addressed to.
pub fn string_to_sign(
    method: &str,
    canonical_path: &str,
    body_digest: &str,
    timestamp: &str,
    nonce: &str,
    audience: &str,
) -> String {
    format!(
        "{REQUEST_SIGNING_DOMAIN}\n{}\n{canonical_path}\n{body_digest}\n{timestamp}\n{nonce}\n{audience}",
        method.to_uppercase(),
    )
}

/// Lowercase hex sha256 of [`string_to_sign`]: the bytes the signature covers,
/// and the digest of one exact request.
pub fn content_hash(
    method: &str,
    canonical_path: &str,
    body_digest: &str,
    timestamp: &str,
    nonce: &str,
    audience: &str,
) -> String {
    sha256_hex(
        string_to_sign(
            method,
            canonical_path,
            body_digest,
            timestamp,
            nonce,
            audience,
        )
        .as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("../tests/vectors/request_signing.json");

    #[test]
    fn builder_reproduces_every_vector() {
        let vectors: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
        let list = vectors["vectors"].as_array().unwrap();
        assert!(!list.is_empty());
        for v in list {
            let s = |k: &str| v[k].as_str().unwrap();
            let name = s("name");
            let canon = canonical_path_and_query(s("path_and_query"));
            assert_eq!(canon, s("canonical_path"), "{name}");
            let digest = body_digest(s("body").as_bytes());
            assert_eq!(digest, s("body_digest"), "{name}");
            let ts = v["timestamp"].as_u64().unwrap().to_string();
            let string =
                string_to_sign(s("method"), &canon, &digest, &ts, s("nonce"), s("audience"));
            assert_eq!(string, s("string_to_sign"), "{name}");
            let hash = content_hash(s("method"), &canon, &digest, &ts, s("nonce"), s("audience"));
            assert_eq!(hash, s("content_hash"), "{name}");
        }
    }

    #[test]
    fn empty_query_is_dropped() {
        assert_eq!(canonical_path_and_query("/x?"), "/x");
        assert_eq!(canonical_path("/x", Some("")), "/x");
    }
}
