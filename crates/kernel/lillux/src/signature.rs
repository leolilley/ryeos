use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

/// Canonical prefix for the Ed25519 signature envelope.
///
/// Every signed item carries this marker so the loader can distinguish
/// signature lines from body content. Changing this value invalidates
/// all existing signatures — only change between major versions.
pub const SIGNATURE_PREFIX: &str = "ryeos:signed:";

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
    sign_content_at_with_options(body, signing_key, prefix, suffix, signed_at, false)
}

pub fn sign_content_at_with_options(
    body: &str,
    signing_key: &SigningKey,
    prefix: &str,
    suffix: Option<&str>,
    signed_at: &str,
    after_shebang: bool,
) -> String {
    let (shebang, signed_body) = split_shebang_body(body, after_shebang);
    let newline = preferred_newline(body);
    let hash = content_hash(signed_body);
    let signature: ed25519_dalek::Signature = signing_key.sign(hash.as_bytes());
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
    let fp = compute_fingerprint(&signing_key.verifying_key());

    let sig_line = match suffix {
        Some(s) => format!("{prefix} {SIGNATURE_PREFIX}{signed_at}:{hash}:{sig_b64}:{fp} {s}"),
        None => format!("{prefix} {SIGNATURE_PREFIX}{signed_at}:{hash}:{sig_b64}:{fp}"),
    };

    match shebang {
        Some(shebang) => format!("{shebang}{newline}{sig_line}{newline}{signed_body}"),
        None => format!("{sig_line}{newline}{signed_body}"),
    }
}

pub fn sign_content(
    body: &str,
    signing_key: &SigningKey,
    prefix: &str,
    suffix: Option<&str>,
) -> String {
    sign_content_with_options(body, signing_key, prefix, suffix, false)
}

struct FixedSignedAt { bytes: [u8; 32], used: usize }
impl std::fmt::Write for FixedSignedAt {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        if text.len() > self.bytes.len().saturating_sub(self.used) { return Err(std::fmt::Error); }
        self.bytes[self.used..self.used + text.len()].copy_from_slice(text.as_bytes());
        self.used += text.len();
        Ok(())
    }
}
fn fixed_hex_digest(input: &[u8]) -> [u8; 64] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(input);
    let mut output = [0; 64];
    for (index, byte) in digest.iter().copied().enumerate() {
        output[index * 2] = HEX[(byte >> 4) as usize];
        output[index * 2 + 1] = HEX[(byte & 15) as usize];
    }
    output
}
/// Sign with the ordinary production timestamp only when the complete output
/// fits the caller's byte ceiling. This checks the exact envelope dimensions
/// before computing a signature; it does not supply signing authority or fund
/// the body, cryptographic work, or returned storage.
pub fn sign_content_bounded(
    body: &str,
    signing_key: &SigningKey,
    prefix: &str,
    suffix: Option<&str>,
    maximum_bytes: usize,
) -> Result<String, &'static str> {
    let newline = preferred_newline(body);
    let mut signed_at = FixedSignedAt { bytes: [0; 32], used: 0 };
    crate::time::iso8601_now_into(&mut signed_at).map_err(|_| "signature timestamp exceeds fixed storage")?;
    // Same envelope as sign_content_at_with_options with after_shebang=false:
    // prefix + space + marker + timestamp + three colons + SHA256 hex (64)
    // + Ed25519 base64 (88) + fingerprint hex (64) + optional suffix + newline.
    let envelope = prefix.len().checked_add(1)
        .and_then(|n| n.checked_add(SIGNATURE_PREFIX.len()))
        .and_then(|n| n.checked_add(signed_at.used))
        .and_then(|n| n.checked_add(3 + 64 + 88 + 64))
        .and_then(|n| n.checked_add(newline.len()))
        .and_then(|n| match suffix { Some(s) => n.checked_add(1)?.checked_add(s.len()), None => Some(n) })
        .ok_or("signature envelope size overflow")?;
    let expected = body.len().checked_add(envelope)
        .ok_or("signed content size overflow")?;
    if expected > maximum_bytes {
        return Err("signed content exceeds byte ceiling");
    }
    std::alloc::Layout::array::<u8>(expected).map_err(|_| "signed output layout overflow")?;
    // Exactly one output allocation. All hash/signature/header temporaries are
    // fixed stack arrays; no format!/String or growing base64 buffer is used.
    let mut output = Box::<[u8]>::new_uninit_slice(expected);
    for slot in output.iter_mut() { slot.write(0); }
    // SAFETY: every output byte was initialized above; only bounded slices are
    // written below, and the final String checks the complete UTF-8 encoding.
    let mut output = unsafe { output.assume_init() };
    let hash = fixed_hex_digest(body.as_bytes());
    let signature = signing_key.sign(&hash);
    let mut encoded = [0; 88];
    let encoded_len = base64::engine::general_purpose::STANDARD
        .encode_slice(signature.to_bytes(), &mut encoded)
        .map_err(|_| "signature base64 exceeds fixed storage")?;
    if encoded_len != encoded.len() { return Err("signature base64 length changed"); }
    let fingerprint = fixed_hex_digest(signing_key.verifying_key().as_bytes());
    let mut used = 0;
    let mut append = |bytes: &[u8]| -> Result<(), &'static str> {
        if bytes.len() > output.len().saturating_sub(used) { return Err("signature envelope exceeds checked dimensions"); }
        output[used..used + bytes.len()].copy_from_slice(bytes);
        used += bytes.len();
        Ok(())
    };
    for part in [prefix.as_bytes(), b" ", SIGNATURE_PREFIX.as_bytes(), &signed_at.bytes[..signed_at.used],
        b":", &hash, b":", &encoded, b":", &fingerprint] { append(part)?; }
    if let Some(suffix) = suffix { append(b" ")?; append(suffix.as_bytes())?; }
    append(newline.as_bytes())?;
    append(body.as_bytes())?;
    drop(append);
    if used != expected { return Err("signature envelope differs from checked dimensions"); }
    String::from_utf8(output.into_vec()).map_err(|_| "signed content is not UTF-8")
}

