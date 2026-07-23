//! Shared trusted verifier for the prepared-request spend bound (§9.2).
//!
//! Before reservation, the exact [`PreparedProviderRequest`] is evaluated
//! against the sealed certificate and reduced to a bounded
//! [`VerifiedPreparedSpendBound`]. The reserve RPC carries this proof — never
//! the prompt or body bytes — and the daemon requires its maximum to equal
//! the sealed authority maximum exactly. The verifier never recomputes a
//! LOWER maximum: a runtime cannot discount its own reservation.

use anyhow::{anyhow, bail, Result};

use crate::directive::ProviderConfig;
use crate::provider_adapter::PreparedProviderRequest;
use ryeos_accounting::{
    BillableDimension, HexDigest, ProviderAccountingAuthority, SpendBoundAuthority,
    SpendBoundCertificate, SpendBoundCommitments, UnitCount, UsdNanos, VerifiedPreparedSpendBound,
};

/// Version string committed into every proof so a daemon can reject proofs
/// produced under a different verifier contract.
pub const VERIFIER_CONTRACT_VERSION: &str = "spend-verifier/v1";

fn verifier_contract_digest() -> Result<HexDigest> {
    HexDigest::new(lillux::cas::sha256_hex(VERIFIER_CONTRACT_VERSION.as_bytes()))
        .map_err(|error| anyhow!("verifier contract digest: {error}"))
}

/// Mirror of launch preparation's per-dimension unit ceiling: prompt-side
/// dimensions are bounded by the declared context window, generation-side
/// dimensions by the effective provider-native output ceiling, per-request
/// by exactly one. Must stay in lock-step with
/// `ryeos_directive_core::resolve_accounting_authority`.
fn dimension_bound(
    dimension: BillableDimension,
    context_window: u64,
    max_provider_output_tokens_per_turn: u64,
) -> Result<UnitCount> {
    let units = match dimension {
        BillableDimension::InputTokens
        | BillableDimension::CacheReadTokens
        | BillableDimension::CacheWriteTokens => context_window,
        BillableDimension::OutputTokens | BillableDimension::ReasoningTokens => {
            if max_provider_output_tokens_per_turn == 0 {
                bail!(
                    "derived worst-case certificate covers {dimension:?} but the provider \
                     output ceiling is disabled; no bounded output exists for this attempt"
                );
            }
            max_provider_output_tokens_per_turn
        }
        BillableDimension::PerRequest => 1,
    };
    Ok(UnitCount { dimension, units })
}

