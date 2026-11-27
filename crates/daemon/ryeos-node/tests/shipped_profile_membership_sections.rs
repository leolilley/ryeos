//! Every shipped init profile carries the `membership` and `client_address`
//! policy sections the node accepts, with no member class and no trusted
//! proxy. A profile that loses a section or a field fails here rather than at
//! a node's first start.

use std::path::PathBuf;

use ryeos_node_runtime::node_policy::sections::client_address::{
    ClientAddressHeader, ClientAddressPolicy,
};
use ryeos_node_runtime::node_policy::sections::membership::MembershipPolicy;

fn profiles_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../bundles/.ai/node/init/profiles")
}

#[test]
fn every_shipped_profile_carries_empty_membership_and_client_address_sections() {
    let mut checked = 0usize;
    for entry in std::fs::read_dir(profiles_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let profile: serde_yaml::Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));

        let section = profile["policies"]["membership"].clone();
        assert!(
            !section.is_null(),
            "{} has no membership policy section",
            path.display()
        );
        let membership: MembershipPolicy = serde_yaml::from_value(section)
            .unwrap_or_else(|error| panic!("{}: membership policy: {error}", path.display()));
        membership
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        assert!(
            membership.classes.is_empty(),
            "{} ships a membership class",
            path.display()
        );

        let section = profile["policies"]["client_address"].clone();
        assert!(
            !section.is_null(),
            "{} has no client_address policy section",
            path.display()
        );
        let address: ClientAddressPolicy = serde_yaml::from_value(section)
            .unwrap_or_else(|error| panic!("{}: client_address policy: {error}", path.display()));
        address
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        assert!(
            address.trusted_proxies.is_empty(),
            "{} ships a trusted proxy",
            path.display()
        );
        assert_eq!(address.header, ClientAddressHeader::Forwarded);
        checked += 1;
    }
    assert!(
        checked >= 15,
        "expected every shipped profile, saw {checked}"
    );
}