pub fn sign_content_with_options(
    body: &str,
    signing_key: &SigningKey,
    prefix: &str,
    suffix: Option<&str>,
    after_shebang: bool,
) -> String {
    sign_content_at_with_options(
        body,
        signing_key,
        prefix,
        suffix,
        &crate::time::iso8601_now(),
        after_shebang,
    )
}

pub fn content_to_sign(body: &str, after_shebang: bool) -> &str {
    split_shebang_body(body, after_shebang).1
}

fn split_shebang_body(body: &str, after_shebang: bool) -> (Option<&str>, &str) {
    if !after_shebang || !body.starts_with("#!") {
        return (None, body);
    }
    match body.find('\n') {
        Some(idx) => {
            let shebang_end = if idx > 0 && body.as_bytes()[idx - 1] == b'\r' {
                idx - 1
            } else {
                idx
            };
            (Some(&body[..shebang_end]), &body[idx + 1..])
        }
        None => (Some(body), ""),
    }
}

fn preferred_newline(content: &str) -> &'static str {
    match content.find('\n') {
        Some(index) if index > 0 && content.as_bytes()[index - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

pub fn parse_signature_line(
    line: &str,
    prefix: &str,
    suffix: Option<&str>,
) -> Option<SignatureHeader> {
    let trimmed = line.trim();

    let after_prefix = trimmed.strip_prefix(prefix)?.trim_start();

    let payload_area = after_prefix.strip_prefix(SIGNATURE_PREFIX)?;

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
    let mut result = String::with_capacity(content.len());
    for segment in content.split_inclusive('\n') {
        let line = segment
            .strip_suffix('\n')
            .unwrap_or(segment)
            .strip_suffix('\r')
            .unwrap_or_else(|| segment.strip_suffix('\n').unwrap_or(segment));
        if !line.contains(SIGNATURE_PREFIX) {
            result.push_str(segment);
        }
    }
    result
}

/// Envelope-aware variant of [`strip_signature_lines`].
///
/// Only strips signature lines whose comment envelope matches the
/// supplied `prefix` (and `suffix`, when present). Lines containing the
/// `ryeos:signed:` marker but wrapped in a *different* envelope (e.g. a
/// `# ryeos:signed:...` comment in the body of a markdown file whose
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
    let mut result = String::with_capacity(content.len());
    for segment in content.split_inclusive('\n') {
        let line = segment
            .strip_suffix('\n')
            .unwrap_or(segment)
            .strip_suffix('\r')
            .unwrap_or_else(|| segment.strip_suffix('\n').unwrap_or(segment));
        if !is_signature_line(line, prefix, suffix) {
            result.push_str(segment);
        }
    }
    result
}

/// Remove the one signature envelope position accepted by item resolution and
/// return the exact remaining source bytes, including line terminators. A
/// signature-shaped line anywhere else is refused rather than silently
/// deleting authored body content.
pub fn strip_canonical_signature_with_envelope(
    content: &str,
    prefix: &str,
    suffix: Option<&str>,
    after_shebang: bool,
) -> anyhow::Result<(String, Option<SignatureHeader>)> {
    // Observe canonical positions without allocating a line table or a list of
    // permitted indices. Iteration preserves str::lines()'s CRLF behavior.
    let has_shebang = after_shebang
        && content.lines().next().is_some_and(|line| line.starts_with("#!"));
    let mut header = None;
    for (index, line) in content.lines().enumerate() {
        if is_signature_line(line, prefix, suffix) {
            let parsed = parse_signature_line(line, prefix, suffix)
                .ok_or_else(|| anyhow::anyhow!("item contains a malformed signature envelope"))?;
            if index != 0 && !(has_shebang && index == 1) {
                anyhow::bail!("item contains a signature envelope outside its canonical position");
            }
            if header.replace(parsed).is_some() {
                anyhow::bail!("item contains more than one signature envelope");
            }
        }
    }
    Ok((
        strip_signature_lines_with_envelope(content, prefix, suffix),
        header,
    ))
}

/// Authenticate a required leading envelope and borrow its exact body. This
/// non-shebang contract uses the same prefix/suffix parsing and signature-shaped
/// line refusals as canonical item resolution. No body/header scratch allocation
/// is needed. Caller-owned input, work and cryptography still need funding.
pub fn verified_leading_body<'a>(
    content: &'a str,
    prefix: &str,
    suffix: Option<&str>,
    verifying_key: &VerifyingKey,
    expected_fingerprint: &str,
) -> anyhow::Result<&'a str> {
    let (line, body) = content.split_once('\n').unwrap_or((content, ""));
    anyhow::ensure!(!body.lines().any(|line| is_signature_line(line, prefix, suffix)),
        "item contains a signature envelope outside its canonical position");
    let payload = line.trim().strip_prefix(prefix).map(str::trim_start)
        .and_then(|line| line.strip_prefix(SIGNATURE_PREFIX))
        .ok_or_else(|| anyhow::anyhow!("required leading signature envelope absent or malformed"))?;
    let payload = match suffix {
        Some(suffix) => payload.trim_end().strip_suffix(suffix).map(str::trim_end)
            .ok_or_else(|| anyhow::anyhow!("required signature suffix absent"))?,
        None => payload.trim_end(),
    };
    let mut fields = payload.rsplitn(4, ':');
    let fingerprint = fields.next().unwrap_or("");
    let signature = fields.next().unwrap_or("");
    let hash = fields.next().unwrap_or("");
    anyhow::ensure!(fields.next().is_some(), "malformed leading signature fields");
    anyhow::ensure!(is_valid_signature_for(hash, signature, fingerprint, body,
        verifying_key, expected_fingerprint), "invalid leading signature");
    Ok(body)
}

