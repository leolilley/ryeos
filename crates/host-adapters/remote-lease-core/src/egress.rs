//! Guest egress policy shapes a lease create may request.
//!
//! A remote lease's guest may reach only the hosts its admitted request names,
//! over HTTPS, as exact DNS names. Which shapes a provider can create is the
//! signed spec's `NetworkPolicyCapability`; the exact policy for one lease is
//! derived from it here, and refused by name when the spec cannot create it.
//! What a provider's echo of that policy proves stays a provider fact.

use std::collections::BTreeSet;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// Network policy types an adapter may ask its provider to create. Allow-all
/// is deliberately not representable, so no signed spec and no lease request
/// can select it.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkPolicyType {
    DenyAll,
    AllowList,
}

/// The one egress rule protocol an allow-list carries.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EgressProtocol {
    Https,
}

impl EgressProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
        }
    }
}

/// One egress rule, in the exact shape a create request takes and its echo
/// repeats.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EgressRule {
    pub domain: String,
    pub protocol: EgressProtocol,
}

/// The exact network policy one lease create asks for, and the policy every
/// later read of that lease must echo. Workload: a remote lease whose guest
/// dials one authenticated connection to its controller and must reach
/// nothing else.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct EgressPolicy {
    #[serde(rename = "type")]
    pub policy_type: NetworkPolicyType,
    /// Ascending by domain; empty exactly when the type is deny-all.
    pub rules: Vec<EgressRule>,
}

impl EgressPolicy {
    pub fn deny_all() -> Self {
        Self {
            policy_type: NetworkPolicyType::DenyAll,
            rules: Vec::new(),
        }
    }

    pub fn rules_text(&self) -> String {
        self.rules
            .iter()
            .map(|rule| format!("{} {}", rule.protocol.as_str(), rule.domain))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Upper bound on allow-list rules any signed spec may declare and any lease
/// request may carry.
pub const MAX_EGRESS_RULES: u32 = 32;
const MAX_EGRESS_HOST_BYTES: usize = 253;

/// Whether `host` is an allow-list host a provider can match exactly: a
/// lowercase DNS name of at least two labels with no scheme, port, path,
/// trailing dot or numeric top label, optionally preceded by one `*.` wildcard
/// label when `allow_wildcard` holds. Workload: the controller hostname a
/// remote lease's guest dials.
pub fn valid_egress_host(host: &str, allow_wildcard: bool) -> bool {
    let name = match host.strip_prefix("*.") {
        Some(rest) if allow_wildcard => rest,
        Some(_) => return false,
        None => host,
    };
    let labels: Vec<&str> = name.split('.').collect();
    host.len() <= MAX_EGRESS_HOST_BYTES
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
        && labels
            .last()
            .is_some_and(|top| !top.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Which allow-list host spellings a signed spec accepts: exact names, or
/// exact names and a wildcard as the leftmost label.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EgressHostSyntax {
    ExactDnsName,
    ExactDnsNameOrLeftmostWildcard,
}

impl EgressHostSyntax {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactDnsName => "exact_dns_name",
            Self::ExactDnsNameOrLeftmostWildcard => "exact_dns_name_or_leftmost_wildcard",
        }
    }
}

/// Bounds on the allow-list rules a create request may carry.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AllowListCapability {
    pub rule_protocol: EgressProtocol,
    pub max_rules: u32,
    pub host_syntax: EgressHostSyntax,
}

/// Signed declaration of the network policies an adapter may create.
/// Workload: a remote lease whose guest may reach only the controller that
/// serves it. Deny-all is always creatable (every lease with an empty egress
/// list); allow-list is creatable exactly when its rule bounds are declared.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicyCapability {
    pub creatable_types: Vec<NetworkPolicyType>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub allow_list: Option<AllowListCapability>,
}

impl NetworkPolicyCapability {
    /// The declaration is closed: deny-all only without allow-list bounds, or
    /// deny-all and allow-list with bounds of at most `MAX_EGRESS_RULES`.
    pub fn validate(&self) -> Result<()> {
        match (self.creatable_types.as_slice(), &self.allow_list) {
            ([NetworkPolicyType::DenyAll], None) => Ok(()),
            ([NetworkPolicyType::DenyAll, NetworkPolicyType::AllowList], Some(allow_list)) => {
                ensure!(
                    (1..=MAX_EGRESS_RULES).contains(&allow_list.max_rules),
                    "provider spec allow-list max_rules must lie in 1..={MAX_EGRESS_RULES}"
                );
                Ok(())
            }
            _ => bail!(
                "provider spec network policy declaration is unsupported: creatable_types must \
                 be [deny-all] without allow_list bounds, or [deny-all, allow-list] with them"
            ),
        }
    }