/// Evaluate the exact prepared request against the sealed certificate and
/// produce the bounded proof carried on the reserve RPC.
///
/// `AdvisoryOnly` routes error: the caller must not reserve for them.
pub fn verify_prepared_spend_bound(
    prepared: &PreparedProviderRequest,
    authority: &ProviderAccountingAuthority,
    provider: &ProviderConfig,
    context_window: u64,
    max_provider_output_tokens_per_turn: u64,
) -> Result<VerifiedPreparedSpendBound> {
    authority
        .validate()
        .map_err(|error| anyhow!("sealed accounting authority failed validation: {error}"))?;

    let (maximum, commitments) = match &authority.spend_bound {
        SpendBoundAuthority::AdvisoryOnly => bail!(
            "route accounting authority is advisory-only; it is ineligible for reservation \
             and the caller must not reserve for it"
        ),
        SpendBoundAuthority::ExplicitlyFree { contract_digest } => (
            UsdNanos::ZERO,
            SpendBoundCommitments::ExplicitlyFree {
                contract_digest: contract_digest.clone(),
            },
        ),
        SpendBoundAuthority::Paid {
            maximum,
            certificate,
        } => match certificate {
            SpendBoundCertificate::DerivedWorstCaseCharge {
                tariff_contract_digest,
                request_limit_digest,
                covered_dimensions,
                currency: _,
                pricing_generation,
                expires_at_ms: _,
            } => {
                if max_provider_output_tokens_per_turn == 0 {
                    bail!(
                        "derived worst-case certificate requires a bounded provider output \
                         ceiling, but max_provider_output_tokens_per_turn is 0"
                    );
                }
                let requested = prepared.requested_output_tokens.ok_or_else(|| {
                    anyhow!(
                        "derived worst-case certificate requires the prepared request to \
                         carry an effective provider-native output limit, but none was read \
                         back from the rendered body"
                    )
                })?;
                if requested > max_provider_output_tokens_per_turn {
                    bail!(
                        "prepared request output limit {requested} exceeds the certified \
                         ceiling {max_provider_output_tokens_per_turn}"
                    );
                }
                // Recompute the request-limit digest exactly as launch
                // preparation sealed it; a drifted ceiling, context window, or
                // output-limit path invalidates the certificate for this
                // prepared request.
                let recomputed = HexDigest::of_canonical_json(&serde_json::json!({
                    "context_window": context_window,
                    "max_provider_output_tokens_per_turn": max_provider_output_tokens_per_turn,
                    "output_limit_path": provider
                        .schemas
                        .as_ref()
                        .and_then(|schemas| schemas.output_limit.as_ref())
                        .map(|limit| limit.path.clone()),
                }))
                .map_err(|error| anyhow!("recompute request-limit digest: {error}"))?;
                if &recomputed != request_limit_digest {
                    bail!(
                        "request-limit digest mismatch: sealed {} recomputed {}",
                        request_limit_digest.as_str(),
                        recomputed.as_str()
                    );
                }
                let tariff = provider
                    .spend_authority
                    .as_ref()
                    .and_then(|sa| sa.tariff.as_ref())
                    .ok_or_else(|| {
                        anyhow!(
                            "derived worst-case certificate references a tariff, but the \
                             resolved provider carries no spend_authority tariff document"
                        )
                    })?;
                let tariff_digest = tariff
                    .digest()
                    .map_err(|error| anyhow!("tariff digest: {error}"))?;
                if &tariff_digest != tariff_contract_digest {
                    bail!(
                        "tariff contract digest mismatch: sealed {} resolved {}",
                        tariff_contract_digest.as_str(),
                        tariff_digest.as_str()
                    );
                }
                if &tariff.pricing_generation != pricing_generation {
                    bail!(
                        "tariff pricing generation mismatch: sealed {pricing_generation:?} \
                         resolved {:?}",
                        tariff.pricing_generation
                    );
                }
                let unit_bounds = covered_dimensions
                    .as_slice()
                    .iter()
                    .map(|dimension| {
                        dimension_bound(
                            *dimension,
                            context_window,
                            max_provider_output_tokens_per_turn,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                (
                    *maximum,
                    SpendBoundCommitments::DerivedUnits {
                        unit_bounds,
                        pricing_generation: pricing_generation.clone(),
                    },
                )
            }
            SpendBoundCertificate::ProviderEnforcedChargeCap {
                request_cap_contract_digest,
                currency: _,
            } => {
                let cap = provider
                    .spend_authority
                    .as_ref()
                    .and_then(|sa| sa.request_charge_cap.as_ref())
                    .ok_or_else(|| {
                        anyhow!(
                            "provider-enforced cap certificate references a cap contract, \
                             but the resolved provider carries no request_charge_cap"
                        )
                    })?;
                let cap_digest = cap
                    .digest()
                    .map_err(|error| anyhow!("charge-cap contract digest: {error}"))?;
                if &cap_digest != request_cap_contract_digest {
                    bail!(
                        "charge-cap contract digest mismatch: sealed {} resolved {}",
                        request_cap_contract_digest.as_str(),
                        cap_digest.as_str()
                    );
                }
                // The cap is server-enforced only if the exact prepared BODY
                // carries the contract maximum at the contract's pointer.
                let body: serde_json::Value = serde_json::from_slice(&prepared.body_bytes)
                    .map_err(|error| {
                        anyhow!("prepared body bytes are not valid JSON: {error}")
                    })?;
                let field = body.pointer(&cap.cap_field_pointer).ok_or_else(|| {
                    anyhow!(
                        "prepared body has no value at cap pointer {}",
                        cap.cap_field_pointer
                    )
                })?;
                let field_value = match field {
                    serde_json::Value::String(text) => UsdNanos::parse_canonical(text)
                        .map_err(|error| {
                            anyhow!("cap field is not a canonical USD decimal: {error:?}")
                        })?,
                    // A JSON number is accepted only when its exact source
                    // text is itself a canonical decimal (no sign/exponent);
                    // anything else fails closed rather than round-tripping
                    // through f64.
                    serde_json::Value::Number(number) => {
                        UsdNanos::parse_canonical(&number.to_string()).map_err(|error| {
                            anyhow!(
                                "cap field number {number} is not a canonical USD decimal: \
                                 {error:?}"
                            )
                        })?
                    }
                    other => bail!("cap field has non-scalar JSON type: {other}"),
                };
                if field_value != cap.maximum {
                    bail!(
                        "prepared body cap value {} does not equal the contract maximum {}",
                        field_value.to_canonical_string(),
                        cap.maximum.to_canonical_string()
                    );
                }
                (
                    *maximum,
                    SpendBoundCommitments::ProviderCapField {
                        cap_field_pointer: cap.cap_field_pointer.clone(),
                        cap_value: cap.maximum,
                    },
                )
            }
        },
    };

    Ok(VerifiedPreparedSpendBound {
        prepared_request_digest: HexDigest::new(prepared.request_digest.clone())
            .map_err(|error| anyhow!("prepared request digest: {error}"))?,
        authority_digest: authority.authority_digest.clone(),
        // The sealed authority maximum, never recomputed lower.
        maximum,
        commitments,
        verifier_contract_digest: verifier_contract_digest()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_accounting::{
        ChargeReconciliationAuthority, ClosedBillableDimensionSet, Currency, FinalityContract,
        ProviderChargeCapContract, SpendTariffDocument, PROVIDER_CHARGE_CAP_SCHEMA_VERSION,
        SPEND_TARIFF_SCHEMA_VERSION,
    };
    use ryeos_directive_core::SpendAuthorityConfig;

    const CONTEXT_WINDOW: u64 = 200_000;
    const OUTPUT_CEILING: u64 = 8_192;

    fn usd(canonical: &str) -> UsdNanos {
        UsdNanos::parse_canonical(canonical).unwrap()
    }

    fn tariff() -> SpendTariffDocument {
        SpendTariffDocument {
            schema_version: SPEND_TARIFF_SCHEMA_VERSION,
            currency: Currency::Usd,
            pricing_generation: "gen-1".to_string(),
            input_per_million: Some(usd("3")),
            output_per_million: Some(usd("15")),
            reasoning_per_million: None,
            cache_read_per_million: None,
            cache_write_per_million: None,
            per_request: None,
            covered_dimensions: ClosedBillableDimensionSet::new(vec![
                BillableDimension::InputTokens,
                BillableDimension::OutputTokens,
            ])
            .unwrap(),
            expires_at_ms: None,
        }
    }

    fn cap_contract() -> ProviderChargeCapContract {
        ProviderChargeCapContract {
            schema_version: PROVIDER_CHARGE_CAP_SCHEMA_VERSION,
            currency: Currency::Usd,
            cap_field_pointer: "/max_cost_usd".to_string(),
            maximum: usd("0.25"),
            finality: FinalityContract {
                final_on_response: true,
                max_reported_fraction_digits: 9,
                byok_zero_is_final: false,
            },
        }
    }

    fn provider_with(spend_authority: Option<SpendAuthorityConfig>) -> ProviderConfig {
        ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            spend_authority,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        }
    }

    fn spend_authority(
        tariff: Option<SpendTariffDocument>,
        cap: Option<ProviderChargeCapContract>,
    ) -> SpendAuthorityConfig {
        SpendAuthorityConfig {
            billing_principal: "acct-1".to_string(),
            credential_authority_generation: "cred-gen-1".to_string(),
            pricing_contract_subject: "subject-1".to_string(),
            tariff,
            request_charge_cap: cap,
            reported_final_charge: None,
        }
    }

    fn request_limit_digest() -> HexDigest {
        // Same shape launch preparation seals (output_limit_path None: the
        // fixture provider declares no output-limit schema).
        HexDigest::of_canonical_json(&serde_json::json!({
            "context_window": CONTEXT_WINDOW,
            "max_provider_output_tokens_per_turn": OUTPUT_CEILING,
            "output_limit_path": serde_json::Value::Null,
        }))
        .unwrap()
    }

    fn prepared(body: serde_json::Value, requested_output_tokens: Option<u64>) -> PreparedProviderRequest {
        let body_bytes = serde_json::to_vec(&body).unwrap();
        let body_sha256 = lillux::cas::sha256_hex(&body_bytes);
        PreparedProviderRequest {
            method: reqwest::Method::POST,
            url: "http://localhost/chat/completions".to_string(),
            header_names: vec!["Accept".to_string(), "Content-Type".to_string()],
            body_bytes,
            body_sha256: body_sha256.clone(),
            requested_output_tokens,
            credential: None,
            headers: vec![],
            request_digest: lillux::cas::sha256_hex(body_sha256.as_bytes()),
        }
    }

    fn derived_authority() -> ProviderAccountingAuthority {
        let t = tariff();
        let maximum = t
            .worst_case_charge(&[
                (BillableDimension::InputTokens, CONTEXT_WINDOW),
                (BillableDimension::OutputTokens, OUTPUT_CEILING),
            ])
            .unwrap();
        ProviderAccountingAuthority {
            authority_digest: HexDigest::new(lillux::cas::sha256_hex(b"seed")).unwrap(),
            config_hash: "cfg".to_string(),
            config_value_digest: HexDigest::new(lillux::cas::sha256_hex(b"cfg-value")).unwrap(),
            billing_principal_digest: HexDigest::new(lillux::cas::sha256_hex(b"principal"))
                .unwrap(),
            credential_authority_generation: "cred-gen-1".to_string(),
            pricing_contract_subject_digest: HexDigest::new(lillux::cas::sha256_hex(b"subject"))
                .unwrap(),
            provider_id: "route".to_string(),
            model_name: "model".to_string(),
            matched_profile: None,
            spend_bound: SpendBoundAuthority::Paid {
                maximum,
                certificate: SpendBoundCertificate::DerivedWorstCaseCharge {
                    tariff_contract_digest: t.digest().unwrap(),
                    request_limit_digest: request_limit_digest(),
                    covered_dimensions: t.covered_dimensions.clone(),
                    currency: Currency::Usd,
                    pricing_generation: "gen-1".to_string(),
                    expires_at_ms: None,
                },
            },
            reconciliation: ChargeReconciliationAuthority::DeterministicTariff {
                tariff_digest: t.digest().unwrap(),
                covered_dimensions: t.covered_dimensions,
            },
        }
        .sealed()
        .unwrap()
    }

    fn cap_authority() -> ProviderAccountingAuthority {
        let cap = cap_contract();
        let mut authority = derived_authority();
        authority.spend_bound = SpendBoundAuthority::Paid {
            maximum: cap.maximum,
            certificate: SpendBoundCertificate::ProviderEnforcedChargeCap {
                request_cap_contract_digest: cap.digest().unwrap(),
                currency: Currency::Usd,
            },
        };
        authority.sealed().unwrap()
    }

    #[test]
    fn derived_route_passes_and_commits_unit_bounds() {
        let authority = derived_authority();
        let provider = provider_with(Some(spend_authority(Some(tariff()), None)));
        let prepared = prepared(serde_json::json!({"messages": []}), Some(OUTPUT_CEILING));

        let verified = verify_prepared_spend_bound(
            &prepared,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap();

        assert_eq!(verified.authority_digest, authority.authority_digest);
        assert_eq!(
            verified.prepared_request_digest.as_str(),
            prepared.request_digest
        );
        let SpendBoundAuthority::Paid { maximum, .. } = &authority.spend_bound else {
            unreachable!()
        };
        assert_eq!(&verified.maximum, maximum, "maximum is the sealed value");
        match &verified.commitments {
            SpendBoundCommitments::DerivedUnits {
                unit_bounds,
                pricing_generation,
            } => {
                assert_eq!(pricing_generation, "gen-1");
                assert_eq!(
                    unit_bounds,
                    &vec![
                        UnitCount {
                            dimension: BillableDimension::InputTokens,
                            units: CONTEXT_WINDOW
                        },
                        UnitCount {
                            dimension: BillableDimension::OutputTokens,
                            units: OUTPUT_CEILING
                        },
                    ]
                );
            }
            other => panic!("expected DerivedUnits, got {other:?}"),
        }
    }

    #[test]
    fn derived_route_rejects_output_limit_above_ceiling() {
        let authority = derived_authority();
        let provider = provider_with(Some(spend_authority(Some(tariff()), None)));
        // Tampered prepared request asks for more output than certified.
        let prepared = prepared(serde_json::json!({"messages": []}), Some(OUTPUT_CEILING + 1));
        let error = verify_prepared_spend_bound(
            &prepared,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap_err();
        assert!(error.to_string().contains("exceeds the certified ceiling"));

        // A prepared request with NO effective output limit also fails.
        let prepared = prepared_missing_limit();
        let error = verify_prepared_spend_bound(
            &prepared,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap_err();
        assert!(error.to_string().contains("output limit"));
    }

    fn prepared_missing_limit() -> PreparedProviderRequest {
        prepared(serde_json::json!({"messages": []}), None)
    }

    #[test]
    fn derived_route_rejects_drifted_request_limit_or_tariff() {
        let authority = derived_authority();
        let provider = provider_with(Some(spend_authority(Some(tariff()), None)));
        let prepared = prepared(serde_json::json!({"messages": []}), Some(OUTPUT_CEILING));

        // A different runtime ceiling breaks the sealed request-limit digest.
        let error = verify_prepared_spend_bound(
            &prepared,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING - 1,
        )
        .unwrap_err();
        assert!(error.to_string().contains("request-limit digest mismatch"));

        // A different resolved tariff (changed rate) breaks the contract digest.
        let mut changed = tariff();
        changed.output_per_million = Some(usd("16"));
        let provider = provider_with(Some(spend_authority(Some(changed), None)));
        let error = verify_prepared_spend_bound(
            &prepared,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap_err();
        assert!(error.to_string().contains("tariff contract digest mismatch"));
    }

    #[test]
    fn cap_route_requires_exact_cap_field_in_prepared_body() {
        let authority = cap_authority();
        let provider = provider_with(Some(spend_authority(None, Some(cap_contract()))));

        // Exact cap value in the body (canonical string form) passes.
        let ok = prepared(
            serde_json::json!({"messages": [], "max_cost_usd": "0.25"}),
            Some(OUTPUT_CEILING),
        );
        let verified = verify_prepared_spend_bound(
            &ok,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap();
        match &verified.commitments {
            SpendBoundCommitments::ProviderCapField {
                cap_field_pointer,
                cap_value,
            } => {
                assert_eq!(cap_field_pointer, "/max_cost_usd");
                assert_eq!(*cap_value, usd("0.25"));
            }
            other => panic!("expected ProviderCapField, got {other:?}"),
        }

        // Tampered body cap value fails.
        let tampered = prepared(
            serde_json::json!({"messages": [], "max_cost_usd": "0.50"}),
            Some(OUTPUT_CEILING),
        );
        let error = verify_prepared_spend_bound(
            &tampered,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not equal the contract maximum"));

        // Missing cap field fails.
        let missing = prepared(serde_json::json!({"messages": []}), Some(OUTPUT_CEILING));
        let error = verify_prepared_spend_bound(
            &missing,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap_err();
        assert!(error.to_string().contains("no value at cap pointer"));
    }

    #[test]
    fn advisory_route_refuses_verification() {
        let mut authority = derived_authority();
        authority.spend_bound = SpendBoundAuthority::AdvisoryOnly;
        let authority = authority.sealed().unwrap();
        let provider = provider_with(None);
        let prepared = prepared(serde_json::json!({"messages": []}), Some(OUTPUT_CEILING));
        let error = verify_prepared_spend_bound(
            &prepared,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap_err();
        assert!(error.to_string().contains("advisory-only"));
    }

    #[test]
    fn explicitly_free_commits_contract_digest_and_zero_maximum() {
        let mut authority = derived_authority();
        let contract_digest = HexDigest::new(lillux::cas::sha256_hex(b"free-contract")).unwrap();
        authority.spend_bound = SpendBoundAuthority::ExplicitlyFree {
            contract_digest: contract_digest.clone(),
        };
        let authority = authority.sealed().unwrap();
        let provider = provider_with(None);
        let prepared = prepared(serde_json::json!({"messages": []}), Some(OUTPUT_CEILING));
        let verified = verify_prepared_spend_bound(
            &prepared,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap();
        assert!(verified.maximum.is_zero());
        assert_eq!(
            verified.commitments,
            SpendBoundCommitments::ExplicitlyFree { contract_digest }
        );
    }

    #[test]
    fn tampered_sealed_authority_fails_validation_first() {
        let mut authority = derived_authority();
        authority.model_name = "other-model".to_string();
        // NOT resealed — the digest no longer matches.
        let provider = provider_with(Some(spend_authority(Some(tariff()), None)));
        let prepared = prepared(serde_json::json!({"messages": []}), Some(OUTPUT_CEILING));
        let error = verify_prepared_spend_bound(
            &prepared,
            &authority,
            &provider,
            CONTEXT_WINDOW,
            OUTPUT_CEILING,
        )
        .unwrap_err();
        assert!(error.to_string().contains("failed validation"));
    }
}