pub fn verify_signature(
    content_hash: &str,
    signature_b64: &str,
    verifying_key: &VerifyingKey,
) -> bool {
    // Ed25519 signatures decode to exactly 64 bytes. Base64 decode_slice may
    // conservatively require 66 slots for an 88-character padded encoding;
    // retain that fixed scratch on the stack rather than allocating a Vec.
    // Keep the accepted standard/URL-safe forms and their existing order.
    let mut bytes = [0u8; 66];
    let decoded = base64::engine::general_purpose::STANDARD
        .decode_slice(signature_b64, &mut bytes)
        .or_else(|_| {
            let stripped = signature_b64.trim_end_matches('=');
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode_slice(stripped, &mut bytes)
        })
        .or_else(|_| base64::engine::general_purpose::URL_SAFE
            .decode_slice(signature_b64, &mut bytes));
    match decoded {
        Ok(64) => match ed25519_dalek::Signature::from_slice(&bytes[..64]) {
            Ok(sig) => verifying_key.verify(content_hash.as_bytes(), &sig).is_ok(),
            Err(_) => false,
        },
        _ => false,
    }
}

/// Check whether a parsed signature's components represent a valid
/// signature for the given body content and signer.
///
/// Returns `true` only when all three conditions hold:
///   1. `content_hash` matches `content_hash(body)`,
///   2. `signer_fingerprint` matches `expected_fingerprint`, and
///   3. `signature_b64` verifies against the hash with `verifying_key`.
///
/// This accepts raw field values so it works with both
/// `lillux::signature::SignatureHeader` and
/// `ryeos_engine::contracts::SignatureHeader` (which have the same
/// fields but are distinct types).
///
/// This is the single canonical validity check used by all idempotent
/// signing paths (operator sign, bundle sign, and publish).
pub fn is_valid_signature_for(
    header_hash: &str,
    signature_b64: &str,
    signer_fingerprint: &str,
    body: &str,
    verifying_key: &VerifyingKey,
    expected_fingerprint: &str,
) -> bool {
    // SHA256's canonical lowercase representation is exactly 64 ASCII bytes;
    // retain it on the stack rather than allocating a verification-only String.
    let expected_hash = fixed_hex_digest(body.as_bytes());
    if header_hash.as_bytes() != expected_hash.as_slice() {
        return false;
    }
    if signer_fingerprint != expected_fingerprint {
        return false;
    }
    verify_signature(header_hash, signature_b64, verifying_key)
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

    let candidates: Vec<usize> =
        if after_shebang && lines.first().is_some_and(|line| line.starts_with("#!")) {
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
            for line in lines.iter().take(idx + 1) {
                offset += line.len();
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
        Some(s) => after_prefix
            .trim_end()
            .strip_suffix(s)
            .map(str::trim_end)
            .unwrap_or_else(|| after_prefix.trim_end()),
        None => after_prefix.trim_end(),
    };

    payload_area.starts_with(SIGNATURE_PREFIX)
        || payload_area == "ryeos:signed"
        || payload_area.starts_with("ryeos:signed ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_verification_borrows_exact_body_and_refuses_embedded_envelopes() {
        let key = SigningKey::from_bytes(&[42u8; 32]);
        let fingerprint = compute_fingerprint(&key.verifying_key());
        for body in ["{\"text\":\"é\"}", "first\r\nsecond\r\n"] {
            let signed = sign_content(body, &key, "#", None);
            let borrowed = verified_leading_body(&signed, "#", None,
                &key.verifying_key(), &fingerprint).unwrap();
            assert_eq!(borrowed, body);
            assert_eq!(borrowed.as_ptr(), signed[signed.find('\n').unwrap() + 1..].as_ptr());
        }
        // Even an authentically signed body cannot move another envelope into
        // body content. Preserve canonical resolution's refusal, not stripping.
        let embedded = sign_content("text\n# ryeos:signed malformed\n", &key, "#", None);
        assert!(verified_leading_body(&embedded, "#", None,
            &key.verifying_key(), &fingerprint).is_err());
        let valid = sign_content("payload", &key, "#", None);
        assert!(verified_leading_body(&valid, "#", None,
            &key.verifying_key(), &"0".repeat(64)).is_err());
    }

    #[test]
    fn bounded_signature_preserves_verified_envelopes_at_exact_limit() {
        let key = SigningKey::from_bytes(&[42u8; 32]);
        for (body, prefix, suffix) in [
            ("{\"claim\":\"é\"}", "#", None),
            ("line one\r\nline two\r\n", "<!--", Some("-->")),
        ] {
            let ordinary = sign_content(body, &key, prefix, suffix);
            let signed = sign_content_bounded(body, &key, prefix, suffix, ordinary.len()).unwrap();
            assert_eq!(signed.len(), ordinary.len());
            let (decoded, header) = strip_canonical_signature_with_envelope(
                &signed, prefix, suffix, false,
            ).unwrap();
            let header = header.unwrap();
            assert_eq!(decoded, body);
            assert!(is_valid_signature_for(&header.content_hash, &header.signature_b64,
                &header.signer_fingerprint, &decoded, &key.verifying_key(),
                &compute_fingerprint(&key.verifying_key())));
            assert!(sign_content_bounded(body, &key, prefix, suffix, ordinary.len() - 1).is_err());
        }
    }

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
        assert!(first_line.starts_with("<!-- ryeos:signed:"));
        assert!(first_line.ends_with("-->"));
        let header = parse_signature_line(first_line, "<!--", Some("-->")).unwrap();
        assert_eq!(header.content_hash, content_hash(body));
    }

    #[test]
    fn sign_with_shebang_keeps_shebang_first() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "#!/usr/bin/env python3\n# ryeos-tool:\n#   category: example\n";
        let signed =
            sign_content_at_with_options(body, &sk, "#", None, "2026-01-01T00:00:00Z", true);
        let mut lines = signed.lines();
        assert_eq!(lines.next().unwrap(), "#!/usr/bin/env python3");
        let sig_line = lines.next().unwrap();
        assert!(sig_line.starts_with("# ryeos:signed:"));
        assert_eq!(lines.next().unwrap(), "# ryeos-tool:");

        let header = parse_signature_line(sig_line, "#", None).unwrap();
        assert_eq!(
            header.content_hash,
            content_hash("# ryeos-tool:\n#   category: example\n")
        );
    }

    #[test]
    fn sign_without_after_shebang_keeps_existing_first_line_behavior() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "#!/usr/bin/env python3\nprint('hello')\n";
        let signed =
            sign_content_at_with_options(body, &sk, "#", None, "2026-01-01T00:00:00Z", false);
        let mut lines = signed.lines();
        assert!(lines.next().unwrap().starts_with("# ryeos:signed:"));
        assert_eq!(lines.next().unwrap(), "#!/usr/bin/env python3");
    }

    #[test]
    fn strip_signature_lines_removes_signed_lines() {
        let content = "# ryeos:signed:2026-04-10T00:00:00Z:abc:sig:fp\nprint('hello')\n";
        assert_eq!(strip_signature_lines(content), "print('hello')\n");
    }

    #[test]
    fn strip_signature_lines_with_envelope_removes_malformed_placeholder() {
        let content = "# ryeos:signed placeholder\nname: hello\n";

        assert_eq!(
            strip_signature_lines_with_envelope(content, "#", None),
            "name: hello\n"
        );
    }

    #[test]
    fn strip_signature_lines_with_envelope_removes_html_placeholder_without_suffix() {
        let content = "<!-- ryeos:signed placeholder\n```yaml\nname: hello\n```\nBody\n";

        assert_eq!(
            strip_signature_lines_with_envelope(content, "<!--", Some("-->")),
            "```yaml\nname: hello\n```\nBody\n"
        );
    }

    #[test]
    fn canonical_strip_preserves_the_exact_body_used_by_resolution() {
        let sk = SigningKey::from_bytes(&[7_u8; 32]);
        let crlf = "name: fixture\r\nvalue: one\r\n";
        let signed = sign_content_at(crlf, &sk, "#", None, "2026-01-01T00:00:00Z");
        let (body, header) =
            strip_canonical_signature_with_envelope(&signed, "#", None, false).unwrap();
        assert_eq!(body, crlf);
        assert_eq!(header.unwrap().content_hash, content_hash(crlf));
        assert!(signed.starts_with("# ryeos:signed:"));
        assert!(signed.contains("\r\nname: fixture\r\n"));
    }

    #[test]
    fn canonical_strip_refuses_signature_shaped_body_content() {
        let later = "name: fixture\n# ryeos:signed:2026-01-01T00:00:00Z:abc:sig:fp\n";
        assert!(strip_canonical_signature_with_envelope(later, "#", None, false).is_err());

        let malformed = "# ryeos:signed placeholder\nname: fixture\n";
        assert!(strip_canonical_signature_with_envelope(malformed, "#", None, false).is_err());
    }

    #[test]
    fn canonical_strip_accepts_only_the_post_shebang_position() {
        let sk = SigningKey::from_bytes(&[9_u8; 32]);
        let body = "#!/usr/bin/env python3\nprint('ok')\n";
        let signed =
            sign_content_at_with_options(body, &sk, "#", None, "2026-01-01T00:00:00Z", true);
        let (stripped, header) =
            strip_canonical_signature_with_envelope(&signed, "#", None, true).unwrap();
        assert_eq!(stripped, body);
        assert!(header.is_some());
    }

    #[test]
    fn parse_signature_line_stays_strict_for_malformed_placeholder() {
        assert!(parse_signature_line("# ryeos:signed placeholder", "#", None).is_none());
        assert!(
            parse_signature_line("<!-- ryeos:signed placeholder", "<!--", Some("-->")).is_none()
        );
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
            format!("# ryeos:signed:2026-04-10T00:00:00Z:{hash}:{sig_b64}:{fp}")
        };
        let content = format!("#!/usr/bin/env python3\n{sig_line}\n{body}");
        let hash = content_hash_after_signature(&content, "#", None, true).unwrap();
        assert_eq!(hash, content_hash(body));
    }

    #[test]
    fn content_hash_after_signature_rejects_line_two_without_shebang() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let body = "actual content\n";
        let sig_line = {
            let hash = content_hash(body);
            let sig: ed25519_dalek::Signature = sk.sign(hash.as_bytes());
            let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
            let fp = compute_fingerprint(&sk.verifying_key());
            format!("# ryeos:signed:2026-04-10T00:00:00Z:{hash}:{sig_b64}:{fp}")
        };
        let content = format!("not a shebang\n{sig_line}\n{body}");

        assert!(content_hash_after_signature(&content, "#", None, true).is_none());
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

    // ── verify hardening: wrong key / truncated / non-base64 signature ──

    #[test]
    fn verify_signature_rejects_wrong_key() {
        // A signature made by one key must not verify against a different
        // key's verifying half — even for the same hash.
        let signer = SigningKey::from_bytes(&[1u8; 32]);
        let other = SigningKey::from_bytes(&[2u8; 32]);
        let hash = content_hash("payload\n");
        let sig: ed25519_dalek::Signature = signer.sign(hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        assert!(!verify_signature(&hash, &sig_b64, &other.verifying_key()));
    }

    #[test]
    fn verify_signature_rejects_truncated_signature() {
        // A truncated envelope: the signature bytes are short of the
        // 64-byte Ed25519 length, so `Signature::from_slice` rejects them.
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let hash = content_hash("payload\n");
        let sig: ed25519_dalek::Signature = sk.sign(hash.as_bytes());
        let full = sig.to_bytes();
        let truncated = base64::engine::general_purpose::STANDARD.encode(&full[..63]);
        assert!(!verify_signature(&hash, &truncated, &sk.verifying_key()));
    }

    #[test]
    fn verify_signature_refuses_oversized_decoded_payloads() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let hash = content_hash("payload\n");
        // Exercise both wrong-size results within scratch and output overflow,
        // through every accepted encoding branch. No prefix may be accepted.
        for length in [65, 66, 67, 1024] {
            let payload = vec![0xa5; length];
            for engine in [base64::engine::general_purpose::STANDARD,
                base64::engine::general_purpose::URL_SAFE,
                base64::engine::general_purpose::URL_SAFE_NO_PAD] {
                assert!(!verify_signature(&hash, &engine.encode(&payload), &sk.verifying_key()));
            }
        }
    }

    #[test]
    fn verify_signature_rejects_non_base64_payload() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let hash = content_hash("payload\n");
        assert!(!verify_signature(
            &hash,
            "not*valid*base64",
            &sk.verifying_key()
        ));
    }

    #[test]
    fn verify_signature_accepts_urlsafe_nopad_encoding() {
        // The verifier falls back to URL-safe base64 with no padding, so a
        // signature encoded that way must still verify.
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let hash = content_hash("payload\n");
        let sig: ed25519_dalek::Signature = sk.sign(hash.as_bytes());
        let sig_url = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes());
        assert!(verify_signature(&hash, &sig_url, &sk.verifying_key()));
    }

    // ── is_valid_signature_for: the canonical three-part validity check ──

    fn signed_triple(body: &str, sk: &SigningKey) -> (String, String, String) {
        let hash = content_hash(body);
        let sig: ed25519_dalek::Signature = sk.sign(hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        let fp = compute_fingerprint(&sk.verifying_key());
        (hash, sig_b64, fp)
    }

    #[test]
    fn is_valid_signature_for_accepts_matching_triple() {
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let body = "authentic body\n";
        let (hash, sig_b64, fp) = signed_triple(body, &sk);
        assert!(is_valid_signature_for(
            &hash,
            &sig_b64,
            &fp,
            body,
            &sk.verifying_key(),
            &fp
        ));
    }

    #[test]
    fn is_valid_signature_for_rejects_tampered_body() {
        // Header signs the original body; verifying against a mutated body
        // fails at the content-hash comparison before any crypto check.
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let (hash, sig_b64, fp) = signed_triple("authentic body\n", &sk);
        assert!(!is_valid_signature_for(
            &hash,
            &sig_b64,
            &fp,
            "TAMPERED body\n",
            &sk.verifying_key(),
            &fp
        ));
    }

    #[test]
    fn is_valid_signature_for_rejects_wrong_fingerprint() {
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let body = "authentic body\n";
        let (hash, sig_b64, fp) = signed_triple(body, &sk);
        let bogus_fp = "0".repeat(64);
        assert!(!is_valid_signature_for(
            &hash,
            &sig_b64,
            &bogus_fp,
            body,
            &sk.verifying_key(),
            &fp
        ));
    }

    #[test]
    fn is_valid_signature_for_rejects_wrong_key() {
        // Hash and fingerprint fields are internally consistent, but the
        // verifying key belongs to a different signer, so the crypto check
        // is the sole gate that fails.
        let signer = SigningKey::from_bytes(&[3u8; 32]);
        let other = SigningKey::from_bytes(&[4u8; 32]);
        let body = "authentic body\n";
        let (hash, sig_b64, fp) = signed_triple(body, &signer);
        assert!(!is_valid_signature_for(
            &hash,
            &sig_b64,
            &fp,
            body,
            &other.verifying_key(),
            &fp
        ));
    }

    #[test]
    fn signed_document_tamper_detected_end_to_end() {
        // Parse a real signed document's header, then validate it against a
        // body whose content line was altered: detection must fire.
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        let expected_fp = compute_fingerprint(&sk.verifying_key());
        let signed = sign_content("original line\n", &sk, "#", None);
        let header = parse_signature_line(signed.lines().next().unwrap(), "#", None).unwrap();

        assert!(
            is_valid_signature_for(
                &header.content_hash,
                &header.signature_b64,
                &header.signer_fingerprint,
                "original line\n",
                &sk.verifying_key(),
                &expected_fp,
            ),
            "untampered document must validate"
        );
        assert!(
            !is_valid_signature_for(
                &header.content_hash,
                &header.signature_b64,
                &header.signer_fingerprint,
                "tampered line\n",
                &sk.verifying_key(),
                &expected_fp,
            ),
            "post-signing body edit must be rejected"
        );
    }

    #[test]
    fn parse_signature_line_rejects_truncated_field_set() {
        // A signature line carrying fewer than the four required
        // colon-separated components (timestamp:hash:sig:fingerprint) is a
        // truncated envelope and must not parse into a header.
        let truncated = "# ryeos:signed:onlytwo:fields";
        assert!(parse_signature_line(truncated, "#", None).is_none());
    }
}
