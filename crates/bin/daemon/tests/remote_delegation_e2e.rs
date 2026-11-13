//! Two-daemon end-to-end proof of principal-signed remote delegation.
//!
//! ORIGIN (node A) holds two local principals, U1 (its operator) and U2. HOSTED
//! (node B) admits ORIGIN's node key as `remote_node` for ORIGIN's site, and
//! admits U1 and U2 as `remote_principal` grants bound to that site with
//! different scopes. ORIGIN holds a remote named `hosted`.
//!
//! Scenarios through ORIGIN's services prove the delegated path: a call runs on
//! HOSTED as the delegating principal, its scopes are the grant intersected
//! with the delegation, a principal with no delegation is refused on ORIGIN,
//! and revoking a delegation records a tombstone on HOSTED that refuses it.
//! Scenarios that drive HOSTED directly forge requests the way a compromised or
//! mistaken origin could, signing with ORIGIN's node key, and prove HOSTED
//! refuses wrong-audience, expired, tampered and transitive delegations.

use ryeosd::test_support as common;

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_node_runtime::remote_delegation::{DelegationBody, DelegationParties, SignedDelegation};
use serde_json::{Value, json};

use common::DaemonHarness;
use common::fast_fixture::FastFixture;

const HOSTED_REMOTE: &str = "hosted";

const THREADS_CAP: &str = "ryeos.execute.service.remote/admin";
const VAULT_LIST: &str = "ryeos.execute.service.vault/list";
const VAULT_SET: &str = "ryeos.execute.service.vault/set";
const VAULT_DELETE: &str = "ryeos.execute.service.vault/delete";
const REVOKE_CAP: &str = "ryeos.execute.service.identity/revoke-delegation";

fn hosted_node_key() -> SigningKey {
    SigningKey::from_bytes(&[171_u8; 32])
}

fn hosted_operator_key() -> SigningKey {
    SigningKey::from_bytes(&[172_u8; 32])
}

fn u2_key() -> SigningKey {
    SigningKey::from_bytes(&[173_u8; 32])
}

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn authorized_keys_dir(state_path: &Path) -> std::path::PathBuf {
    state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/auth/authorized_keys")
}

fn public_b64(key: &SigningKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
}

/// Grant `key` a `local_client` grant on the daemon at `state_path`.
fn authorize_local_client(
    state_path: &Path,
    fixture: &FastFixture,
    key: &SigningKey,
    scopes: &[&str],
) -> Result<()> {
    ryeos_node_runtime::identity::write_authorized_key_toml(
        &authorized_keys_dir(state_path),
        &fingerprint(key),
        &public_b64(key),
        &strings(scopes),
        "delegation-e2e-local",
        &fixture.node_fp(),
        &lillux::time::iso8601_now(),
        &ryeos_node_runtime::identity::GrantTerms::for_tests(),
        &fixture.node,
        ryeos_node_runtime::identity::WildcardPolicy::Reject,
    )?;
    Ok(())
}

fn authorize_remote_node(
    state_path: &Path,
    fixture: &FastFixture,
    node: &SigningKey,
    site_id: &str,
) -> Result<()> {
    ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
        &authorized_keys_dir(state_path),
        &fingerprint(node),
        &public_b64(node),
        &[ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE.to_owned()],
        "delegation-e2e-origin-node",
        &fixture.node_fp(),
        &lillux::time::iso8601_now(),
        &ryeos_node_runtime::identity::GrantTerms::for_tests(),
        site_id,
        &fixture.node,
    )?;
    Ok(())
}

fn authorize_remote_principal(
    state_path: &Path,
    fixture: &FastFixture,
    principal: &SigningKey,
    site_id: &str,
    scopes: &[&str],
) -> Result<()> {
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(&identity_path)?;
    ryeos_node_runtime::identity::reconcile_authorized_key_toml_scopes(
        &authorized_keys_dir(state_path),
        &fingerprint(principal),
        &public_b64(principal),
        &strings(scopes),
        "delegation-e2e-principal",
        &fixture.node_fp(),
        &lillux::time::iso8601_now(),
        &ryeos_node_runtime::identity::GrantTerms::for_tests(),
        &identity,
        ryeos_node_runtime::identity::WildcardPolicy::Reject,
        false,
        Some(site_id),
        false,
    )?;
    Ok(())
}