    /// The exact policy a lease create requests for `lease_egress`, the
    /// admitted lease's guest egress hosts. An empty list means deny-all. A
    /// non-empty list is an allow-list of rules in the declared protocol,
    /// refused by name (naming the spec as `provider`) unless allow-list
    /// creation is declared and the hosts fit its bounds; it never falls back
    /// to deny-all or allow-all. A declaration outside the closed shapes
    /// `validate` accepts creates nothing, not even deny-all.
    pub fn egress_policy(
        &self,
        provider: &str,
        lease_egress: &BTreeSet<String>,
    ) -> Result<EgressPolicy> {
        self.validate()?;
        if lease_egress.is_empty() {
            return Ok(EgressPolicy::deny_all());
        }
        let requested = lease_egress
            .iter()
            .map(|host| format!("{host:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let Some(allow_list) = &self.allow_list else {
            bail!(
                "provider spec {provider} declares network_policy creatable_types [deny-all] \
                 only, so it cannot create the allow-list [{requested}] the workload requests"
            );
        };
        ensure!(
            u32::try_from(lease_egress.len()).is_ok_and(|count| count <= allow_list.max_rules),
            "provider spec {provider} creates allow-lists of at most {} rules, and the workload \
             requests {} hosts [{requested}]",
            allow_list.max_rules,
            lease_egress.len()
        );
        let wildcard = allow_list.host_syntax == EgressHostSyntax::ExactDnsNameOrLeftmostWildcard;
        for host in lease_egress {
            ensure!(
                valid_egress_host(host, wildcard),
                "provider spec {provider} accepts allow-list hosts only as {}, and the workload \
                 requests {host:?}",
                allow_list.host_syntax.as_str()
            );
        }
        Ok(EgressPolicy {
            policy_type: NetworkPolicyType::AllowList,
            rules: lease_egress
                .iter()
                .map(|host| EgressRule {
                    domain: host.clone(),
                    protocol: allow_list.rule_protocol,
                })
                .collect(),
        })
    }
}

/// A present field must carry a value; an explicit `null` is refused rather
/// than read as absent.
fn deserialize_non_null_option<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROVIDER: &str = "test_provider_v1";

