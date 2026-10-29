//! Tests of the application rules: every refusal, the receipts, the ledger
//! order, recovery after a crash, and the rules that keep a bad inbox from
//! stopping a node.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_node_runtime::identity::{WildcardPolicy, reconcile_authorized_key_toml_scopes};
use serde_json::{Value, json};

use super::*;
use crate::offline_operation::contract::genesis_ledger_head;
use crate::offline_operation::contract::{MAX_REQUEST_BYTES, SignedRefusal};
use crate::offline_operation::envelope::authorized_keys_dir;
use crate::offline_operation::facts::{FACT_POLICY_GENERATION, FACT_RESTART_INHIBITION};
use crate::offline_operation::host::BareHost;

struct Fixture {
    root: tempfile::TempDir,
    node: NodeIdentity,
    operator: SigningKey,
    store: Store,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let runtime = RuntimeRoot::new(root.path().to_path_buf());
    let node = NodeIdentity::create(&runtime.node_signing_key_path()).unwrap();
    let operator =
        lillux::crypto::create_signing_key(&runtime.operator_signing_key_path()).unwrap();
    std::fs::create_dir_all(runtime.state()).unwrap();
    let store = Store::for_app_root(root.path());
    store.create_layout().unwrap();
    Fixture {
        root,
        node,
        operator,
        store,
    }
}

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn stamp(offset_secs: i64) -> String {
    lillux::time::iso8601_from_unix_secs(
        lillux::time::unix_secs_now()
            .checked_add_signed(offset_secs)
            .unwrap(),
    )
}

impl Fixture {
    fn path(&self) -> &Path {
        self.root.path()
    }

    fn facts(&self) -> NodeFacts {
        read_facts(self.path()).unwrap()
    }

    /// A body signed against the node's present facts, for the local operator.
    fn body(&self, operation: &str, arguments: Value) -> RequestBody {
        let facts = self.facts();
        let precondition = match lookup(operation) {
            Some(spec) => MANDATORY_FACTS
                .iter()
                .chain(spec.preconditions)
                .map(|fact| ((*fact).to_owned(), facts[*fact].clone()))
                .collect(),
            None => BTreeMap::new(),
        };
        RequestBody {
            schema: REQUEST_SCHEMA.to_owned(),
            node_fingerprint: self.node.fingerprint().to_owned(),
            signer_fingerprint: fingerprint(&self.operator),
            signer_grant_digest: None,
            signer_origin_site_id: None,
            operation: operation.to_owned(),
            arguments,
            inputs: BTreeMap::new(),
            expected_result: BTreeMap::new(),
            precondition,
            issued_at: stamp(-30),
            not_after: stamp(3600),
        }
    }

    fn restart_body(&self) -> RequestBody {
        self.body("node.allow-restart", json!({}))
    }

    fn sign(&self, body: RequestBody) -> SignedRequest {
        SignedRequest::sign(body, &self.operator).unwrap()
    }

    /// Put a request in the inbox under the digest of its bytes.
    fn drop_request(&self, request: &SignedRequest) -> String {
        let bytes = request.to_bytes().unwrap();
        self.drop_bytes(&bytes)
    }

    fn drop_bytes(&self, bytes: &[u8]) -> String {
        let name = lillux::sha256_hex(bytes);
        std::fs::write(self.store.inbox_dir().join(format!("{name}.json")), bytes).unwrap();
        name
    }

    fn drop_input(&self, bytes: &[u8]) -> String {
        let digest = lillux::sha256_hex(bytes);
        std::fs::write(
            self.store.inbox_dir().join(format!("{digest}.input")),
            bytes,
        )
        .unwrap();
        digest
    }

    fn run(&self) -> ApplyReport {
        apply_pending_offline_operations(self.path(), &BareHost).unwrap()
    }

    fn run_with(&self, options: &ApplyOptions) -> ApplyReport {
        apply_with_options(self.path(), &BareHost, options).unwrap()
    }

    fn refusals(&self) -> Vec<SignedRefusal> {
        self.store.refusals().unwrap()
    }

    fn ledger(&self) -> LedgerState {
        read_ledger(self.path()).unwrap()
    }

    fn inbox_names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.store.inbox_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn only_refusal(&self) -> RefusalReason {
        let refusals = self.refusals();
        assert_eq!(refusals.len(), 1, "{refusals:?}");
        refusals[0].body.reason
    }

    /// A remote-operator grant, returning its fingerprint, key and digest.
    fn remote_operator(
        &self,
        scopes: &[&str],
        site: &str,
        wildcard: WildcardPolicy,
    ) -> (SigningKey, String) {
        let key = lillux::crypto::generate_signing_key();
        let digest = self.write_grant(&key, scopes, site, wildcard);
        (key, digest)
    }

    fn write_grant(
        &self,
        key: &SigningKey,
        scopes: &[&str],
        site: &str,
        wildcard: WildcardPolicy,
    ) -> String {
        self.try_write_grant(key, scopes, site, wildcard).unwrap()
    }

    fn local_wildcard_grant(&self) {
        let key = lillux::crypto::generate_signing_key();
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes());
        reconcile_authorized_key_toml_scopes(
            &authorized_keys_dir(self.path()),
            &fingerprint(&key),
            &encoded,
            &["*".to_owned()],
            "local operator",
            "test",
            "2026-01-01T00:00:00Z",
            &self.node,
            WildcardPolicy::AllowBootstrap,
            false,
            None,
            true,
        )
        .unwrap();
    }

    fn try_write_grant(
        &self,
        key: &SigningKey,
        scopes: &[&str],
        site: &str,
        wildcard: WildcardPolicy,
    ) -> anyhow::Result<String> {
        let fp = fingerprint(key);
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes());
        let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
        let directory = authorized_keys_dir(self.path());
        reconcile_authorized_key_toml_scopes(
            &directory,
            &fp,
            &encoded,
            &scopes,
            "remote operator",
            "test",
            "2026-01-01T00:00:00Z",
            &self.node,
            wildcard,
            false,
            Some(site),
            true,
        )?;
        Ok(lillux::sha256_hex(&std::fs::read(
            directory.join(format!("{fp}.toml")),
        )?))
    }

    fn remote_body(
        &self,
        key: &SigningKey,
        grant_digest: &str,
        site: &str,
        operation: &str,
    ) -> RequestBody {
        let mut body = self.body(operation, json!({}));
        body.signer_fingerprint = fingerprint(key);
        body.signer_grant_digest = Some(grant_digest.to_owned());
        body.signer_origin_site_id = Some(site.to_owned());
        body
    }
}