struct Pair {
    origin: DaemonHarness,
    origin_fixture: FastFixture,
    hosted: DaemonHarness,
    hosted_fixture: FastFixture,
    http: reqwest::Client,
}

async fn start_pair() -> Result<Pair> {
    let origin_node = common::fast_fixture::node_signing_key();
    let u1 = common::fast_fixture::user_signing_key();
    let u2 = u2_key();
    let origin_site = format!("site:{}", fingerprint(&origin_node));
    let (origin, origin_fixture) = DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            authorize_local_client(
                state_path,
                fixture,
                &u2,
                &[
                    "ryeos.execute.service.remote/delegate",
                    THREADS_CAP,
                    "ryeos.execute.service.remote/delegation-revoke",
                ],
            )
        },
        |_| {},
    )
    .await?;
    let (hosted, hosted_fixture) = DaemonHarness::start_fast_with_node_and_user_keys(
        hosted_node_key(),
        hosted_operator_key(),
        |state_path, _, fixture| {
            authorize_remote_node(state_path, fixture, &origin_node, &origin_site)?;
            authorize_remote_principal(
                state_path,
                fixture,
                &u1,
                &origin_site,
                &[VAULT_LIST, VAULT_SET, REVOKE_CAP],
            )?;
            authorize_remote_principal(state_path, fixture, &u2, &origin_site, &[VAULT_LIST])?;
            Ok(())
        },
        |_| {},
    )
    .await?;
    let hosted_node = hosted_node_key();
    let public =
        base64::engine::general_purpose::STANDARD.encode(hosted_node.verifying_key().as_bytes());
    ryeos_api::remote::config::save_remotes(
        &origin.state_path,
        &HashMap::from([(
            HOSTED_REMOTE.to_owned(),
            ryeos_api::remote::config::RemoteConfig {
                name: HOSTED_REMOTE.to_owned(),
                url: format!("http://{}", hosted.bind),
                principal_id: format!("fp:{}", fingerprint(&hosted_node)),
                signing_key: format!("ed25519:{public}"),
                site_id: hosted_fixture.site_id(),
                vault_fingerprint: "sha256:delegation-e2e-vault".to_owned(),
                ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
                project_bindings: HashMap::new(),
            },
        )]),
    )?;
    Ok(Pair {
        origin,
        origin_fixture,
        hosted,
        hosted_fixture,
        http: reqwest::Client::new(),
    })
}

impl Pair {
    /// Call an ORIGIN service as `key`.
    async fn origin_service(
        &self,
        key: &SigningKey,
        service: &str,
        parameters: Value,
    ) -> Result<(reqwest::StatusCode, Value)> {
        let body = json!({
            "item_ref": service,
            "ref_bindings": {},
            "parameters": parameters,
            "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
        });
        let bytes = serde_json::to_vec(&body)?;
        let mut request = self
            .http
            .post(format!("http://{}/execute", self.origin.bind))
            .header("content-type", "application/json")
            .body(bytes.clone());
        for (name, value) in common::build_signed_headers_for_bytes(
            key,
            &self.origin_fixture.node,
            "POST",
            "/execute",
            &bytes,
        ) {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let status = response.status();
        let value = response.json().await.unwrap_or(json!({}));
        Ok((status, value))
    }

    /// Sign a delegation with `key` for `hosted`, the way `ryeos remote
    /// delegate` does, and store it on ORIGIN.
    async fn delegate(&self, key: &SigningKey, scopes: &[&str]) -> Result<SignedDelegation> {
        let (status, template) = self
            .origin_service(
                key,
                "service:remote/delegate",
                json!({"remote": HOSTED_REMOTE}),
            )
            .await?;
        ensure!(
            status.is_success(),
            "delegation template: {status} {template}"
        );
        let facts = &template["result"]["template"];
        let field = |name: &str| -> Result<String> {
            facts[name]
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("template omits {name}"))
        };
        let body = DelegationBody::new(
            &field("delegator")?,
            DelegationParties {
                delegate_site: &field("delegate_site")?,
                delegate_node: &field("delegate_node")?,
                audience_site: &field("audience_site")?,
                audience_node: &field("audience_node")?,
            },
            &strings(scopes),
            now() + 600,
        );
        let signed = SignedDelegation::sign(body, key)?;
        self.store(key, &signed).await?;
        Ok(signed)
    }

