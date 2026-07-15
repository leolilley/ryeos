//! Attestation — a signed claim about a CAS object.
//!
//! Attestations are immutable CAS objects. They do not make a claim
//! trusted by themselves; they only prove that `issuer` signed a claim
//! about `subject_hash` under `policy`. Local policy decides whether a
//! verified attestation is authoritative.

use anyhow::{anyhow, Context};
use base64::Engine as _;
use lillux::crypto::Verifier;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::refs::TrustStore;
use crate::signer::Signer;

const ATTESTATION_SCHEMA: u32 = 1;
const ATTESTATION_KIND: &str = "attestation";
const ISSUER_PREFIX: &str = "fp:";

/// A signed claim about a CAS object.
///
/// The signature is computed over the canonical JSON form of this object
/// without the `signature` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    pub schema: u32,
    pub kind: String,
    pub subject_hash: String,
    pub claim: String,
    pub policy: String,
    pub issuer: String,
    pub issued_at: String,
    pub expires_at: Option<String>,
    pub evidence: Value,
    pub signature: String,
}

impl Attestation {
    /// Build an unsigned attestation. Call [`Self::sign`] before storing
    /// or verifying it.
    pub fn unsigned(
        subject_hash: String,
        claim: String,
        policy: String,
        issued_at: String,
        expires_at: Option<String>,
        evidence: Value,
    ) -> Self {
        Self {
            schema: ATTESTATION_SCHEMA,
            kind: ATTESTATION_KIND.to_string(),
            subject_hash,
            claim,
            policy,
            issuer: String::new(),
            issued_at,
            expires_at,
            evidence,
            signature: String::new(),
        }
    }

    /// Sign this attestation with the supplied signer.
    ///
    /// The issuer is always set from the signing key fingerprint as
    /// `fp:<fingerprint>` immediately before signing.
    pub fn sign(mut self, signer: &dyn Signer) -> anyhow::Result<Self> {
        self.issuer = issuer_from_fingerprint(signer.fingerprint())?;
        self.signature.clear();
        self.validate_unsigned_fields()?;

        let unsigned = self.without_signature();
        let canonical = lillux::canonical_json(&unsigned);
        let sig_bytes = signer.sign(canonical.as_bytes());
        self.signature = base64::engine::general_purpose::STANDARD.encode(sig_bytes);
        self.validate()?;
        Ok(self)
    }

