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

/// Every shipped profile carries the public-routes section and names no
/// route: no shipped node serves a keyless route of the public page. Only the
/// public node's own profile names them, and none ships yet; when it does, it
/// is the one exception here.
#[test]
fn every_shipped_profile_carries_public_routes_and_names_no_route() {
    use ryeos_node_runtime::node_policy::sections::public_routes::PublicRoutesPolicy;

    let mut checked = 0usize;
    for entry in std::fs::read_dir(profiles_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let profile: serde_yaml::Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let section = profile["policies"]["public_routes"].clone();
        assert!(
            !section.is_null(),
            "{} has no public_routes policy section",
            path.display()
        );
        let policy: PublicRoutesPolicy = serde_yaml::from_value(section)
            .unwrap_or_else(|error| panic!("{}: public_routes policy: {error}", path.display()));
        policy
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        assert!(
            policy.routes.is_empty(),
            "{} names public routes; only the public node's profile may",
            path.display()
        );
        assert_eq!(policy, PublicRoutesPolicy::as_shipped());
        checked += 1;
    }
    assert!(
        checked >= 15,
        "expected every shipped profile, saw {checked}"
    );
}

#[test]
fn every_shipped_profile_carries_the_bundle_staging_bounds() {
    use ryeos_node_runtime::node_policy::sections::bundle_staging::BundleStagingPolicy;

    let mut checked = 0usize;
    for entry in std::fs::read_dir(profiles_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let profile: serde_yaml::Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let section = profile["policies"]["bundle_staging"].clone();
        assert!(
            !section.is_null(),
            "{} has no bundle_staging policy section",
            path.display()
        );
        let policy: BundleStagingPolicy = serde_yaml::from_value(section)
            .unwrap_or_else(|error| panic!("{}: bundle_staging policy: {error}", path.display()));
        policy
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(
            (
                policy.max_node_bytes,
                policy.max_node_stages,
                policy.max_key_bytes,
                policy.max_key_stages
            ),
            (2 * 1024 * 1024 * 1024, 64, 512 * 1024 * 1024, 16),
            "{} drifted from the shipped bundle_staging bounds",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 15,
        "expected every shipped profile, saw {checked}"
    );
}