    async fn store(&self, key: &SigningKey, signed: &SignedDelegation) -> Result<()> {
        let (status, stored) = self
            .origin_service(
                key,
                "service:remote/delegate",
                json!({"remote": HOSTED_REMOTE, "delegation": signed}),
            )
            .await?;
        ensure!(status.is_success(), "store delegation: {status} {stored}");
        Ok(())
    }

    /// A delegation signed by `key` for HOSTED with explicit parties, for the
    /// forged-request scenarios.
    fn forged_delegation(
        &self,
        key: &SigningKey,
        audience_site: &str,
        not_after: u64,
        scopes: &[&str],
    ) -> SignedDelegation {
        let origin_node = common::fast_fixture::node_signing_key();
        let body = DelegationBody::new(
            &format!("fp:{}", fingerprint(key)),
            DelegationParties {
                delegate_site: &self.origin_fixture.site_id(),
                delegate_node: &format!("fp:{}", fingerprint(&origin_node)),
                audience_site,
                audience_node: &format!("fp:{}", self.hosted_fixture.node_fp()),
            },
            &strings(scopes),
            not_after,
        );
        SignedDelegation::sign(body, key).expect("sign forged delegation")
    }

    /// POST to HOSTED's `/revoke-delegation`, signed by `primary` and
    /// co-signed by ORIGIN's node key as the forwarding authority.
    async fn hosted_post(
        &self,
        primary: &SigningKey,
        delegation: Option<&SignedDelegation>,
        body: &Value,
    ) -> Result<(reqwest::StatusCode, String)> {
        let path = "/revoke-delegation";
        let origin_node = common::fast_fixture::node_signing_key();
        let audience = format!("fp:{}", self.hosted_fixture.node_fp());
        let bytes = serde_json::to_vec(body)?;
        let timestamp = now().to_string();
        let nonce = format!("{:016x}", rand::random::<u64>());
        let body_hash = lillux::cas::sha256_hex(&bytes);
        let to_sign = format!(
            "ryeos-request-v1\nPOST\n{path}\n{body_hash}\n{timestamp}\n{nonce}\n{audience}"
        );
        let signature = lillux::crypto::Signer::sign(
            primary,
            lillux::cas::sha256_hex(to_sign.as_bytes()).as_bytes(),
        );
        let signature_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let key_id = format!("fp:{}", fingerprint(primary));
        let forwarding_key_id = format!("fp:{}", fingerprint(&origin_node));
        let site = self.origin_fixture.site_id();
        let forwarded = format!(
            "ryeos-forwarded-request-v1\nPOST\n{path}\n{body_hash}\n{timestamp}\n{nonce}\n{audience}\n{key_id}\n{signature_b64}\n{forwarding_key_id}\n{site}"
        );
        let forwarding_signature = lillux::crypto::Signer::sign(
            &origin_node,
            lillux::cas::sha256_hex(forwarded.as_bytes()).as_bytes(),
        );
        let mut request = self
            .http
            .post(format!("http://{}{path}", self.hosted.bind))
            .header("content-type", "application/json")
            .header("x-ryeos-key-id", key_id)
            .header("x-ryeos-timestamp", timestamp)
            .header("x-ryeos-nonce", nonce)
            .header("x-ryeos-signature", signature_b64)
            .header("x-ryeos-forwarding-key-id", forwarding_key_id)
            .header("x-ryeos-forwarding-site-id", &site)
            .header(
                "x-ryeos-forwarding-signature",
                base64::engine::general_purpose::STANDARD.encode(forwarding_signature.to_bytes()),
            )
            .body(bytes);
        if let Some(delegation) = delegation {
            request = request.header(
                ryeos_node_runtime::remote_delegation::DELEGATION_HEADER,
                delegation.to_header_value()?,
            );
        }
        let response = request.send().await?;
        let status = response.status();
        Ok((status, response.text().await?))
    }

