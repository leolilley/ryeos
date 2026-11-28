//! Node-owned membership policy: the classes a token-admitted key can belong
//! to, and what each class may do.
//!
//! The section is required in every profile. An empty `classes` map means no
//! class exists, so no token-admitted grant has a class. Every field of a class
//! is stated by the operator: there is no default class, no default quota and
//! no code-owned ceiling. A quota is either a number or an explicit `null`.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "membership";

/// One concrete capability of each launch kind. A ceiling pattern that
/// matches any of them can launch work.
const LAUNCH_CAPABILITY_PROBES: [&str; 3] = [
    "ryeos.execute.directive.x",
    "ryeos.execute.graph.x",
    "ryeos.execute.tool.x",
];

/// The capability namespaces of the launch kinds. A ceiling pattern inside
/// one of them names a launch capability, literal or wildcard.
const LAUNCH_CAPABILITY_NAMESPACES: [&str; 3] = [
    "ryeos.execute.directive.",
    "ryeos.execute.graph.",
    "ryeos.execute.tool.",
];

const MAX_CLASS_ID_LEN: usize = 64;

/// How a key is placed in a class. A closed set: only the admission-token
/// path makes a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmittedThrough {
    AdmissionToken,
}

impl<'de> Deserialize<'de> for AdmittedThrough {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        match name.as_str() {
            "admission_token" => Ok(Self::AdmissionToken),
            "operator_grant" => Err(serde::de::Error::custom(
                "`operator_grant` is never a member class: a key the operator authorizes directly is not a member",
            )),
            other => Err(serde::de::Error::custom(format!(
                "unknown admitted_through `{other}`; the only allowed value is `admission_token`"
            ))),
        }
    }
}

/// A quota key must be present: a number, or an explicit `null`.
fn required_nullable<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipQuotas {
    #[serde(deserialize_with = "required_nullable")]
    pub sessions: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub requests_per_minute: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub max_request_bytes: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub max_response_bytes: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub enrolled_keys: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub storage_bytes: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub live_threads: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub schedules: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub provider_contacts_per_day: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipLifetime {
    pub session_sliding_days: u64,
    pub retention_days_after_last_renewal: u64,
    pub purge_after_retention: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipSelfEnrolment {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipClass {
    pub admitted_through: Vec<AdmittedThrough>,
    pub scope_ceiling: Vec<String>,
    pub surface: String,
    pub execute_allowlist: Vec<String>,
    pub runtime_cap_bundles_reviewed: Vec<String>,
    pub interpreter_products: Vec<String>,
    pub quotas: MembershipQuotas,
    pub lifetime: MembershipLifetime,
    pub directory_listed_by_default: bool,
    pub self_enrolment: MembershipSelfEnrolment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipPolicy {
    pub schema: u32,
    pub classes: BTreeMap<String, MembershipClass>,
}

fn validate_class_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || id.len() > MAX_CLASS_ID_LEN {
        bail!("membership class id `{id}` must be 1 to {MAX_CLASS_ID_LEN} characters");
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("membership class id `{id}` must use only lowercase letters, digits and `-`");
    }
    Ok(())
}

impl MembershipClass {
    fn validate(&self, id: &str) -> anyhow::Result<()> {
        if self.admitted_through.is_empty() {
            bail!("membership class `{id}` must name at least one admitted_through");
        }
        // The section cannot see the node's host class when it compiles, so a
        // ceiling that covers a launch capability is refused on every host
        // class.
        for pattern in &self.scope_ceiling {
            if pattern.is_empty() {
                bail!("membership class `{id}` has an empty scope_ceiling pattern");
            }
            if let Some(namespace) = LAUNCH_CAPABILITY_NAMESPACES
                .iter()
                .find(|namespace| pattern.starts_with(*namespace))
            {
                bail!(
                    "membership class `{id}` scope_ceiling `{pattern}` names a launch capability (under `{namespace}`); a member holds no launch capability"
                );
            }
            if let Some(probe) = LAUNCH_CAPABILITY_PROBES
                .iter()
                .find(|probe| ryeos_runtime::authorizer::cap_matches(pattern, probe))
            {
                bail!(
                    "membership class `{id}` scope_ceiling `{pattern}` covers a launch capability (it matches `{probe}`); a member holds no launch capability"
                );
            }
        }
        if self.surface.is_empty() {
            bail!("membership class `{id}` must name a surface");
        }
        if self.self_enrolment.enabled {
            bail!(
                "membership class `{id}` self_enrolment.enabled is refused until it has its own ruling"
            );
        }
        Ok(())
    }
}

impl MembershipPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("membership policy schema is not current");
        }
        for (id, class) in &self.classes {
            validate_class_id(id)?;
            class.validate(id)?;
        }
        Ok(())
    }
}