const RESTART_SCOPE: &str = "ryeos.offline.node.allow-restart";
const SITE: &str = "site:origin-a";

// ── The first-cut happy path ─────────────────────────────────────────────

#[test]
fn a_local_operators_request_is_applied_and_receipted() {
    let fx = fixture();
    let request = fx.sign(fx.restart_body());
    let digest = request.digest().unwrap();
    fx.drop_request(&request);
    let report = fx.run();
    assert_eq!(report.applied.len(), 1, "{:?}", report.refused);
    assert_eq!(report.applied[0].operation, "node.allow-restart");
    let state = fx.ledger();
    assert_eq!(state.last_sequence, 1);
    assert!(state.request_digests.contains(&digest));
    assert!(
        fx.inbox_names().is_empty(),
        "request is removed after its receipt"
    );
    assert!(fx.store.pending_digests().unwrap().is_empty());
    // The receipt is node-signed and carries the request it was applied under.
    let node =
        NodeIdentity::load(&RuntimeRoot::new(fx.path().to_path_buf()).node_signing_key_path())
            .unwrap();
    let receipt = Ledger::new(&fx.store, &node)
        .find_receipt(&state, &digest)
        .unwrap()
        .unwrap();
    receipt.verify(node.verifying_key()).unwrap();
    assert_eq!(receipt.body.request, request);
    assert_eq!(
        receipt.body.previous,
        genesis_ledger_head(node.fingerprint())
    );
}

#[test]
fn startup_reports_every_fact_and_the_ledger_head() {
    let fx = fixture();
    let report = fx.run();
    for fact in crate::offline_operation::facts::ALL_FACTS {
        assert!(report.facts.contains_key(*fact), "{fact}");
    }
    let lines = report.log_lines().join("\n");
    assert!(lines.contains("fact ledger_head="), "{lines}");
    assert!(lines.contains("fact init_completion=absent"), "{lines}");
}

#[test]
fn an_uninitialized_node_is_left_alone() {
    let root = tempfile::tempdir().unwrap();
    let report = apply_pending_offline_operations(root.path(), &BareHost).unwrap();
    assert_eq!(report.skipped, Some(Skipped::NotInitialized));
    assert!(!root.path().join(".ai").exists(), "nothing was created");
}

// ── Bounds and shape ─────────────────────────────────────────────────────

#[test]
fn an_oversized_document_is_refused_unread() {
    let fx = fixture();
    let name = "a".repeat(64);
    std::fs::write(
        fx.store.inbox_dir().join(format!("{name}.json")),
        vec![b' '; usize::try_from(MAX_REQUEST_BYTES).unwrap() + 1],
    )
    .unwrap();
    let report = fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::Malformed);
    assert_eq!(report.refused.len(), 1);
    assert!(fx.inbox_names().is_empty());
}

#[test]
fn a_document_that_is_not_a_request_is_refused() {
    let fx = fixture();
    fx.drop_bytes(b"{\"hello\": \"world\"}");
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::Malformed);
}

#[test]
fn a_request_not_named_by_its_digest_is_refused() {
    let fx = fixture();
    let request = fx.sign(fx.restart_body());
    let bytes = request.to_bytes().unwrap();
    std::fs::write(
        fx.store
            .inbox_dir()
            .join(format!("{}.json", "b".repeat(64))),
        bytes,
    )
    .unwrap();
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::Malformed);
    assert_eq!(fx.ledger().last_sequence, 0);
}

#[test]
fn an_inbox_of_twenty_junk_requests_is_cleared_without_stopping() {
    let fx = fixture();
    for index in 0..20_u8 {
        fx.drop_bytes(format!("{{\"junk\": {index}}}").as_bytes());
    }
    let report = fx.run();
    assert_eq!(report.refused.len(), 20);
    assert!(fx.inbox_names().is_empty());
    assert!(fx.refusals().len() <= crate::offline_operation::store::MAX_REFUSAL_RECEIPTS);
}

#[test]
fn only_digest_named_files_are_read_and_the_rest_is_ignored() {
    let fx = fixture();
    std::fs::write(fx.store.inbox_dir().join("notes.txt"), b"hello").unwrap();
    std::fs::write(fx.store.inbox_dir().join("request.json.tmp"), b"{}").unwrap();
    let report = fx.run();
    assert!(report.refused.is_empty());
    assert_eq!(fx.inbox_names(), ["notes.txt", "request.json.tmp"]);
}

// ── Envelope: node, signature, grant, scope ──────────────────────────────

#[test]
fn a_request_for_another_node_is_refused() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.node_fingerprint = "c".repeat(64);
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::WrongNode);
}

#[test]
fn an_unknown_signer_is_refused() {
    let fx = fixture();
    let stranger = lillux::crypto::generate_signing_key();
    let mut body = fx.restart_body();
    body.signer_fingerprint = fingerprint(&stranger);
    fx.drop_request(&SignedRequest::sign(body, &stranger).unwrap());
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::UnknownSigner);
}

#[test]
fn a_forged_signature_is_refused() {
    let fx = fixture();
    let mut request = fx.sign(fx.restart_body());
    request.body.arguments = json!({ "tampered": true });
    fx.drop_request(&request);
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::BadSignature);
}

#[test]
fn a_signature_by_another_key_under_the_operators_name_is_refused() {
    let fx = fixture();
    let forger = lillux::crypto::generate_signing_key();
    let request = SignedRequest::sign(fx.restart_body(), &forger).unwrap();
    fx.drop_request(&request);
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::BadSignature);
}

#[test]
fn an_unknown_operation_is_refused() {
    let fx = fixture();
    let body = fx.body("node.does-not-exist", json!({}));
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::UnknownOperation);
}

#[test]
fn a_remote_operator_with_the_exact_scope_is_applied() {
    let fx = fixture();
    let (key, grant) = fx.remote_operator(&[RESTART_SCOPE], SITE, WildcardPolicy::Reject);
    let body = fx.remote_body(&key, &grant, SITE, "node.allow-restart");
    let request = SignedRequest::sign(body, &key).unwrap();
    fx.drop_request(&request);
    let report = fx.run();
    assert_eq!(report.applied.len(), 1, "{:?}", fx.refusals());
}