    /// Validate this attestation's structure, including the presence and
    /// parseability of the signature.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_unsigned_fields()?;
        if self.signature.is_empty() {
            anyhow::bail!("signature must not be empty");
        }
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.signature)
            .context("failed to decode signature")?;
        lillux::crypto::Signature::from_slice(&sig_bytes)
            .map_err(|e| anyhow!("failed to parse signature: {e}"))?;
        Ok(())
    }

    /// Verify this attestation against a specific verifying key.
    ///
    /// Verification checks both the Ed25519 signature and the issuer/key
    /// binding: the verifying key fingerprint must equal the raw
    /// fingerprint inside `issuer`.
    pub fn verify_with_key(&self, key: &lillux::crypto::VerifyingKey) -> anyhow::Result<()> {
        self.validate()?;

        let expected = self.issuer_fingerprint()?;
        let actual = lillux::crypto::fingerprint(key);
        if actual != expected {
            anyhow::bail!(
                "issuer fingerprint mismatch: attestation issuer is {}, verifying key is {}",
                expected,
                actual
            );
        }

        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.signature)
            .context("failed to decode signature")?;
        let signature = lillux::crypto::Signature::from_slice(&sig_bytes)
            .map_err(|e| anyhow!("failed to parse signature: {e}"))?;
        let unsigned = self.without_signature();
        let canonical = lillux::canonical_json(&unsigned);
        key.verify(canonical.as_bytes(), &signature)
            .map_err(|e| anyhow!("signature verification failed: {e}"))?;
        Ok(())
    }

    /// Verify using a trust store keyed by raw fingerprint.
    pub fn verify_with_trust_store(&self, trust: &TrustStore) -> anyhow::Result<()> {
        let raw = self.issuer_fingerprint()?;
        let key = trust
            .get(raw)
            .ok_or_else(|| anyhow!("issuer {} not in trust store", self.issuer))?;
        self.verify_with_key(key)
    }

    /// Return true if this attestation is expired at `now_iso8601`.
    pub fn is_expired_at(&self, now_iso8601: &str) -> anyhow::Result<bool> {
        let Some(expires_at) = self.expires_at.as_deref() else {
            return Ok(false);
        };
        let now = parse_utc_rfc3339_seconds(now_iso8601)?;
        let expires = parse_utc_rfc3339_seconds(expires_at)?;
        Ok(now >= expires)
    }

    /// Convert to JSON value for CAS storage.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Parse and validate an attestation from a JSON value.
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let attestation: Self =
            serde_json::from_value(value.clone()).context("failed to deserialize attestation")?;
        attestation.validate()?;
        Ok(attestation)
    }

    /// Return the raw fingerprint from the `fp:<fingerprint>` issuer.
    pub fn issuer_fingerprint(&self) -> anyhow::Result<&str> {
        self.issuer
            .strip_prefix(ISSUER_PREFIX)
            .filter(|fp| is_valid_fingerprint(fp))
            .ok_or_else(|| anyhow!("issuer must be fp:<64-hex-fingerprint>"))
    }

    fn validate_unsigned_fields(&self) -> anyhow::Result<()> {
        if self.schema != ATTESTATION_SCHEMA {
            anyhow::bail!(
                "invalid schema: expected {}, got {}",
                ATTESTATION_SCHEMA,
                self.schema
            );
        }
        if self.kind != ATTESTATION_KIND {
            anyhow::bail!(
                "invalid kind: expected {}, got {}",
                ATTESTATION_KIND,
                self.kind
            );
        }
        crate::objects::thread_snapshot::validate_canonical_hash(
            "subject_hash",
            &self.subject_hash,
        )?;
        if self.claim.is_empty() {
            anyhow::bail!("claim must not be empty");
        }
        if self.policy.is_empty() {
            anyhow::bail!("policy must not be empty");
        }
        self.issuer_fingerprint()?;
        let issued_at = parse_utc_rfc3339_seconds(&self.issued_at)?;
        if let Some(expires_at) = self.expires_at.as_deref() {
            let expires_at = parse_utc_rfc3339_seconds(expires_at)?;
            if expires_at <= issued_at {
                anyhow::bail!("expires_at must be after issued_at");
            }
        }
        if !self.evidence.is_object() {
            anyhow::bail!("evidence must be a JSON object");
        }
        Ok(())
    }

    fn without_signature(&self) -> Value {
        json!({
            "schema": self.schema,
            "kind": self.kind,
            "subject_hash": self.subject_hash,
            "claim": self.claim,
            "policy": self.policy,
            "issuer": self.issuer,
            "issued_at": self.issued_at,
            "expires_at": self.expires_at,
            "evidence": self.evidence,
        })
    }
}

fn issuer_from_fingerprint(fingerprint: &str) -> anyhow::Result<String> {
    if !is_valid_fingerprint(fingerprint) {
        anyhow::bail!("invalid signer fingerprint: {fingerprint}");
    }
    Ok(format!("{ISSUER_PREFIX}{fingerprint}"))
}

fn is_valid_fingerprint(fingerprint: &str) -> bool {
    fingerprint.len() == 64
        && fingerprint
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn parse_utc_rfc3339_seconds(input: &str) -> anyhow::Result<i64> {
    let bytes = input.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        anyhow::bail!("timestamp must be RFC3339 UTC in YYYY-MM-DDTHH:MM:SSZ form");
    }
    let year = parse_fixed_i32(input, 0, 4)?;
    let month = parse_fixed_u32(input, 5, 7)?;
    let day = parse_fixed_u32(input, 8, 10)?;
    let hour = parse_fixed_u32(input, 11, 13)?;
    let minute = parse_fixed_u32(input, 14, 16)?;
    let second = parse_fixed_u32(input, 17, 19)?;

    if !(1..=12).contains(&month) {
        anyhow::bail!("timestamp month out of range");
    }
    let max_day = days_in_month(year, month);
    if day == 0 || day > max_day {
        anyhow::bail!("timestamp day out of range");
    }
    if hour > 23 || minute > 59 || second > 59 {
        anyhow::bail!("timestamp time out of range");
    }

    let days = days_from_civil(year, month, day);
    Ok(days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64)
}

