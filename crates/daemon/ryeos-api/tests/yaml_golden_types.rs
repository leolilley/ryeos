//! The exact YAML bytes the node signs for its typed documents.
//!
//! `ryeos-bundle`'s `yaml_golden_bytes` pins `serde_json::Value` emission and
//! scalar readings. Most signing emitters serialize typed structs, whose
//! field order, `Option`, newtype and enum forms are the library's own. These
//! cases emit the production types from fixed values, pin the bytes, and
//! read the pinned bytes back into the type.
//!
//! The production types in signed documents use unit enums (written as plain
//! strings); none carries data in an externally tagged enum. The last case
//! pins that form anyway, so a library change to it is seen.
//!
//! Every case is checked and every mismatch reported in one run.

use std::path::Path;

use ryeos_api::remote::config::PinSource;
use ryeos_api::remote::descriptor::{AdmissionDescriptor, RemoteDescriptor, RemoteDescriptorNode};
use ryeos_node_runtime::node_policy::sections::host_access::{
    HostAccessContainment, HostAccessGrant, HostAccessOrigin, HostAccessRuntimePin,
    NodeHostAccessPolicy,
};
use serde::{Deserialize, Serialize};

fn hex(c: char) -> String {
    c.to_string().repeat(64)
}

/// Pin `value`'s emission, and that the pinned bytes read back as `value`.
fn check<T>(mismatches: &mut Vec<String>, case: &str, value: &T, pinned: &str)
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
{
    let emitted = serde_yaml::to_string(value).expect("emit");
    if emitted != pinned {
        mismatches.push(format!(
            "{case}\n  pinned:  {pinned:?}\n  emitted: {emitted:?}"
        ));
    }
    match serde_yaml::from_str::<T>(pinned) {
        Ok(read) if &read == value => {}
        Ok(read) => mismatches.push(format!(
            "{case}: the pinned bytes read back differently\n  wrote: {value:?}\n  read:  {read:?}"
        )),
        Err(error) => mismatches.push(format!("{case}: the pinned bytes do not read: {error}")),
    }
}