#[test]
fn wildcard_grants_never_satisfy_an_offline_scope() {
    let fx = fixture();
    for scopes in [vec!["*"], vec!["ryeos.*"], vec!["ryeos.offline"]] {
        let key = lillux::crypto::generate_signing_key();
        let Ok(grant) = fx.try_write_grant(&key, &scopes, SITE, WildcardPolicy::AllowBootstrap)
        else {
            // A scope the grammar refuses cannot be granted at all.
            continue;
        };
        let body = fx.remote_body(&key, &grant, SITE, "node.allow-restart");
        fx.drop_request(&SignedRequest::sign(body, &key).unwrap());
        let report = fx.run();
        assert!(report.applied.is_empty(), "{scopes:?}");
        assert!(
            fx.refusals()
                .iter()
                .any(|r| r.body.reason == RefusalReason::ScopeNotGranted),
            "{scopes:?}"
        );
    }
}

#[test]
fn a_grant_cannot_carry_a_wildcard_under_the_offline_prefix() {
    let fx = fixture();
    for scope in [
        "ryeos.offline.*",
        "ryeos.offline.node.*",
        "ryeos.offline.node.reset.*",
    ] {
        let key = lillux::crypto::generate_signing_key();
        for wildcard in [WildcardPolicy::Reject, WildcardPolicy::AllowBootstrap] {
            let error = fx
                .try_write_grant(&key, &[scope], SITE, wildcard)
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("wildcard"),
                "{scope}: {error:#}"
            );
        }
    }
}

#[test]
fn wildcard_grants_are_reported_at_startup() {
    let fx = fixture();
    // Remote grants refuse wildcards, so only a local grant can carry one.
    fx.local_wildcard_grant();
    let report = fx.run();
    assert_eq!(report.wildcard_grants.len(), 1);
    assert!(
        report.wildcard_grants[0]
            .operations
            .contains(&"node.allow-restart")
    );
}

#[test]
fn a_remote_operator_without_the_scope_is_refused() {
    let fx = fixture();
    let (key, grant) = fx.remote_operator(
        &["ryeos.execute.service.remote/run"],
        SITE,
        WildcardPolicy::Reject,
    );
    let body = fx.remote_body(&key, &grant, SITE, "node.allow-restart");
    fx.drop_request(&SignedRequest::sign(body, &key).unwrap());
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::ScopeNotGranted);
}

#[test]
fn a_grant_that_changed_after_signing_is_refused() {
    let fx = fixture();
    let (key, old_grant) = fx.remote_operator(&[RESTART_SCOPE], SITE, WildcardPolicy::Reject);
    // The grant is rewritten (a scope is added) after the request was signed.
    fx.write_grant(
        &key,
        &[RESTART_SCOPE, "ryeos.execute.service.remote/run"],
        SITE,
        WildcardPolicy::Reject,
    );
    let body = fx.remote_body(&key, &old_grant, SITE, "node.allow-restart");
    fx.drop_request(&SignedRequest::sign(body, &key).unwrap());
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::GrantChanged);
}

#[test]
fn a_remote_request_without_a_grant_digest_is_refused() {
    let fx = fixture();
    let (key, grant) = fx.remote_operator(&[RESTART_SCOPE], SITE, WildcardPolicy::Reject);
    let mut body = fx.remote_body(&key, &grant, SITE, "node.allow-restart");
    body.signer_grant_digest = None;
    body.signer_origin_site_id = None;
    fx.drop_request(&SignedRequest::sign(body, &key).unwrap());
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::GrantChanged);
}

#[test]
fn a_request_naming_another_origin_site_is_refused() {
    let fx = fixture();
    let (key, grant) = fx.remote_operator(&[RESTART_SCOPE], SITE, WildcardPolicy::Reject);
    let body = fx.remote_body(&key, &grant, "site:somewhere-else", "node.allow-restart");
    fx.drop_request(&SignedRequest::sign(body, &key).unwrap());
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::OriginMismatch);
}

#[test]
fn a_remote_node_grant_may_not_sign() {
    let fx = fixture();
    let key = lillux::crypto::generate_signing_key();
    let fp = fingerprint(&key);
    let encoded = base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes());
    ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
        &authorized_keys_dir(fx.path()),
        &fp,
        &encoded,
        &[RESTART_SCOPE.to_owned()],
        "peer",
        "test",
        "2026-01-01T00:00:00Z",
        SITE,
        fx.node.signing_key(),
    )
    .unwrap();
    let digest = lillux::sha256_hex(
        &std::fs::read(authorized_keys_dir(fx.path()).join(format!("{fp}.toml"))).unwrap(),
    );
    let body = fx.remote_body(&key, &digest, SITE, "node.allow-restart");
    fx.drop_request(&SignedRequest::sign(body, &key).unwrap());
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::SignerClassNotPermitted);
}

#[test]
fn an_authority_bearing_operation_is_not_delegable_by_default() {
    let fx = fixture();
    let scope = "ryeos.offline.node.policy-apply";
    let (key, grant) = fx.remote_operator(&[scope], SITE, WildcardPolicy::Reject);
    let mut body = fx.remote_body(&key, &grant, SITE, "node.policy-apply");
    body.arguments = json!({ "section": "external_content" });
    fx.drop_request(&SignedRequest::sign(body, &key).unwrap());
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::NotDelegable);
}

#[test]
fn only_the_local_operator_may_reset_authorization() {
    let fx = fixture();
    let scope = "ryeos.offline.node.reset.authorization";
    let (key, grant) = fx.remote_operator(&[scope], SITE, WildcardPolicy::Reject);
    let body = fx.remote_body(&key, &grant, SITE, "node.reset.authorization");
    fx.drop_request(&SignedRequest::sign(body, &key).unwrap());
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::LocalSignerOnly);
}

// ── Time ─────────────────────────────────────────────────────────────────

#[test]
fn an_expired_request_is_refused() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.issued_at = stamp(-10800);
    body.not_after = stamp(-7200);
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::Expired);
}

#[test]
fn a_request_from_the_future_is_refused() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.issued_at = stamp(1800);
    body.not_after = stamp(7200);
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::FromTheFuture);
}

#[test]
fn a_lifetime_beyond_a_day_is_refused() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.issued_at = stamp(-60);
    body.not_after = stamp(108000);
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::InvalidLifetime);
}

// ── Schema: arguments, inputs, facts, results ────────────────────────────