fn parse_fixed_i32(input: &str, start: usize, end: usize) -> anyhow::Result<i32> {
    input[start..end].parse::<i32>().with_context(|| {
        format!(
            "invalid timestamp number '{}': expected digits",
            &input[start..end]
        )
    })
}

fn parse_fixed_u32(input: &str, start: usize, end: usize) -> anyhow::Result<u32> {
    input[start..end].parse::<u32>().with_context(|| {
        format!(
            "invalid timestamp number '{}': expected digits",
            &input[start..end]
        )
    })
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;

    fn subject_hash() -> String {
        "ab".repeat(32)
    }

    fn evidence() -> Value {
        json!({ "checks": ["hash_valid"], "refs": [] })
    }

    fn unsigned() -> Attestation {
        Attestation::unsigned(
            subject_hash(),
            "accepted".to_string(),
            "ryeos.node.admission.v1".to_string(),
            "2026-05-29T12:00:00Z".to_string(),
            Some("2026-05-30T12:00:00Z".to_string()),
            evidence(),
        )
    }

    fn alternate_signer() -> TestSignerAlt {
        TestSignerAlt::new([7u8; 32])
    }

    struct TestSignerAlt {
        signing_key: lillux::crypto::SigningKey,
        fingerprint: String,
    }

    impl TestSignerAlt {
        fn new(seed: [u8; 32]) -> Self {
            let signing_key = lillux::crypto::SigningKey::from_bytes(&seed);
            let fingerprint = lillux::crypto::fingerprint(&signing_key.verifying_key());
            Self {
                signing_key,
                fingerprint,
            }
        }

        fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
            self.signing_key.verifying_key()
        }
    }

    impl Signer for TestSignerAlt {
        fn sign(&self, data: &[u8]) -> Vec<u8> {
            use lillux::crypto::Signer as Ed25519Signer;
            self.signing_key.sign(data).to_bytes().to_vec()
        }

        fn fingerprint(&self) -> &str {
            &self.fingerprint
        }

        fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
            self.signing_key.verifying_key()
        }
    }

    #[test]
    fn roundtrip() {
        let signer = TestSigner::new();
        let signed = unsigned().sign(&signer).unwrap();
        let value = signed.to_value();
        let restored = Attestation::from_value(&value).unwrap();
        assert_eq!(restored, signed);
    }

    #[test]
    fn signed_attestation_verifies_with_issuer_key() {
        let signer = TestSigner::new();
        let signed = unsigned().sign(&signer).unwrap();
        signed.verify_with_key(&signer.verifying_key()).unwrap();
    }

    #[test]
    fn stable_hash_for_same_canonical_content() {
        let signer = TestSigner::new();
        let signed1 = unsigned().sign(&signer).unwrap();
        let signed2 = unsigned().sign(&signer).unwrap();
        assert_eq!(signed1.to_value(), signed2.to_value());
        let hash1 = lillux::sha256_hex(lillux::canonical_json(&signed1.to_value()).as_bytes());
        let hash2 = lillux::sha256_hex(lillux::canonical_json(&signed2.to_value()).as_bytes());
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn tampering_with_subject_fails_verification() {
        let signer = TestSigner::new();
        let mut signed = unsigned().sign(&signer).unwrap();
        signed.subject_hash = "cd".repeat(32);
        assert!(signed.verify_with_key(&signer.verifying_key()).is_err());
    }

    #[test]
    fn tampering_with_claim_fails_verification() {
        let signer = TestSigner::new();
        let mut signed = unsigned().sign(&signer).unwrap();
        signed.claim = "rejected".to_string();
        assert!(signed.verify_with_key(&signer.verifying_key()).is_err());
    }

    #[test]
    fn verifying_with_wrong_key_fails() {
        let signer = TestSigner::new();
        let other = alternate_signer();
        let signed = unsigned().sign(&signer).unwrap();
        assert!(signed.verify_with_key(&other.verifying_key()).is_err());
    }

    #[test]
    fn missing_or_invalid_subject_hash_rejects() {
        let signer = TestSigner::new();
        let mut attestation = unsigned();
        attestation.subject_hash = "not-a-hash".to_string();
        assert!(attestation.sign(&signer).is_err());
    }

    #[test]
    fn unknown_claim_roundtrips_and_verifies() {
        let signer = TestSigner::new();
        let mut attestation = unsigned();
        attestation.claim = "custom.future.claim".to_string();
        let signed = attestation.sign(&signer).unwrap();
        let restored = Attestation::from_value(&signed.to_value()).unwrap();
        restored.verify_with_key(&signer.verifying_key()).unwrap();
        assert_eq!(restored.claim, "custom.future.claim");
    }

    #[test]
    fn expiry_helper_returns_expected_result() {
        let signed = unsigned().sign(&TestSigner::new()).unwrap();
        assert!(!signed.is_expired_at("2026-05-30T11:59:59Z").unwrap());
        assert!(signed.is_expired_at("2026-05-30T12:00:00Z").unwrap());
    }

    #[test]
    fn changing_issuer_after_signing_fails_verification() {
        let signer = TestSigner::new();
        let other = alternate_signer();
        let mut signed = unsigned().sign(&signer).unwrap();
        signed.issuer = format!("fp:{}", other.fingerprint());
        assert!(signed.verify_with_key(&other.verifying_key()).is_err());
    }

    #[test]
    fn valid_signature_claiming_wrong_issuer_fails_verification() {
        let signer = TestSigner::with_fingerprint(alternate_signer().fingerprint().to_string());
        let signed = unsigned().sign(&signer).unwrap();
        assert!(signed.verify_with_key(&signer.verifying_key()).is_err());
    }

    #[test]
    fn malformed_base64_signature_fails() {
        let signer = TestSigner::new();
        let mut signed = unsigned().sign(&signer).unwrap();
        signed.signature = "not base64".to_string();
        assert!(signed.validate().is_err());
    }

    #[test]
    fn invalid_timestamps_reject() {
        let signer = TestSigner::new();
        let mut invalid_issued = unsigned();
        invalid_issued.issued_at = "2026-99-29T12:00:00Z".to_string();
        assert!(invalid_issued.sign(&signer).is_err());

        let mut invalid_expiry = unsigned();
        invalid_expiry.expires_at = Some("2026-05-29T12:00:00Z".to_string());
        assert!(invalid_expiry.sign(&signer).is_err());
    }

    #[test]
    fn uppercase_fingerprint_rejects_before_signing() {
        let signer = TestSigner::with_fingerprint(TestSigner::new().fingerprint().to_uppercase());
        assert!(unsigned().sign(&signer).is_err());
    }

    #[test]
    fn tampering_with_policy_timestamp_or_evidence_fails_verification() {
        let signer = TestSigner::new();

        let mut policy_tampered = unsigned().sign(&signer).unwrap();
        policy_tampered.policy = "other.policy".to_string();
        assert!(policy_tampered
            .verify_with_key(&signer.verifying_key())
            .is_err());

        let mut issued_tampered = unsigned().sign(&signer).unwrap();
        issued_tampered.issued_at = "2026-05-29T12:00:01Z".to_string();
        assert!(issued_tampered
            .verify_with_key(&signer.verifying_key())
            .is_err());

        let mut expiry_tampered = unsigned().sign(&signer).unwrap();
        expiry_tampered.expires_at = Some("2026-05-31T12:00:00Z".to_string());
        assert!(expiry_tampered
            .verify_with_key(&signer.verifying_key())
            .is_err());

        let mut evidence_tampered = unsigned().sign(&signer).unwrap();
        evidence_tampered.evidence = json!({ "checks": ["different"], "refs": [] });
        assert!(evidence_tampered
            .verify_with_key(&signer.verifying_key())
            .is_err());
    }

    #[test]
    fn trust_store_verification_accepts_raw_fingerprint_key() {
        let signer = TestSigner::new();
        let signed = unsigned().sign(&signer).unwrap();
        let mut trust = TrustStore::new();
        trust.insert(signer.fingerprint().to_string(), signer.verifying_key());
        signed.verify_with_trust_store(&trust).unwrap();
    }

    #[test]
    fn unknown_top_level_field_rejects() {
        let signer = TestSigner::new();
        let mut value = unsigned().sign(&signer).unwrap().to_value();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), json!(true));
        assert!(Attestation::from_value(&value).is_err());
    }

    #[test]
    fn evidence_must_be_object() {
        let signer = TestSigner::new();
        let mut attestation = unsigned();
        attestation.evidence = json!(["not", "object"]);
        assert!(attestation.sign(&signer).is_err());
    }
}