    fn hosts(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn allow_list(max_rules: u32, host_syntax: EgressHostSyntax) -> NetworkPolicyCapability {
        NetworkPolicyCapability {
            creatable_types: vec![NetworkPolicyType::DenyAll, NetworkPolicyType::AllowList],
            allow_list: Some(AllowListCapability {
                rule_protocol: EgressProtocol::Https,
                max_rules,
                host_syntax,
            }),
        }
    }

    fn deny_all_only() -> NetworkPolicyCapability {
        NetworkPolicyCapability {
            creatable_types: vec![NetworkPolicyType::DenyAll],
            allow_list: None,
        }
    }

    fn refusal(capability: &NetworkPolicyCapability, names: &[&str]) -> String {
        format!(
            "{:#}",
            capability
                .egress_policy(PROVIDER, &hosts(names))
                .unwrap_err()
        )
    }

    #[test]
    fn hosts_are_bounded_exact_names_and_wildcards_need_the_declared_syntax() {
        let exact = allow_list(4, EgressHostSyntax::ExactDnsName);
        for malformed in [
            "https://controller.example.com",
            "controller.example.com:443",
            "controller.example.com/path",
            "Controller.example.com",
            "controller.example.com.",
            "controller",
            "10.0.0.1",
            "-controller.example.com",
            "controller_1.example.com",
            "api.*.example.com",
            "*",
            "*.com",
            "",
        ] {
            assert!(
                exact.egress_policy(PROVIDER, &hosts(&[malformed])).is_err(),
                "{malformed:?} must be refused"
            );
        }
        let long_label = format!("{}.example.com", "a".repeat(64));
        assert!(!valid_egress_host(&long_label, false));
        let long_name = format!("{}example.com", "abcdefghi.".repeat(25));
        assert!(long_name.len() > 253 && !valid_egress_host(&long_name, false));

        let reason = refusal(&exact, &["*.example.com"]);
        assert!(
            reason.contains(
                "provider spec test_provider_v1 accepts allow-list hosts only as exact_dns_name"
            ),
            "{reason}"
        );
        let wide = allow_list(4, EgressHostSyntax::ExactDnsNameOrLeftmostWildcard);
        assert!(
            wide.egress_policy(PROVIDER, &hosts(&["*.example.com"]))
                .is_ok()
        );
        assert!(
            wide.egress_policy(PROVIDER, &hosts(&["*.*.example.com"]))
                .is_err()
        );
        assert!(
            wide.egress_policy(PROVIDER, &hosts(&["api.*.example.com"]))
                .is_err()
        );
    }

    #[test]
    fn allow_lists_are_bounded_by_max_rules_and_requested_in_order() {
        let capability = allow_list(2, EgressHostSyntax::ExactDnsName);
        let reason = refusal(
            &capability,
            &["a.example.com", "b.example.com", "c.example.com"],
        );
        assert!(
            reason.contains(
                "provider spec test_provider_v1 creates allow-lists of at most 2 rules, and the \
                 workload requests 3 hosts"
            ),
            "{reason}"
        );
        let policy = capability
            .egress_policy(PROVIDER, &hosts(&["b.example.com", "a.example.com"]))
            .unwrap();
        assert_eq!(policy.policy_type, NetworkPolicyType::AllowList);
        assert_eq!(
            policy.rules_text(),
            "https a.example.com, https b.example.com"
        );
        for out_of_bounds in [0, MAX_EGRESS_RULES + 1] {
            assert!(
                allow_list(out_of_bounds, EgressHostSyntax::ExactDnsName)
                    .validate()
                    .is_err()
            );
        }
    }

    #[test]
    fn empty_egress_is_deny_all_and_a_deny_all_only_spec_refuses_by_name() {
        for capability in [
            deny_all_only(),
            allow_list(4, EgressHostSyntax::ExactDnsName),
        ] {
            assert_eq!(
                capability
                    .egress_policy(PROVIDER, &BTreeSet::new())
                    .unwrap(),
                EgressPolicy::deny_all()
            );
        }
        let reason = refusal(&deny_all_only(), &["controller.example.com"]);
        assert!(
            reason.contains(
                "provider spec test_provider_v1 declares network_policy creatable_types \
                 [deny-all] only, so it cannot create the allow-list \
                 [\"controller.example.com\"]"
            ),
            "{reason}"
        );
    }

    #[test]
    fn an_undeclared_capability_creates_nothing_not_even_deny_all() {
        let undeclared = [
            NetworkPolicyCapability {
                creatable_types: Vec::new(),
                allow_list: None,
            },
            NetworkPolicyCapability {
                creatable_types: vec![NetworkPolicyType::AllowList],
                allow_list: None,
            },
            NetworkPolicyCapability {
                creatable_types: vec![NetworkPolicyType::DenyAll],
                allow_list: allow_list(4, EgressHostSyntax::ExactDnsName).allow_list,
            },
            allow_list(MAX_EGRESS_RULES + 1, EgressHostSyntax::ExactDnsName),
        ];
        for capability in undeclared {
            for names in [&[][..], &["controller.example.com"][..]] {
                let reason = refusal(&capability, names);
                assert!(
                    reason.contains("network policy declaration is unsupported")
                        || reason.contains("max_rules must lie in"),
                    "{reason}"
                );
            }
        }
    }

    #[test]
    fn declaration_refuses_explicit_null_and_unknown_fields() {
        let parse = |text: &str| serde_json::from_str::<NetworkPolicyCapability>(text);
        let declared = parse(
            r#"{"creatable_types":["deny-all","allow-list"],
                "allow_list":{"rule_protocol":"https","max_rules":4,"host_syntax":"exact_dns_name"}}"#,
        )
        .unwrap();
        declared.validate().unwrap();
        let absent = parse(r#"{"creatable_types":["deny-all"]}"#).unwrap();
        assert!(absent.allow_list.is_none());
        absent.validate().unwrap();
        assert!(parse(r#"{"creatable_types":["deny-all"],"allow_list":null}"#).is_err());
        assert!(parse(r#"{"creatable_types":["deny-all"],"other":1}"#).is_err());
        assert!(parse(r#"{"creatable_types":["allow-all"]}"#).is_err());
    }
}