#[test]
fn unknown_arguments_are_refused() {
    let fx = fixture();
    let body = fx.body("node.allow-restart", json!({ "force": true }));
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::InvalidArguments);
}

#[test]
fn an_input_slot_the_operation_lacks_is_refused() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.inputs.insert("extra".into(), "d".repeat(64));
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::InvalidInputs);
}

#[test]
fn a_missing_required_input_slot_is_refused() {
    let fx = fixture();
    let mut body = fx.body(
        "node.policy-apply",
        json!({ "section": "external_content" }),
    );
    body.expected_result
        .insert("policy_generation".into(), "e".repeat(64));
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::InvalidInputs);
}

#[test]
fn a_request_must_pin_exactly_the_facts_its_operation_names() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.precondition.remove(FACT_RESTART_INHIBITION);
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::PreconditionFailed);
}

#[test]
fn a_result_fact_the_operation_cannot_report_is_refused() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.expected_result
        .insert("anything".into(), "f".repeat(64));
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::ExpectedResultMismatch);
}

#[test]
fn a_required_result_fact_must_be_pinned() {
    let fx = fixture();
    let mut body = fx.body(
        "node.policy-apply",
        json!({ "section": "external_content" }),
    );
    body.inputs.insert("policy".into(), "1".repeat(64));
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::ExpectedResultMismatch);
}

#[test]
fn the_handlers_result_is_checked_against_the_pinned_facts() {
    let result = json!({ "settings_digest": "a".repeat(64) });
    let mut expected = BTreeMap::new();
    expected.insert("settings_digest".to_owned(), "a".repeat(64));
    verify_expected(&expected, &result).unwrap();
    expected.insert("settings_digest".to_owned(), "b".repeat(64));
    assert_eq!(
        verify_expected(&expected, &result).unwrap_err().reason,
        RefusalReason::ExpectedResultMismatch
    );
    expected.clear();
    expected.insert("absent_fact".to_owned(), "a".repeat(64));
    assert!(verify_expected(&expected, &result).is_err());
}

// ── Replay, order and preconditions ──────────────────────────────────────

#[test]
fn a_request_in_the_ledger_is_refused_as_a_replay() {
    let fx = fixture();
    let request = fx.sign(fx.restart_body());
    fx.drop_request(&request);
    assert_eq!(fx.run().applied.len(), 1);
    fx.drop_request(&request);
    let report = fx.run();
    assert!(report.applied.is_empty());
    assert_eq!(fx.only_refusal(), RefusalReason::Replayed);
    assert_eq!(fx.ledger().last_sequence, 1);
}

#[test]
fn of_two_requests_signed_against_one_head_the_earlier_applies() {
    let fx = fixture();
    let mut early = fx.restart_body();
    early.issued_at = stamp(-600);
    let mut late = fx.restart_body();
    late.issued_at = stamp(-300);
    late.arguments = json!({});
    // Distinct documents: a different lifetime gives a different digest.
    late.not_after = stamp(7200);
    let early = fx.sign(early);
    let late = fx.sign(late);
    fx.drop_request(&late);
    fx.drop_request(&early);
    let report = fx.run();
    assert_eq!(report.applied.len(), 1);
    assert_eq!(report.applied[0].request_digest, early.digest().unwrap());
    assert_eq!(fx.only_refusal(), RefusalReason::StaleLedgerHead);
}

#[test]
fn a_request_signed_against_a_head_not_yet_reached_waits() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.precondition
        .insert("ledger_head".into(), "9".repeat(64));
    let request = fx.sign(body);
    let name = fx.drop_request(&request);
    let report = fx.run();
    assert!(report.applied.is_empty() && report.refused.is_empty());
    assert_eq!(report.waiting, 1);
    assert_eq!(fx.inbox_names(), [format!("{name}.json")]);
}

#[test]
fn a_waiting_request_is_refused_once_it_expires() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.precondition
        .insert("ledger_head".into(), "9".repeat(64));
    body.issued_at = stamp(-5400);
    body.not_after = stamp(-1800);
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::Expired);
}

#[test]
fn a_changed_fact_refuses_the_request() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.precondition
        .insert(FACT_RESTART_INHIBITION.into(), "7".repeat(64));
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::PreconditionFailed);
    assert_eq!(fx.ledger().last_sequence, 0);
}

#[test]
fn each_application_pins_the_head_the_last_one_left() {
    let fx = fixture();
    let first = fx.sign(fx.restart_body());
    fx.drop_request(&first);
    fx.run();
    let second = fx.sign(fx.restart_body());
    assert_ne!(
        first.body.precondition["ledger_head"],
        second.body.precondition["ledger_head"]
    );
    fx.drop_request(&second);
    assert_eq!(fx.run().applied.len(), 1);
    assert_eq!(fx.ledger().last_sequence, 2);
}

// ── Inputs ───────────────────────────────────────────────────────────────

fn policy_apply_body(fx: &Fixture, input: &[u8]) -> RequestBody {
    let mut body = fx.body(
        "node.policy-apply",
        json!({ "section": "external_content" }),
    );
    body.inputs
        .insert("policy".into(), lillux::sha256_hex(input));
    body.expected_result
        .insert("policy_generation".into(), "e".repeat(64));
    assert!(body.precondition.contains_key(FACT_POLICY_GENERATION));
    body
}

#[test]
fn a_missing_input_is_refused() {
    let fx = fixture();
    let body = policy_apply_body(&fx, b"schema: 1");
    fx.drop_request(&fx.sign(body));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::InputMissing);
}

#[test]
fn an_input_that_does_not_hash_to_its_pin_is_refused() {
    let fx = fixture();
    let body = policy_apply_body(&fx, b"schema: 1");
    fx.drop_request(&fx.sign(body));
    // Another file under the pinned digest's name.
    let digest = lillux::sha256_hex(b"schema: 1");
    std::fs::write(
        fx.store.inbox_dir().join(format!("{digest}.input")),
        b"schema: 2",
    )
    .unwrap();
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::InputMismatch);
    assert!(
        fx.inbox_names().is_empty(),
        "a refused request's inputs go with it"
    );
}

#[test]
fn an_oversized_input_is_refused_without_being_read() {
    let fx = fixture();
    let big = vec![b'x'; 256 * 1024 + 1];
    let body = policy_apply_body(&fx, &big);
    fx.drop_request(&fx.sign(body));
    fx.drop_input(&big);
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::InputOversized);
    assert!(fx.inbox_names().is_empty());
}

