use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct SignatureHeader {
    pub timestamp: String,
    pub content_hash: String,
    pub signature_b64: String,
    pub signer_fingerprint: String,
}

pub fn compute_fingerprint(key: &VerifyingKey) -> String {
    let hash = Sha256::digest(key.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hash.iter() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub fn content_hash(content: &str) -> String {
    crate::cas::sha256_hex(content.as_bytes())
}

/// Like [`sign_content`] but takes the signed-at timestamp explicitly,
/// for byte-deterministic test fixtures and snapshot golden files.
///
/// Production code paths should keep using [`sign_content`] which fills
/// in `iso8601_now()`. Only test support and reproducible-build tools
/// should call this.
pub fn sign_content_at(
    body: &str,
    signing_key: &SigningKey,
    prefix: &str,
    suffix: Option<&str>,
    signed_at: &str,
) -> String {
    let hash = content_hash(body);
    let signature: ed25519_dalek::Signature = signing_key.sign(hash.as_bytes());
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
    let fp = compute_fingerprint(&signing_key.verifying_key());

    let sig_line = match suffix {
        Some(s) => format!("{prefix} rye:signed:{signed_at}:{hash}:{sig_b64}:{fp} {s}"),
        None => format!("{prefix} rye:signed:{signed_at}:{hash}:{sig_b64}:{fp}"),
    };

    format!("{sig_line}\n{body}")
}

pub fn sign_content(
    body: &str,
    signing_key: &SigningKey,
    prefix: &str,
    suffix: Option<&str>,
) -> String {
    sign_content_at(body, signing_key, prefix, suffix, &crate::time::iso8601_now())
}

pub fn parse_signature_line(
    line: &str,
    prefix: &str,
    suffix: Option<&str>,
) -> Option<SignatureHeader> {
    let trimmed = line.trim();

    let after_prefix = trimmed.strip_prefix(prefix)?.trim_start();

    let payload_area = after_prefix.strip_prefix("rye:signed:")?;

    let payload = match suffix {
        Some(s) => payload_area.trim_end().strip_suffix(s)?.trim_end(),
        None => payload_area.trim_end(),
    };

    let parts: Vec<&str> = payload.rsplitn(4, ':').collect();
    if parts.len() != 4 {
        return None;
    }

    Some(SignatureHeader {
        timestamp: parts[3].to_string(),
        content_hash: parts[2].to_string(),
        signature_b64: parts[1].to_string(),
        signer_fingerprint: parts[0].to_string(),
    })
}

pub fn strip_signature_lines(content: &str) -> String {
    let has_trailing_newline = content.ends_with('\n');
    let result: String = content
        .lines()
        .filter(|line| !line.contains("rye:signed:"))
        .collect::<Vec<_>>()
        .join("\n");
    if has_trailing_newline && !result.is_empty() {
        format!("{result}\n")
    } else {
        result
    }
}

/// Envelope-aware variant of [`strip_signature_lines`].
///
/// Only strips signature lines whose comment envelope matches the
/// supplied `prefix` (and `suffix`, when present). Lines containing the
/// `rye:signed:` marker but wrapped in a *different* envelope (e.g. a
/// `# rye:signed:...` comment in the body of a markdown file whose
/// envelope is `<!-- ... -->`) are left intact.
///
/// This is the version every parser dispatcher should use: each kind
/// declares its own envelope, and only that envelope's signature line
/// is part of the bootstrap layer to strip before parsing.
pub fn strip_signature_lines_with_envelope(
    content: &str,
    prefix: &str,
    suffix: Option<&str>,
) -> String {
    let has_trailing_newline = content.ends_with('\n');
    let result: String = content
        .lines()
        .filter(|line| !is_signature_line(line, prefix, suffix))
        .collect::<Vec<_>>()
        .join("\n");
    if has_trailing_newline && !result.is_empty() {
        format!("{result}\n")
    } else {
        result
    }
}

pub fn verify_signature(
    content_hash: &str,
    signature_b64: &str,
    verifying_key: &VerifyingKey,
) -> bool {
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64)
        .or_else(|_| {
            let stripped = signature_b64.trim_end_matches('=');
            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(stripped)
        })
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(signature_b64));

    match sig_bytes {
        Ok(bytes) => match ed25519_dalek::Signature::from_slice(&bytes) {
            Ok(sig) => verifying_key.verify(content_hash.as_bytes(), &sig).is_ok(),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

pub fn content_hash_after_signature(
    content: &str,
    prefix: &str,
    suffix: Option<&str>,
    after_shebang: bool,
) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let candidates: Vec<usize> = if after_shebang {
        let mut c = Vec::new();
        if lines.len() > 1 {
            c.push(1);
        }
        c.push(0);
        c
    } else {
        vec![0]
    };

    for idx in candidates {
        if is_signature_line(lines[idx], prefix, suffix) {
            let mut offset = 0;
            for i in 0..=idx {
                offset += lines[i].len();
                let pos = offset;
                if pos < content.len() {
                    let byte = content.as_bytes()[pos];
                    if byte == b'\n' {
                        offset += 1;
                    } else if byte == b'\r' {
                        offset += 1;
                        if offset < content.len() && content.as_bytes()[offset] == b'\n' {
                            offset += 1;
                        }
                    }
                }
            }
            let after = &content[offset..];
            return Some(crate::cas::sha256_hex(after.as_bytes()));
        }
    }

    None
}

fn is_signature_line(line: &str, prefix: &str, suffix: Option<&str>) -> bool {
    let trimmed = line.trim();
    let after_prefix = match trimmed.strip_prefix(prefix) {
        Some(s) => s.trim_start(),
        None => return false,
    };

    let payload_area = match suffix {
        Some(s) => match after_prefix.trim_end().strip_suffix(s) {
            Some(inner) => inner.trim_end(),
            None => return false,
        },
        None => after_prefix.trim_end(),
    };

    payload_area.starts_with("rye:signed:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_parse_round_trip() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "print('hello')\n";
        let signed = sign_content(body, &sk, "#", None);
        let first_line = signed.lines().next().unwrap();
        let header = parse_signature_line(first_line, "#", None).unwrap();
        assert_eq!(header.content_hash, content_hash(body));
        assert_eq!(
            header.signer_fingerprint,
            compute_fingerprint(&sk.verifying_key())
        );
    }

    #[test]
    fn sign_with_html_envelope() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "# Hello\n";
        let signed = sign_content(body, &sk, "<!--", Some("-->"));
        let first_line = signed.lines().next().unwrap();
        assert!(first_line.starts_with("<!-- rye:signed:"));
        assert!(first_line.ends_with("-->"));
        let header = parse_signature_line(first_line, "<!--", Some("-->")).unwrap();
        assert_eq!(header.content_hash, content_hash(body));
    }

    #[test]
    fn strip_signature_lines_removes_signed_lines() {
        let content = "# rye:signed:2026-04-10T00:00:00Z:abc:sig:fp\nprint('hello')\n";
        assert_eq!(strip_signature_lines(content), "print('hello')\n");
    }

    #[test]
    fn compute_fingerprint_is_full_sha256_hex() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let fp = compute_fingerprint(&sk.verifying_key());
        assert_eq!(fp.len(), 64);
    }

    #[test]
    fn verify_signature_round_trip() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "test content\n";
        let hash = content_hash(body);
        let sig: ed25519_dalek::Signature = sk.sign(hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        assert!(verify_signature(&hash, &sig_b64, &sk.verifying_key()));
    }

    #[test]
    fn verify_signature_rejects_tampered() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "test content\n";
        let hash = content_hash(body);
        let sig: ed25519_dalek::Signature = sk.sign(hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        assert!(!verify_signature(
            "wrong_hash",
            &sig_b64,
            &sk.verifying_key()
        ));
    }

    #[test]
    fn content_hash_after_signature_finds_content() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "body content\n";
        let signed = sign_content(body, &sk, "#", None);
        let hash = content_hash_after_signature(&signed, "#", None, false).unwrap();
        assert_eq!(hash, content_hash(body));
    }

    #[test]
    fn content_hash_after_signature_with_html_envelope() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "markdown content\n";
        let signed = sign_content(body, &sk, "<!--", Some("-->"));
        let hash = content_hash_after_signature(&signed, "<!--", Some("-->"), false).unwrap();
        assert_eq!(hash, content_hash(body));
    }

    #[test]
    fn content_hash_after_signature_returns_none_when_absent() {
        assert!(content_hash_after_signature("no sig here", "#", None, false).is_none());
    }

    #[test]
    fn content_hash_after_signature_with_shebang() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "actual content\n";
        let sig_line = {
            let hash = content_hash(body);
            let sig: ed25519_dalek::Signature = sk.sign(hash.as_bytes());
            let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
            let fp = compute_fingerprint(&sk.verifying_key());
            format!("# rye:signed:2026-04-10T00:00:00Z:{hash}:{sig_b64}:{fp}")
        };
        let content = format!("#!/usr/bin/env python3\n{sig_line}\n{body}");
        let hash = content_hash_after_signature(&content, "#", None, true).unwrap();
        assert_eq!(hash, content_hash(body));
    }

    #[test]
    fn strip_signature_lines_preserves_normal_lines() {
        let content = "# normal comment\ncode here\n";
        assert_eq!(strip_signature_lines(content), content);
    }

    #[test]
    fn sign_content_at_is_byte_deterministic() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "deterministic body\n";
        let a = sign_content_at(body, &sk, "#", None, "2026-01-01T00:00:00Z");
        let b = sign_content_at(body, &sk, "#", None, "2026-01-01T00:00:00Z");
        assert_eq!(a, b, "same inputs must produce identical bytes");
    }
}
