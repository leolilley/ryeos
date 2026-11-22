//! `node.locality` — this node's own graded facts about what it shares with
//! other nodes: its boot, its backing storage and its account.
//!
//! Evidence only. The response holds a digest or an "unknown" with a reason
//! key for each relation, and nothing else: never a boot id, a path, a mount,
//! a device or a uid. Two nodes compare digests and learn only whether they
//! match.
//!
//! What the three relations mean:
//!
//! - `boot` is the kernel's per-boot id. It names one run of one kernel, so it
//!   is not a machine id: the same machine reports a new digest after a reboot.
//! - `storage` is one filesystem instance: the device number of the superblock
//!   that holds the app root. Equal means the same live filesystem. Different
//!   does not mean different disks (two overlay mounts over one disk, or two
//!   btrfs subvolumes, differ), so "not shared" never implies separate hardware.
//! - `account` is the effective uid within one boot.
//!
//! The response also names the node (`site_id`) and when it measured (`as_of`),
//! so a reader can tell whose facts they are and how old.
//!
//! The service is gated by `ryeos.execute.service.node/locality`, so a remote
//! node answers only a caller its own operator granted that capability to.
//!
//! Digests are plain SHA-256 over a domain-separated string with no salt: the
//! values are served only to callers the operator granted the capability, so
//! there is nothing for a salt to protect. `account` and `storage` are scoped
//! to the raw boot id (never served), so one uid or one device number on two
//! machines never matches, and neither value can be recovered from the served
//! digests by trying every small number.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use lillux::process_control::LocalityObservation;
use ryeos_app::state::AppState;
use ryeos_effect_contract::locality::{
    NodeLocalityFacts, REASON_EMULATED_KERNEL, REASON_NOT_MEASURABLE, REASON_USER_NAMESPACE,
    RELATION_ACCOUNT, RELATION_BOOT, RELATION_STORAGE, RelationFact,
};
use ryeos_executor::executor::ServiceAvailability;
use serde_json::Value;

use crate::registry::ServiceDescriptor;

/// The capability a caller needs, here and on every remote.
pub const CAPABILITY: &str = "ryeos.execute.service.node/locality";

const DOMAIN_BOOT: &str = "ryeos.locality.boot.v1";
const DOMAIN_ACCOUNT: &str = "ryeos.locality.account.v1";
const DOMAIN_STORAGE: &str = "ryeos.locality.storage.v1";

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

fn digest(domain: &str, parts: &[&str]) -> String {
    let mut text = String::from(domain);
    for part in parts {
        text.push('\0');
        text.push_str(part);
    }
    lillux::sha256_hex(text.as_bytes())
}

/// The facts a node serves, from what its host measured. Pure.
pub fn facts_from_observation(
    observed: &LocalityObservation,
    site_id: &str,
    as_of: &str,
) -> NodeLocalityFacts {
    NodeLocalityFacts::new(site_id, as_of, relations_from_observation(observed))
}

fn relations_from_observation(observed: &LocalityObservation) -> BTreeMap<String, RelationFact> {
    let boot: Result<(&str, String), &'static str> = if observed.kernel_kind == "gvisor" {
        // Under gVisor the kernel is emulated: its boot id is the sandbox's
        // own and says nothing about the machine beneath.
        Err(REASON_EMULATED_KERNEL)
    } else {
        match observed.boot_id.as_deref() {
            Some(boot_id) => Ok((boot_id, digest(DOMAIN_BOOT, &[boot_id]))),
            None => Err(REASON_NOT_MEASURABLE),
        }
    };
    let unknown = |reason: &str| RelationFact::Unknown {
        reason: reason.to_owned(),
    };

    let mut relations = BTreeMap::new();
    match &boot {
        // Account and storage are scoped to the raw boot id, which is never
        // served. Scoping them to the served boot digest would let a granted
        // caller recover the uid or device number by trying every small value.
        Ok((boot_id, boot_digest)) => {
            relations.insert(
                RELATION_BOOT.to_owned(),
                RelationFact::Measured {
                    digest: boot_digest.clone(),
                },
            );
            relations.insert(
                RELATION_ACCOUNT.to_owned(),
                match (observed.account_uid, observed.initial_user_namespace) {
                    (Some(uid), _) => RelationFact::Measured {
                        digest: digest(DOMAIN_ACCOUNT, &[boot_id, &uid.to_string()]),
                    },
                    (None, Some(false)) => unknown(REASON_USER_NAMESPACE),
                    (None, _) => unknown(REASON_NOT_MEASURABLE),
                },
            );
            relations.insert(
                RELATION_STORAGE.to_owned(),
                match observed.storage_device {
                    Some(device) => RelationFact::Measured {
                        digest: digest(DOMAIN_STORAGE, &[boot_id, &device.to_string()]),
                    },
                    None => unknown(REASON_NOT_MEASURABLE),
                },
            );
        }
        Err(reason) => {
            // Account and storage mean something only inside one boot, and
            // there is no boot to scope them to.
            for relation in [RELATION_BOOT, RELATION_ACCOUNT, RELATION_STORAGE] {
                relations.insert(relation.to_owned(), unknown(reason));
            }
        }
    }
    relations
}