// ── Handler failure, the stuck exit and the time bound ───────────────────

#[test]
fn a_handler_that_keeps_failing_is_stuck_after_three_startups() {
    let fx = fixture();
    // No runtime state exists, so the replay-index reset cannot run.
    let request = fx.sign(fx.body("node.reset.replay-indexes", json!({})));
    let digest = request.digest().unwrap();
    fx.drop_request(&request);
    for attempt in 1..=2 {
        let report = fx.run();
        assert!(
            matches!(report.stopped, Some(StopReason::HandlerFailed { .. })),
            "attempt {attempt}: {:?}",
            report.stopped
        );
        // An ordinary failure leaves no intent record, only its count.
        assert!(fx.store.load_pending(&digest).unwrap().is_none());
        assert_eq!(fx.store.attempts(&digest), attempt);
        assert!(fx.refusals().is_empty());
    }
    let report = fx.run();
    assert_eq!(report.refused.len(), 1);
    assert_eq!(fx.only_refusal(), RefusalReason::Stuck);
    // The refusal says the handler had begun, so its effect may have landed.
    assert!(
        fx.refusals()[0].body.detail.contains("may have landed"),
        "{}",
        fx.refusals()[0].body.detail
    );
    assert!(fx.inbox_names().is_empty());
    assert!(fx.store.pending_digests().unwrap().is_empty());
    assert_eq!(
        fx.store.attempts(&digest),
        0,
        "the counter goes with the request"
    );
    assert_eq!(
        fx.ledger().last_sequence,
        0,
        "a stuck request leaves no receipt"
    );
}

#[test]
fn a_node_that_died_inside_a_handler_three_times_is_stuck() {
    let fx = fixture();
    let request = fx.sign(fx.body("node.reset.replay-indexes", json!({})));
    let digest = request.digest().unwrap();
    let name = fx.drop_request(&request);
    // Three earlier startups began the request and never came back.
    fx.store
        .save_pending(&Pending {
            request_digest: digest.clone(),
            inbox_name: name,
            request,
            phase: Phase::Running,
            measured_preconditions: BTreeMap::new(),
            result: None,
            discarded_grant_digests: Vec::new(),
            started_at: "2026-10-28T00:00:00Z".to_owned(),
        })
        .unwrap();
    fx.store
        .record_attempts(&digest, MAX_STARTUP_ATTEMPTS)
        .unwrap();
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::Stuck);
    assert!(fx.store.pending_digests().unwrap().is_empty());
}

#[test]
fn application_stops_at_a_request_boundary_when_time_is_up() {
    let fx = fixture();
    fx.drop_request(&fx.sign(fx.restart_body()));
    let report = fx.run_with(&ApplyOptions {
        time_budget: Duration::ZERO,
        ..ApplyOptions::default()
    });
    assert_eq!(report.stopped, Some(StopReason::TimeBudget));
    assert!(report.applied.is_empty());
    assert_eq!(
        fx.inbox_names().len(),
        1,
        "the request waits for the next startup"
    );
    assert_eq!(fx.run().applied.len(), 1);
}

#[test]
fn a_held_state_lock_skips_application_without_failing_startup() {
    let fx = fixture();
    let _held =
        StateLock::acquire(&default_lock_path(fx.path())).expect("hold the node's state lock");
    let report = fx.run_with(&ApplyOptions {
        lock_timeout: Duration::from_millis(50),
        ..ApplyOptions::default()
    });
    assert!(matches!(report.skipped, Some(Skipped::LockHeld(_))));
}

// ── Crash recovery ───────────────────────────────────────────────────────

#[test]
fn a_node_that_died_after_the_effect_writes_the_receipt_without_rerunning_it() {
    let fx = fixture();
    // The replay-index reset fails on this node, so reaching a receipt proves
    // the handler was not run again.
    let request = fx.sign(fx.body("node.reset.replay-indexes", json!({})));
    let digest = request.digest().unwrap();
    let name = fx.drop_request(&request);
    fx.store
        .save_pending(&Pending {
            request_digest: digest.clone(),
            inbox_name: name,
            request: request.clone(),
            phase: Phase::Committed,
            measured_preconditions: BTreeMap::new(),
            result: Some(json!({ "done": true })),
            discarded_grant_digests: Vec::new(),
            started_at: "2026-10-28T00:00:00Z".to_owned(),
        })
        .unwrap();
    let report = fx.run();
    assert_eq!(report.applied.len(), 1, "{:?}", report.stopped);
    let state = fx.ledger();
    let node = &fx.node;
    let receipt = Ledger::new(&fx.store, node)
        .find_receipt(&state, &digest)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.body.result, json!({ "done": true }));
    assert!(fx.inbox_names().is_empty());
    assert!(fx.store.pending_digests().unwrap().is_empty());
}

#[test]
fn a_node_that_died_after_the_receipt_only_cleans_up() {
    let fx = fixture();
    let request = fx.sign(fx.restart_body());
    let digest = request.digest().unwrap();
    let name = fx.drop_request(&request);
    // Applied once; then the clean-up is undone as if the node died before it.
    assert_eq!(fx.run().applied.len(), 1);
    fx.drop_bytes(&request.to_bytes().unwrap());
    fx.store
        .save_pending(&Pending {
            request_digest: digest,
            inbox_name: name,
            request,
            phase: Phase::Committed,
            measured_preconditions: BTreeMap::new(),
            result: Some(json!({})),
            discarded_grant_digests: Vec::new(),
            started_at: "2026-10-28T00:00:00Z".to_owned(),
        })
        .unwrap();
    let report = fx.run();
    assert!(report.applied.is_empty() && report.refused.is_empty());
    assert_eq!(fx.ledger().last_sequence, 1, "no second receipt");
    assert!(fx.inbox_names().is_empty());
}

#[test]
fn a_node_that_died_inside_the_handler_reruns_it_against_changed_facts() {
    let fx = fixture();
    let request = fx.sign(fx.restart_body());
    let digest = request.digest().unwrap();
    let name = fx.drop_request(&request);
    // The facts the request pinned no longer hold (the handler had begun
    // changing them), but the application it continues passed them already.
    let mut measured = BTreeMap::new();
    measured.insert(FACT_RESTART_INHIBITION.to_owned(), "7".repeat(64));
    fx.store
        .save_pending(&Pending {
            request_digest: digest.clone(),
            inbox_name: name,
            request,
            phase: Phase::Running,
            measured_preconditions: measured,
            result: None,
            discarded_grant_digests: Vec::new(),
            started_at: "2026-10-28T00:00:00Z".to_owned(),
        })
        .unwrap();
    let report = fx.run();
    assert_eq!(report.applied.len(), 1, "{:?}", report.stopped);
    assert!(fx.ledger().request_digests.contains(&digest));
}