impl TypedNodePolicy for MembershipPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct MembershipPolicySection;

impl NodePolicySection for MembershipPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: MembershipPolicy = serde_json::from_value(body.clone())
            .context("failed to parse membership policy record")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUOTA_KEYS: [&str; 9] = [
        "sessions",
        "requests_per_minute",
        "max_request_bytes",
        "max_response_bytes",
        "enrolled_keys",
        "storage_bytes",
        "live_threads",
        "schedules",
        "provider_contacts_per_day",
    ];

    fn ctx() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.into(),
            source_file: format!("/node/policies/{SECTION_NAME}.yaml").into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn class() -> Value {
        serde_json::json!({
            "admitted_through": ["admission_token"],
            "scope_ceiling": ["ryeos.fetch.item.*"],
            "surface": "member-home",
            "execute_allowlist": [],
            "runtime_cap_bundles_reviewed": [],
            "interpreter_products": [],
            "quotas": {
                "sessions": 2,
                "requests_per_minute": 60,
                "max_request_bytes": 1048576,
                "max_response_bytes": 1048576,
                "enrolled_keys": 3,
                "storage_bytes": null,
                "live_threads": 0,
                "schedules": 0,
                "provider_contacts_per_day": 0
            },
            "lifetime": {
                "session_sliding_days": 7,
                "retention_days_after_last_renewal": 90,
                "purge_after_retention": true
            },
            "directory_listed_by_default": false,
            "self_enrolment": {"enabled": false}
        })
    }

    fn body(class: Value) -> Value {
        serde_json::json!({"schema": 1, "classes": {"guest": class}})
    }

    fn refusal(body: &Value) -> String {
        format!(
            "{:#}",
            MembershipPolicySection.parse(&ctx(), body).err().unwrap()
        )
    }

    #[test]
    fn empty_classes_compile() {
        let body = serde_json::json!({"schema": 1, "classes": {}});
        assert!(MembershipPolicySection.parse(&ctx(), &body).is_ok());
    }

    #[test]
    fn a_valid_class_parses() {
        let parsed = MembershipPolicySection
            .parse(&ctx(), &body(class()))
            .unwrap();
        let record = parsed.as_any().downcast_ref::<MembershipPolicy>().unwrap();
        let guest = &record.classes["guest"];
        assert_eq!(guest.admitted_through, [AdmittedThrough::AdmissionToken]);
        assert_eq!(guest.quotas.sessions, Some(2));
        assert_eq!(guest.quotas.storage_bytes, None);
    }

    #[test]
    fn every_missing_quota_key_is_refused() {
        for key in QUOTA_KEYS {
            let mut class = class();
            class["quotas"].as_object_mut().unwrap().remove(key);
            let message = refusal(&body(class));
            assert!(message.contains(key), "{key}: {message}");
        }
    }

    #[test]
    fn missing_lifetime_and_flags_are_refused() {
        for key in [
            "lifetime",
            "directory_listed_by_default",
            "self_enrolment",
            "quotas",
        ] {
            let mut class = class();
            class.as_object_mut().unwrap().remove(key);
            assert!(MembershipPolicySection.parse(&ctx(), &body(class)).is_err());
        }
        for key in [
            "session_sliding_days",
            "retention_days_after_last_renewal",
            "purge_after_retention",
        ] {
            let mut class = class();
            class["lifetime"].as_object_mut().unwrap().remove(key);
            assert!(MembershipPolicySection.parse(&ctx(), &body(class)).is_err());
        }
    }

    #[test]
    fn operator_grant_is_refused_by_name() {
        let mut class = class();
        class["admitted_through"] = serde_json::json!(["admission_token", "operator_grant"]);
        assert!(refusal(&body(class)).contains("operator_grant"));
    }

    #[test]
    fn admitted_through_must_be_non_empty_and_closed() {
        let mut empty = class();
        empty["admitted_through"] = serde_json::json!([]);
        assert!(refusal(&body(empty)).contains("admitted_through"));

        let mut unknown = class();
        unknown["admitted_through"] = serde_json::json!(["self_service"]);
        assert!(refusal(&body(unknown)).contains("self_service"));
    }

    #[test]
    fn a_launch_capability_ceiling_is_refused() {
        for pattern in [
            "ryeos.execute.tool.ryeos/core/fetch",
            "ryeos.execute.graph.ryeos/bundle-release/submit",
            "ryeos.execute.directive.ryeos/core/hello",
            "ryeos.execute.directive.x",
            "ryeos.execute.directive.*",
            "ryeos.execute.graph.*",
            "ryeos.execute.tool.*",
            "ryeos.execute.*",
            "ryeos.*",
            "*",
        ] {
            let mut class = class();
            class["scope_ceiling"] = serde_json::json!(["ryeos.fetch.item.*", pattern]);
            let message = refusal(&body(class));
            assert!(
                message.contains("launch capability"),
                "{pattern}: {message}"
            );
        }
    }

    #[test]
    fn a_non_launch_ceiling_is_accepted() {
        let mut class = class();
        class["scope_ceiling"] =
            serde_json::json!(["ryeos.execute.service.ui/*", "ryeos.fetch.item.*"]);
        assert!(MembershipPolicySection.parse(&ctx(), &body(class)).is_ok());
    }

    #[test]
    fn self_enrolment_enabled_is_refused() {
        let mut class = class();
        class["self_enrolment"]["enabled"] = Value::Bool(true);
        assert!(refusal(&body(class)).contains("self_enrolment"));
    }

    #[test]
    fn unknown_fields_are_refused() {
        let mut class = class();
        class["default"] = Value::Bool(true);
        assert!(refusal(&body(class)).contains("unknown field"));

        let mut quota = self::class();
        quota["quotas"]["bandwidth"] = Value::from(1);
        assert!(refusal(&body(quota)).contains("unknown field"));

        let mut body = body(self::class());
        body["ceiling"] = Value::from(1);
        assert!(refusal(&body).contains("unknown field"));
    }

    #[test]
    fn class_ids_are_validated() {
        let long = "a".repeat(65);
        for bad in ["", "Guest", "a_b", "a.b", long.as_str()] {
            let mut classes = serde_json::Map::new();
            classes.insert(bad.to_string(), class());
            let body = serde_json::json!({"schema": 1, "classes": classes});
            assert!(
                MembershipPolicySection.parse(&ctx(), &body).is_err(),
                "{bad:?}"
            );
        }
        let mut classes = serde_json::Map::new();
        classes.insert("a-1".into(), class());
        classes.insert("a".repeat(64), class());
        let ok = serde_json::json!({"schema": 1, "classes": classes});
        assert!(MembershipPolicySection.parse(&ctx(), &ok).is_ok());
    }

    #[test]
    fn unknown_schema_is_refused() {
        let mut body = body(class());
        body["schema"] = Value::from(2);
        assert!(MembershipPolicySection.parse(&ctx(), &body).is_err());
    }
}