    /// POST to HOSTED signed by ORIGIN's node key, carrying `delegation`.
    async fn hosted_as_node(
        &self,
        delegation: &SignedDelegation,
        digest: &str,
    ) -> Result<(reqwest::StatusCode, String)> {
        self.hosted_post(
            &common::fast_fixture::node_signing_key(),
            Some(delegation),
            &json!({"delegator": delegation.body.delegator, "delegation_digest": digest}),
        )
        .await
    }

    fn tombstone_path(&self, digest: &str) -> std::path::PathBuf {
        self.hosted
            .state_path
            .join(ryeos_engine::AI_DIR)
            .join("node/auth/delegation_revocations")
            .join(format!("{digest}.toml"))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_call_is_recorded_on_hosted_as_the_principal_and_revocation_sticks()
-> Result<()> {
    let pair = start_pair().await?;
    let u1 = common::fast_fixture::user_signing_key();
    let u1_principal = format!("fp:{}", fingerprint(&u1));
    let delegation = pair
        .delegate(&u1, &[THREADS_CAP, VAULT_LIST, REVOKE_CAP])
        .await?;
    let digest = delegation.digest()?;

    // The delegated call reaches HOSTED and is admitted as U1.
    let (status, body) = pair
        .origin_service(
            &u1,
            "service:remote/threads",
            json!({"remote": HOSTED_REMOTE}),
        )
        .await?;
    ensure!(
        status.is_success(),
        "delegated threads call: {status} {body}"
    );

    // A forwarded request carrying the delegation is recorded as the
    // delegating principal, with the forwarding site named.
    let probe = lillux::sha256_hex(b"recorded-as-the-principal");
    let (status, text) = pair.hosted_as_node(&delegation, &probe).await?;
    ensure!(status.is_success(), "forwarded request: {status} {text}");
    let recorded: Value = serde_json::from_str(&text)?;
    let result = recorded.get("result").unwrap_or(&recorded);
    assert_eq!(result["revoked_by"], u1_principal.as_str());
    assert_eq!(
        result["delegated_via"],
        pair.origin_fixture.site_id().as_str()
    );

    // Revoking from ORIGIN tombstones the digest on HOSTED and drops the local
    // record.
    let (status, body) = pair
        .origin_service(
            &u1,
            "service:remote/delegation-revoke",
            json!({"remote": HOSTED_REMOTE}),
        )
        .await?;
    ensure!(status.is_success(), "revoke: {status} {body}");
    assert_eq!(body["result"]["remote_tombstoned"], true);
    let ledger = std::fs::read_to_string(pair.tombstone_path(&digest))
        .context("hosted holds no tombstone for the revoked delegation")?;
    assert!(
        ledger.contains(&format!("revoked_by = \"{u1_principal}\"")),
        "{ledger}"
    );
    assert!(ledger.contains("delegated_via = "), "{ledger}");

    let (status, body) = pair
        .origin_service(
            &u1,
            "service:remote/threads",
            json!({"remote": HOSTED_REMOTE}),
        )
        .await?;
    assert!(!status.is_success());
    assert!(
        body.to_string().contains("remote_delegation_missing"),
        "{body}"
    );

    // Storing the same signed delegation again does not resurrect it: HOSTED
    // refuses the tombstoned digest.
    pair.store(&u1, &delegation).await?;
    let (status, body) = pair
        .origin_service(
            &u1,
            "service:remote/threads",
            json!({"remote": HOSTED_REMOTE}),
        )
        .await?;
    assert!(!status.is_success());
    assert!(
        body.to_string().contains("remote_delegation_revoked"),
        "{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn effective_scopes_are_the_grant_intersected_with_the_delegation() -> Result<()> {
    let pair = start_pair().await?;
    let u1 = common::fast_fixture::user_signing_key();
    // U1's grant on HOSTED holds vault/list, vault/set and revoke; the
    // delegation lends vault/list (granted) and vault/delete (not granted).
    pair.delegate(&u1, &[THREADS_CAP, VAULT_LIST, VAULT_DELETE])
        .await?;

    // Granted but not lent.
    let (status, body) = pair
        .origin_service(
            &u1,
            "service:remote/vault-set",
            json!({"remote": HOSTED_REMOTE, "name": "N", "value": "v"}),
        )
        .await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    // Lent but not granted.
    let (status, body) = pair
        .origin_service(
            &u1,
            "service:remote/vault-delete",
            json!({"remote": HOSTED_REMOTE, "name": "N"}),
        )
        .await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    // Both granted and lent: not refused for authority.
    let (status, body) = pair
        .origin_service(
            &u1,
            "service:remote/vault-list",
            json!({"remote": HOSTED_REMOTE}),
        )
        .await?;
    assert_ne!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    assert_ne!(status, reqwest::StatusCode::UNAUTHORIZED, "{body}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_principal_without_a_delegation_is_refused_on_origin_and_capped_on_hosted() -> Result<()>
{
    let pair = start_pair().await?;
    let u2 = u2_key();
    let (status, body) = pair
        .origin_service(
            &u2,
            "service:remote/threads",
            json!({"remote": HOSTED_REMOTE}),
        )
        .await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.to_string().contains("remote_delegation_missing"),
        "{body}"
    );
    assert!(
        body.to_string()
            .contains("ryeos remote delegate --remote hosted"),
        "{body}"
    );

    // With a delegation that lends more than U2's grant on HOSTED holds, HOSTED
    // still refuses the capability outside the grant.
    pair.delegate(&u2, &[THREADS_CAP, VAULT_LIST, VAULT_SET])
        .await?;
    let (status, body) = pair
        .origin_service(
            &u2,
            "service:remote/vault-set",
            json!({"remote": HOSTED_REMOTE, "name": "N", "value": "v"}),
        )
        .await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn hosted_refuses_wrong_audience_expired_tampered_and_transitive_delegations() -> Result<()> {
    let pair = start_pair().await?;
    let u1 = common::fast_fixture::user_signing_key();
    let hosted_site = pair.hosted_fixture.site_id();
    let probe = lillux::sha256_hex(b"refusal-probe");
    let scopes = [REVOKE_CAP];

    let wrong_audience = pair.forged_delegation(&u1, "site:elsewhere", now() + 600, &scopes);
    let (status, text) = pair.hosted_as_node(&wrong_audience, &probe).await?;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert!(text.contains("remote_delegation_wrong_audience"), "{text}");

    let expired = pair.forged_delegation(&u1, &hosted_site, now() - 10, &scopes);
    let (status, text) = pair.hosted_as_node(&expired, &probe).await?;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert!(text.contains("remote_delegation_expired"), "{text}");

    let mut tampered = pair.forged_delegation(&u1, &hosted_site, now() + 600, &scopes);
    tampered.body.scopes = strings(&[REVOKE_CAP, VAULT_SET]);
    let (status, text) = pair.hosted_as_node(&tampered, &probe).await?;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert!(text.contains("remote_delegation_bad_signature"), "{text}");

    // A request already forwarded once, signed by the principal's own key,
    // cannot also carry a delegation.
    let good = pair.forged_delegation(&u1, &hosted_site, now() + 600, &scopes);
    let (status, text) = pair
        .hosted_post(
            &u1,
            Some(&good),
            &json!({"delegator": good.body.delegator, "delegation_digest": probe}),
        )
        .await?;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert!(text.contains("remote_delegation_not_transitive"), "{text}");

    // Positive control: the untampered delegation is admitted.
    let (status, text) = pair.hosted_as_node(&good, &probe).await?;
    ensure!(status.is_success(), "control request: {status} {text}");
    Ok(())
}