// ── The authorization reset ──────────────────────────────────────────────

fn reset_request(fx: &Fixture) -> SignedRequest {
    fx.sign(fx.body("node.reset.authorization", json!({})))
}

#[test]
fn the_authorization_reset_lists_every_discarded_grant_in_its_receipt() {
    let fx = fixture();
    let (_a, grant_a) = fx.remote_operator(&[RESTART_SCOPE], SITE, WildcardPolicy::Reject);
    let (_b, grant_b) = fx.remote_operator(
        &["ryeos.execute.service.remote/run"],
        "site:origin-b",
        WildcardPolicy::Reject,
    );
    let request = reset_request(&fx);
    let digest = request.digest().unwrap();
    fx.drop_request(&request);
    let report = fx.run();
    assert_eq!(report.applied.len(), 1, "{:?}", report.stopped);
    assert_eq!(
        report.stopped,
        Some(StopReason::AuthorityBearingApplied(
            "node.reset.authorization".to_owned()
        ))
    );
    let state = fx.ledger();
    let receipt = Ledger::new(&fx.store, &fx.node)
        .find_receipt(&state, &digest)
        .unwrap()
        .unwrap();
    let mut listed = receipt.body.discarded_grant_digests.clone();
    listed.sort();
    let mut want = vec![grant_a, grant_b];
    want.sort();
    assert_eq!(listed, want);
    // Only the local operator's grant remains.
    let directory = authorized_keys_dir(fx.path());
    let remaining: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(remaining, [format!("{}.toml", fingerprint(&fx.operator))]);
    let kept = ryeos_node_runtime::identity::load_verified_authorized_key(
        &fingerprint(&fx.operator),
        &directory,
        &fx.node,
    )
    .unwrap()
    .unwrap();
    assert_eq!(kept.scopes, ["*"]);
    assert!(
        !directory
            .parent()
            .unwrap()
            .join(".authorized_keys.offline-reset")
            .exists()
    );
}

#[test]
fn the_receipt_is_in_the_ledger_before_the_grants_are_swapped() {
    let fx = fixture();
    let (_a, grant) = fx.remote_operator(&[RESTART_SCOPE], SITE, WildcardPolicy::Reject);
    let request = reset_request(&fx);
    let digest = request.digest().unwrap();
    let name = fx.drop_request(&request);
    // The node died after the receipt and before the swap.
    let ledger = Ledger::new(&fx.store, &fx.node);
    let state = ledger.load().unwrap();
    ledger
        .append(
            &state,
            ReceiptDraft {
                request: request.clone(),
                measured_preconditions: BTreeMap::new(),
                result: json!({ "effect": "follows this receipt" }),
                discarded_grant_digests: vec![grant],
            },
        )
        .unwrap();
    fx.store
        .save_pending(&Pending {
            request_digest: digest.clone(),
            inbox_name: name,
            request,
            phase: Phase::Receipted,
            measured_preconditions: BTreeMap::new(),
            result: None,
            discarded_grant_digests: Vec::new(),
            started_at: "2026-10-28T00:00:00Z".to_owned(),
        })
        .unwrap();
    let report = fx.run();
    assert_eq!(report.applied.len(), 1, "{:?}", report.stopped);
    assert_eq!(
        fx.ledger().last_sequence,
        1,
        "the swap is completed under the receipt already written"
    );
    let remaining = std::fs::read_dir(authorized_keys_dir(fx.path()))
        .unwrap()
        .count();
    assert_eq!(remaining, 1);
}

#[test]
fn an_alone_operation_waits_behind_anything_applied_before_it() {
    let fx = fixture();
    let request = reset_request(&fx);
    let digest = request.digest().unwrap();
    let candidate = Candidate {
        name: digest.clone(),
        bytes: request.to_bytes().unwrap(),
        digest,
        request,
    };
    let lock = StateLock::acquire(&default_lock_path(fx.path())).unwrap();
    let applier = Applier {
        app_root: fx.path(),
        host: &BareHost,
        node: &fx.node,
        store: &fx.store,
        lock: &lock,
        clock: lillux::time::unix_secs_now,
        bumped: RefCell::new(BTreeMap::new()),
        notes: RefCell::new(Vec::new()),
    };
    let state = applier.ledger().load().unwrap();
    let attempt = applier
        .attempt(&state, &candidate, &InputSource::Inbox, None, 1)
        .unwrap();
    assert!(matches!(attempt, Attempt::Deferred));
    assert!(
        fx.store.pending_digests().unwrap().is_empty(),
        "a deferred request leaves no trace"
    );
}

// ── Refusal receipts ─────────────────────────────────────────────────────

#[test]
fn the_same_bad_request_dropped_again_raises_a_count() {
    let fx = fixture();
    let mut request = fx.sign(fx.restart_body());
    request.body.arguments = json!({ "tampered": true });
    for _ in 0..3 {
        fx.drop_request(&request);
        fx.run();
    }
    let refusals = fx.refusals();
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].body.occurrences, 3);
    refusals[0].verify(fx.node.verifying_key()).unwrap();
}

#[test]
fn a_refused_request_does_not_move_the_ledger_head() {
    let fx = fixture();
    let before = fx.ledger().head;
    let mut request = fx.sign(fx.restart_body());
    request.body.arguments = json!({ "tampered": true });
    fx.drop_request(&request);
    fx.run();
    assert_eq!(
        fx.ledger().head,
        before,
        "junk in the inbox cannot make real requests stale"
    );
}

// ── The ledger is not reachable from handlers or resets ──────────────────

#[test]
fn the_handlers_cannot_name_the_ledger_or_the_offline_state() {
    let handlers = include_str!("../handlers.rs");
    for forbidden in [
        "Ledger",
        "ledger",
        "offline-operations",
        "super::store",
        "ReceiptDraft",
    ] {
        assert!(
            !handlers.contains(forbidden),
            "handlers.rs mentions `{forbidden}`"
        );
    }
}