/// This node's facts, observed now.
pub fn local_facts(app_root: &std::path::Path, site_id: &str) -> NodeLocalityFacts {
    facts_from_observation(
        &LocalityObservation::observe(app_root),
        site_id,
        &ryeos_effect_contract::rfc3339::now_millis(),
    )
}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    let app_root = state.config.app_root.clone();
    let site_id = state.threads.site_id().to_string();
    let facts = tokio::task::spawn_blocking(move || local_facts(&app_root, &site_id)).await?;
    Ok(serde_json::to_value(facts)?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:node/locality",
    endpoint: "node.locality",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[CAPABILITY],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_effect_contract::locality::{LOCALITY_FACTS_SCHEMA, RelationFact};

    const SITE_ID: &str = "site:test-node";
    const AS_OF: &str = "2026-11-19T00:00:00.000Z";

    /// The facts as the tests read them: one fixed node and time.
    fn facts_from_observation(observed: &LocalityObservation) -> NodeLocalityFacts {
        super::facts_from_observation(observed, SITE_ID, AS_OF)
    }

    const BOOT_ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";
    const OTHER_BOOT_ID: &str = "ffffffff-4b5a-6978-8796-a5b4c3d2e1f0";

    fn native() -> LocalityObservation {
        LocalityObservation {
            kernel_kind: "native".to_owned(),
            boot_id: Some(BOOT_ID.to_owned()),
            account_uid: Some(31337),
            initial_user_namespace: Some(true),
            storage_device: Some(4_242_424),
        }
    }

    fn digest_of(facts: &NodeLocalityFacts, relation: &str) -> String {
        facts.relations[relation]
            .digest()
            .unwrap_or_else(|| panic!("{relation} is not measured: {:?}", facts.relations))
            .to_owned()
    }

    #[test]
    fn a_native_host_measures_all_three_relations() {
        let facts = facts_from_observation(&native());
        facts.validate().unwrap();
        assert_eq!(facts.schema, LOCALITY_FACTS_SCHEMA);
        for relation in [RELATION_BOOT, RELATION_ACCOUNT, RELATION_STORAGE] {
            assert!(
                matches!(facts.relations[relation], RelationFact::Measured { .. }),
                "{relation}"
            );
        }
        assert_eq!(
            digest_of(&facts, RELATION_BOOT),
            lillux::sha256_hex(format!("ryeos.locality.boot.v1\0{BOOT_ID}").as_bytes())
        );
    }

    #[test]
    fn one_node_gives_one_answer_every_time() {
        assert_eq!(
            facts_from_observation(&native()),
            facts_from_observation(&native())
        );
    }

    #[test]
    fn account_and_storage_are_scoped_to_the_boot() {
        let here = facts_from_observation(&native());
        let elsewhere = facts_from_observation(&LocalityObservation {
            boot_id: Some(OTHER_BOOT_ID.to_owned()),
            ..native()
        });
        // The same uid and the same device number on another boot never match.
        assert_ne!(
            digest_of(&here, RELATION_BOOT),
            digest_of(&elsewhere, RELATION_BOOT)
        );
        assert_ne!(
            digest_of(&here, RELATION_ACCOUNT),
            digest_of(&elsewhere, RELATION_ACCOUNT)
        );
        assert_ne!(
            digest_of(&here, RELATION_STORAGE),
            digest_of(&elsewhere, RELATION_STORAGE)
        );
    }

    #[test]
    fn another_account_or_filesystem_on_one_boot_differs_and_the_boot_matches() {
        let here = facts_from_observation(&native());
        let other = facts_from_observation(&LocalityObservation {
            account_uid: Some(1000),
            storage_device: Some(7),
            ..native()
        });
        assert_eq!(
            digest_of(&here, RELATION_BOOT),
            digest_of(&other, RELATION_BOOT)
        );
        assert_ne!(
            digest_of(&here, RELATION_ACCOUNT),
            digest_of(&other, RELATION_ACCOUNT)
        );
        assert_ne!(
            digest_of(&here, RELATION_STORAGE),
            digest_of(&other, RELATION_STORAGE)
        );
    }

    #[test]
    fn an_emulated_kernel_is_unknown_for_every_relation() {
        let facts = facts_from_observation(&LocalityObservation {
            kernel_kind: "gvisor".to_owned(),
            ..native()
        });
        facts.validate().unwrap();
        for relation in [RELATION_BOOT, RELATION_ACCOUNT, RELATION_STORAGE] {
            assert_eq!(
                facts.relations[relation],
                RelationFact::Unknown {
                    reason: REASON_EMULATED_KERNEL.to_owned()
                },
                "{relation}"
            );
        }
    }

    #[test]
    fn what_cannot_be_measured_is_unknown_with_a_reason() {
        let nested = facts_from_observation(&LocalityObservation {
            account_uid: None,
            initial_user_namespace: Some(false),
            ..native()
        });
        assert_eq!(
            nested.relations[RELATION_ACCOUNT],
            RelationFact::Unknown {
                reason: REASON_USER_NAMESPACE.to_owned()
            }
        );
        assert!(matches!(
            nested.relations[RELATION_BOOT],
            RelationFact::Measured { .. }
        ));

        let blind = facts_from_observation(&LocalityObservation {
            kernel_kind: "native".to_owned(),
            boot_id: None,
            account_uid: None,
            initial_user_namespace: None,
            storage_device: None,
        });
        for relation in [RELATION_BOOT, RELATION_ACCOUNT, RELATION_STORAGE] {
            assert_eq!(
                blind.relations[relation],
                RelationFact::Unknown {
                    reason: REASON_NOT_MEASURABLE.to_owned()
                },
                "{relation}"
            );
        }
        let no_device = facts_from_observation(&LocalityObservation {
            storage_device: None,
            ..native()
        });
        assert_eq!(
            no_device.relations[RELATION_STORAGE],
            RelationFact::Unknown {
                reason: REASON_NOT_MEASURABLE.to_owned()
            }
        );
    }

    /// The response never carries a boot id, a path, a mount point, a device
    /// name or a uid: only digests, grades and reason keys.
    #[test]
    fn the_response_never_carries_a_raw_fact() {
        for observed in [
            native(),
            LocalityObservation {
                kernel_kind: "gvisor".to_owned(),
                ..native()
            },
            LocalityObservation {
                account_uid: None,
                initial_user_namespace: Some(false),
                ..native()
            },
        ] {
            let facts = facts_from_observation(&observed);
            let value = serde_json::to_value(&facts).unwrap();
            let text = serde_json::to_string(&value).unwrap();

            for forbidden in [
                BOOT_ID, "31337", "4242424", "boot_id", "uid", "mount", "device",
            ] {
                assert!(
                    !text.contains(forbidden),
                    "the response carries `{forbidden}`: {text}"
                );
            }
            // Exactly the contract's shape, and nothing beside it.
            let object = value.as_object().unwrap();
            assert_eq!(
                object.keys().map(String::as_str).collect::<Vec<_>>(),
                ["as_of", "relations", "schema", "site_id"]
            );
            assert_eq!(object["site_id"], SITE_ID);
            assert_eq!(object["as_of"], AS_OF);
            for (relation, fact) in object["relations"].as_object().unwrap() {
                let keys: Vec<_> = fact
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                match fact["grade"].as_str().unwrap() {
                    "measured" => assert_eq!(keys, ["digest", "grade"], "{relation}"),
                    "unknown" => assert_eq!(keys, ["grade", "reason"], "{relation}"),
                    other => panic!("unexpected grade {other}"),
                }
            }
        }
    }

    /// The real observation of a real app root: its path and its mount point
    /// never appear in what is served.
    #[test]
    fn a_real_observation_never_carries_the_app_root_or_its_mount_point() {
        let root = tempfile::tempdir().unwrap();
        let text = serde_json::to_string(&local_facts(root.path(), SITE_ID)).unwrap();
        let path = root.path().display().to_string();
        assert!(!text.contains(&path), "{text}");
        let storage = lillux::process_control::RootStorageFacts::observe(root.path());
        for forbidden in [storage.mount_point, storage.mount_source]
            .into_iter()
            .flatten()
            .filter(|value| value.len() > 1)
        {
            assert!(!text.contains(&forbidden), "{forbidden} in {text}");
        }
    }

    #[test]
    fn the_signed_item_and_the_descriptor_name_one_capability() {
        assert_eq!(DESCRIPTOR.required_caps, &[CAPABILITY]);
        assert_eq!(DESCRIPTOR.service_ref, "service:node/locality");
    }
}
