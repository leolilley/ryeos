//! Every shipped init profile carries a publication policy section the node
//! accepts: it parses through the policy's own strict type (unknown or missing
//! fields refused) and validates. A profile that loses a field fails here
//! rather than at a node's first start.

use std::path::PathBuf;

use ryeos_node_runtime::node_policy::sections::publication::PublicationPolicy;

fn profiles_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../bundles/.ai/node/init/profiles")
}

#[test]
fn every_shipped_profile_carries_a_publication_policy_the_node_accepts() {
    let mut checked = 0usize;
    for entry in std::fs::read_dir(profiles_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let profile: serde_yaml::Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let section = profile["policies"]["publication"].clone();
        assert!(
            !section.is_null(),
            "{} has no publication policy section",
            path.display()
        );
        let policy: PublicationPolicy = serde_yaml::from_value(section)
            .unwrap_or_else(|error| panic!("{}: publication policy: {error}", path.display()));
        policy
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        assert_eq!(
            policy,
            PublicationPolicy::as_shipped(),
            "{} drifted from the shipped publication policy",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 15,
        "expected every shipped profile, saw {checked}"
    );
}