fn report(mismatches: Vec<String>) {
    assert!(
        mismatches.is_empty(),
        "{} case(s) differ from the pinned bytes:\n\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

/// The registration `bundle install` writes, from the production builder.
#[test]
fn the_bundle_registration_body_is_pinned() {
    use lillux::crypto::EncodePrivateKey as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let key_path = dir.path().join("node.pem");
    let key = lillux::crypto::SigningKey::from_bytes(&[0x11; 32]);
    let pem = key.to_pkcs8_pem(Default::default()).expect("pem");
    lillux::atomic_write_private(&key_path, pem.as_bytes()).expect("write key");

    let signed = ryeos_app::bundle_transaction::prepare_signed_bundle_registration(
        Path::new("/var/lib/node/.ai/bundles/core"),
        &key_path,
    )
    .expect("registration");
    let signed = String::from_utf8(signed).expect("utf-8");
    let body = lillux::signature::strip_signature_lines(&signed);
    assert_eq!(body, "kind: node\npath: /var/lib/node/.ai/bundles/core\n");
}

#[test]
fn typed_signed_documents_are_pinned() {
    let mut mismatches = Vec::new();

    // A remote descriptor: skipped `None` options and an empty list are
    // omitted, a present option is written as its struct.
    let descriptor = RemoteDescriptor {
        version: 2,
        name: "home".into(),
        url: None,
        site_id: format!("site:{}", hex('a')),
        vault_fingerprint: hex('b'),
        node: RemoteDescriptorNode {
            public_key: "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            fingerprint: hex('c'),
        },
        ingest_ignore: None,
        capabilities: Vec::new(),
        admission: Some(AdmissionDescriptor {
            mode: "open".into(),
        }),
        provider: None,
    };
    let descriptor_bytes = format!(
        "version: 2\n\
         name: home\n\
         site_id: site:{a}\n\
         vault_fingerprint: {b}\n\
         node:\n  \
         public_key: ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n  \
         fingerprint: {c}\n\
         admission:\n  \
         mode: open\n",
        a = hex('a'),
        b = hex('b'),
        c = hex('c'),
    );
    check(
        &mut mismatches,
        "remote descriptor",
        &descriptor,
        &descriptor_bytes,
    );

    // The node-signed `host_access` policy section: a `None` option that is
    // not skipped is written as null, unit enums as plain strings, an empty
    // list as [], and a renamed field under its YAML name.
    let policy = NodeHostAccessPolicy {
        schema: 1,
        grants: vec![
            HostAccessGrant {
                id: "tool-a".into(),
                item: "tool:demo/run".into(),
                signer: hex('d'),
                source_closure: None,
                runtime: HostAccessRuntimePin {
                    runtime_ref: "runtime:python/function".into(),
                    content_hash: hex('e'),
                },
                origins: vec![HostAccessOrigin::Direct, HostAccessOrigin::GraphChild],
                host_env: Vec::new(),
                containment: HostAccessContainment::ScopeRequired,
            },
            HostAccessGrant {
                id: "tool-b".into(),
                item: "tool:demo/other".into(),
                signer: hex('d'),
                source_closure: Some(hex('f')),
                runtime: HostAccessRuntimePin {
                    runtime_ref: "runtime:python/function".into(),
                    content_hash: hex('e'),
                },
                origins: vec![HostAccessOrigin::Schedule],
                host_env: vec!["HOME".into()],
                containment: HostAccessContainment::GroupAllowed,
            },
        ],
    };
    let policy_bytes = format!(
        "schema: 1\n\
         grants:\n\
         - id: tool-a\n  \
         item: tool:demo/run\n  \
         signer: {d}\n  \
         source_closure: null\n  \
         runtime:\n    \
         ref: runtime:python/function\n    \
         content_hash: {e}\n  \
         origins:\n  \
         - direct\n  \
         - graph_child\n  \
         host_env: []\n  \
         containment: scope_required\n\
         - id: tool-b\n  \
         item: tool:demo/other\n  \
         signer: {d}\n  \
         source_closure: {f}\n  \
         runtime:\n    \
         ref: runtime:python/function\n    \
         content_hash: {e}\n  \
         origins:\n  \
         - schedule\n  \
         host_env:\n  \
         - HOME\n  \
         containment: group_allowed\n",
        d = hex('d'),
        e = hex('e'),
        f = hex('f'),
    );
    check(
        &mut mismatches,
        "host_access policy section",
        &policy,
        &policy_bytes,
    );

    // A remote pin's source, a unit enum as the pin record writes it.
    check(
        &mut mismatches,
        "pin source",
        &PinSource::DescriptorAndExpectedFingerprint,
        "descriptor_and_expected_fingerprint\n",
    );

    report(mismatches);
}

/// Forms no signed production type uses today, pinned so that a library
/// change to them is seen before a type starts to use them.
#[test]
fn enum_and_newtype_forms_are_pinned() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Newtype(String);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Tagged {
        Unit,
        Newtype(String),
        Tuple(u8, u8),
        Struct { a: u8 },
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Holder {
        newtype: Newtype,
        unit: Tagged,
        one: Tagged,
        tuple: Tagged,
        record: Tagged,
        absent: Option<u8>,
    }

    let mut mismatches = Vec::new();
    check(
        &mut mismatches,
        "externally tagged enum forms, a newtype struct and a None option",
        &Holder {
            newtype: Newtype("inner".into()),
            unit: Tagged::Unit,
            one: Tagged::Newtype("x".into()),
            tuple: Tagged::Tuple(1, 2),
            record: Tagged::Struct { a: 3 },
            absent: None,
        },
        "newtype: inner\n\
         unit: Unit\n\
         one: !Newtype x\n\
         tuple: !Tuple\n- 1\n- 2\n\
         record: !Struct\n  a: 3\n\
         absent: null\n",
    );
    report(mismatches);
}