#[test]
fn no_reset_core_names_the_offline_operation_directory() {
    for (name, source) in [
        (
            "execution_history_reset.rs",
            include_str!("../../../../ryeos-app/src/execution_history_reset.rs"),
        ),
        (
            "scratch.rs",
            include_str!("../../../../ryeos-app/src/execution_history_reset/scratch.rs"),
        ),
        (
            "offline_native.rs",
            include_str!("../../../../ryeos-app/src/offline_native.rs"),
        ),
    ] {
        assert!(
            !source.contains("offline-operations"),
            "{name} reaches the offline-operation directory"
        );
    }
}

// ── The local operator's path ────────────────────────────────────────────

fn restart_draft() -> LocalDraft {
    LocalDraft {
        operation: "node.allow-restart".to_owned(),
        arguments: json!({}),
        inputs: BTreeMap::new(),
        expected_result: BTreeMap::new(),
    }
}

#[test]
fn the_local_path_signs_applies_and_chains_receipts() {
    let fx = fixture();
    let first = apply_local(fx.path(), &BareHost, restart_draft()).unwrap();
    let second = apply_local(fx.path(), &BareHost, restart_draft()).unwrap();
    assert_eq!(first.receipt.body.sequence, 1);
    assert_eq!(second.receipt.body.sequence, 2);
    assert_eq!(
        second.receipt.body.previous,
        first.receipt.digest().unwrap()
    );
    let signer = fingerprint(&fx.operator);
    assert_eq!(second.receipt.body.request.body.signer_fingerprint, signer);
    assert!(fx.store.pending_digests().unwrap().is_empty());
}

#[test]
fn the_local_path_refuses_while_the_node_runs() {
    let fx = fixture();
    let _held = StateLock::acquire(&default_lock_path(fx.path())).unwrap();
    let error = apply_local(fx.path(), &BareHost, restart_draft()).unwrap_err();
    assert!(format!("{error:#}").contains("stopped node"), "{error:#}");
}

#[test]
fn the_local_path_refuses_an_unknown_operation() {
    let fx = fixture();
    let mut draft = restart_draft();
    draft.operation = "node.nothing".to_owned();
    assert!(apply_local(fx.path(), &BareHost, draft).is_err());
}

#[test]
fn the_local_path_refuses_what_the_envelope_refuses() {
    let fx = fixture();
    let mut draft = restart_draft();
    draft.arguments = json!({ "force": true });
    let error = apply_local(fx.path(), &BareHost, draft).unwrap_err();
    assert!(
        format!("{error:#}").contains("invalid_arguments"),
        "{error:#}"
    );
}

#[test]
fn the_local_path_hands_a_handler_only_verified_inputs() {
    let fx = fixture();
    // A policy input for a node with no policy generation: the handler runs
    // (the input verified) and then fails on the missing generation.
    let mut draft = LocalDraft {
        operation: "node.policy-apply".to_owned(),
        arguments: json!({ "section": "external_content" }),
        inputs: BTreeMap::new(),
        expected_result: BTreeMap::new(),
    };
    draft
        .inputs
        .insert("policy".to_owned(), b"schema: 1".to_vec());
    draft
        .expected_result
        .insert("policy_generation".to_owned(), "e".repeat(64));
    let error = format!(
        "{:#}",
        apply_local(fx.path(), &BareHost, draft).unwrap_err()
    );
    assert!(
        !error.contains("is not in the inbox") && !error.contains("does not hash"),
        "{error}"
    );
}

// A small helper so unused-import checks stay honest about `Value`.
#[test]
fn log_lines_name_what_was_refused() {
    let fx = fixture();
    fx.drop_bytes(b"{}");
    let report = fx.run();
    let lines = report.log_lines().join("\n");
    assert!(lines.contains("refused request"), "{lines}");
    let _: Value = json!(null);
}

// ── Nothing in the inbox or the intent records stops a startup ──────────

fn pending_for(fx: &Fixture, request: SignedRequest, phase: Phase) -> Pending {
    Pending {
        request_digest: request.digest().unwrap(),
        inbox_name: fx.drop_request(&request),
        request,
        phase,
        measured_preconditions: BTreeMap::new(),
        result: Some(json!({ "done": true })),
        discarded_grant_digests: Vec::new(),
        started_at: "2026-10-28T00:00:00Z".to_owned(),
    }
}

#[test]
fn an_inbox_too_full_to_list_whole_does_not_fail_the_startup() {
    let fx = fixture();
    for index in 0..4200 {
        std::fs::write(fx.store.inbox_dir().join(format!("junk-{index}")), b"x").unwrap();
    }
    let report = apply_pending_offline_operations(fx.path(), &BareHost).unwrap();
    assert!(report.skipped.is_none() && report.refused.is_empty());
}

#[test]
fn an_unparseable_intent_record_is_set_aside_with_a_refusal() {
    let fx = fixture();
    let digest = "a".repeat(64);
    std::fs::write(
        fx.store.pending_dir().join(format!("{digest}.json")),
        b"{ not json",
    )
    .unwrap();
    let report = fx.run();
    assert_eq!(report.refused.len(), 1);
    assert_eq!(fx.only_refusal(), RefusalReason::Malformed);
    assert!(fx.store.pending_digests().unwrap().is_empty());
}

#[test]
fn an_oversized_intent_record_is_set_aside_with_a_refusal() {
    let fx = fixture();
    let digest = "b".repeat(64);
    std::fs::write(
        fx.store.pending_dir().join(format!("{digest}.json")),
        vec![
            b' ';
            usize::try_from(crate::offline_operation::store::MAX_PENDING_BYTES).unwrap() + 1
        ],
    )
    .unwrap();
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::Malformed);
    assert!(fx.store.pending_digests().unwrap().is_empty());
}

#[test]
fn an_intent_record_for_an_operation_this_build_lacks_is_refused() {
    let fx = fixture();
    let request = fx.sign(fx.body("node.does-not-exist", json!({})));
    let pending = pending_for(&fx, request, Phase::Running);
    fx.store.save_pending(&pending).unwrap();
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::UnknownOperation);
    assert!(fx.store.pending_digests().unwrap().is_empty());
    assert!(fx.inbox_names().is_empty());
}

// ── Continuations are checked again ──────────────────────────────────────

#[test]
fn a_continuation_whose_signature_does_not_verify_is_refused() {
    let fx = fixture();
    let mut request = fx.sign(fx.restart_body());
    request.body.arguments = json!({ "tampered": true });
    let pending = pending_for(&fx, request, Phase::Running);
    fx.store.save_pending(&pending).unwrap();
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::BadSignature);
    assert!(fx.refusals()[0].body.detail.contains("may have landed"));
}

#[test]
fn a_continuation_pinned_to_neither_the_current_head_nor_its_receipts_is_refused() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.precondition
        .insert("ledger_head".into(), "9".repeat(64));
    let pending = pending_for(&fx, fx.sign(body), Phase::Running);
    fx.store.save_pending(&pending).unwrap();
    let report = fx.run();
    assert!(report.applied.is_empty());
    assert_eq!(fx.only_refusal(), RefusalReason::StaleLedgerHead);
}

#[test]
fn a_continuation_is_not_checked_against_the_clock() {
    let fx = fixture();
    let mut body = fx.restart_body();
    body.issued_at = stamp(-10800);
    body.not_after = stamp(-7200);
    let pending = pending_for(&fx, fx.sign(body), Phase::Running);
    fx.store.save_pending(&pending).unwrap();
    assert_eq!(fx.run().applied.len(), 1);
}

#[test]
fn the_local_path_finishes_an_interrupted_operation_before_its_own() {
    let fx = fixture();
    let interrupted = fx.sign(fx.restart_body());
    let digest = interrupted.digest().unwrap();
    let pending = pending_for(&fx, interrupted, Phase::Committed);
    fx.store.save_pending(&pending).unwrap();
    let applied = apply_local(fx.path(), &BareHost, restart_draft()).unwrap();
    assert_eq!(applied.receipt.body.sequence, 2);
    assert!(fx.ledger().request_digests.contains(&digest));
    assert!(fx.store.pending_digests().unwrap().is_empty());
}

// ── The receipt-first crash window ───────────────────────────────────────

#[test]
fn a_node_that_died_between_the_receipt_and_the_intent_record_still_makes_the_effect() {
    let fx = fixture();
    fx.remote_operator(&[RESTART_SCOPE], SITE, WildcardPolicy::Reject);
    let request = reset_request(&fx);
    let digest = request.digest().unwrap();
    let pending = pending_for(&fx, request.clone(), Phase::Running);
    // The receipt was appended; the record still says the handler was running.
    let ledger = Ledger::new(&fx.store, &fx.node);
    let state = ledger.load().unwrap();
    ledger
        .append(
            &state,
            ReceiptDraft {
                request,
                measured_preconditions: BTreeMap::new(),
                result: json!({ "effect": "follows this receipt" }),
                discarded_grant_digests: Vec::new(),
            },
        )
        .unwrap();
    fx.store.save_pending(&pending).unwrap();
    let report = fx.run();
    assert_eq!(report.applied.len(), 1, "{:?}", report.stopped);
    assert_eq!(fx.ledger().last_sequence, 1, "no second receipt");
    assert!(fx.ledger().request_digests.contains(&digest));
    let remaining = std::fs::read_dir(authorized_keys_dir(fx.path()))
        .unwrap()
        .count();
    assert_eq!(remaining, 1, "the swap ran");
}

// ── Inputs belong to the requests that pass ──────────────────────────────

#[test]
fn an_input_with_no_request_is_cleaned_up() {
    let fx = fixture();
    let digest = fx.drop_input(b"nobody wants this");
    fx.run();
    assert!(!fx.inbox_names().contains(&format!("{digest}.input")));
}

#[test]
fn a_request_that_fails_the_envelope_cannot_delete_anothers_input() {
    let fx = fixture();
    let policy = b"schema: 1";
    let input = fx.drop_input(policy);
    // A waiting request (a head not yet reached) wants the input.
    let mut waiting = policy_apply_body(&fx, policy);
    waiting
        .precondition
        .insert("ledger_head".into(), "9".repeat(64));
    let waiting = fx.sign(waiting);
    fx.drop_request(&waiting);
    // A forged request names the same input.
    let mut forged = fx.sign(policy_apply_body(&fx, policy));
    forged.body.arguments = json!({ "section": "ingest_ignore" });
    fx.drop_request(&forged);
    let report = fx.run();
    assert_eq!(report.refused.len(), 1);
    assert!(fx.inbox_names().contains(&format!("{input}.input")));
    assert_eq!(report.waiting, 1);
}

#[test]
fn a_refused_request_leaves_an_input_another_still_names() {
    let fx = fixture();
    let policy = b"schema: 1";
    let input = fx.drop_input(policy);
    let mut waiting = policy_apply_body(&fx, policy);
    waiting
        .precondition
        .insert("ledger_head".into(), "9".repeat(64));
    fx.drop_request(&fx.sign(waiting));
    // A second, validly signed request with the same input fails its
    // preconditions: its refusal must leave the shared input in place.
    let mut stale = policy_apply_body(&fx, policy);
    stale
        .precondition
        .insert(FACT_POLICY_GENERATION.into(), "7".repeat(64));
    stale.not_after = stamp(7200);
    fx.drop_request(&fx.sign(stale));
    fx.run();
    assert_eq!(fx.only_refusal(), RefusalReason::PreconditionFailed);
    assert!(fx.inbox_names().contains(&format!("{input}.input")));
}

// ── A failed read is not a refusal ───────────────────────────────────────

#[test]
fn an_unreadable_grant_leaves_a_signed_request_waiting_until_it_is_stuck() {
    let fx = fixture();
    let key = lillux::crypto::generate_signing_key();
    let fp = fingerprint(&key);
    let directory = authorized_keys_dir(fx.path());
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("{fp}.toml")), b"not a grant").unwrap();
    let mut body = fx.restart_body();
    body.signer_fingerprint = fp;
    body.signer_grant_digest = Some("d".repeat(64));
    body.signer_origin_site_id = Some(SITE.to_owned());
    let request = SignedRequest::sign(body, &key).unwrap();
    fx.drop_request(&request);
    for startup in 1..=3 {
        let report = fx.run();
        assert!(report.refused.is_empty(), "startup {startup}");
        assert_eq!(report.waiting, 1, "startup {startup}");
        assert!(fx.refusals().is_empty());
    }
    let report = fx.run();
    assert_eq!(report.refused.len(), 1);
    assert_eq!(fx.only_refusal(), RefusalReason::Stuck);
    assert!(
        fx.refusals()[0].body.detail.contains("never began"),
        "{}",
        fx.refusals()[0].body.detail
    );
}
