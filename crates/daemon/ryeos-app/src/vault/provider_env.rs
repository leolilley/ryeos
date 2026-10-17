//! Provider-delivered environment values taken at daemon startup.
//!
//! The node's signed policy declares the names and, per name, who may
//! receive it. Startup takes exactly those names out of the daemon's own
//! environment, before any thread exists, and this snapshot is then the only
//! place their values live. Values leave it only through authorized secret
//! delivery; diagnostics see names only.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};
use ryeos_node_runtime::node_policy::sections::provider_environment::{
    ProviderEnvironmentName, ProviderEnvironmentRecipients,
};
use zeroize::Zeroizing;

/// Declared provider-environment names, their recipient ceilings, and the
/// values present at startup.
pub struct ProviderEnvironment {
    declared: Vec<ProviderEnvironmentName>,
    declared_names: Vec<String>,
    values: BTreeMap<String, Zeroizing<String>>,
}

impl ProviderEnvironment {
    /// A node that declares no provider-delivered names.
    pub const fn none() -> Self {
        Self {
            declared: Vec::new(),
            declared_names: Vec::new(),
            values: BTreeMap::new(),
        }
    }

    /// Bind the values taken at startup to the declaration they were taken
    /// under. Every value must belong to a declared name, and every declared
    /// name must carry a valid recipient ceiling.
    pub fn captured(
        declared: Vec<ProviderEnvironmentName>,
        values: Vec<(String, Zeroizing<String>)>,
    ) -> Result<Self> {
        let mut previous: Option<&str> = None;
        for entry in &declared {
            if previous.is_some_and(|candidate| candidate >= entry.name.as_str()) {
                bail!("provider-environment declaration must be sorted and unique");
            }
            previous = Some(&entry.name);
            entry.recipients.validate().map_err(|error| {
                anyhow::anyhow!(
                    "provider-environment name `{}` recipients: {error:#}",
                    entry.name
                )
            })?;
        }
        let declared_names: Vec<String> =
            declared.iter().map(|entry| entry.name.clone()).collect();
        let mut retained = BTreeMap::new();
        for (name, value) in values {
            if declared_names.binary_search(&name).is_err() {
                bail!("provider-environment value `{name}` is outside the declaration");
            }
            if retained.insert(name.clone(), value).is_some() {
                bail!("provider-environment value `{name}` was taken twice");
            }
        }
        Ok(Self {
            declared,
            declared_names,
            values: retained,
        })
    }

    pub fn declared_names(&self) -> &[String] {
        &self.declared_names
    }

    pub fn declares(&self, name: &str) -> bool {
        self.declared_names
            .binary_search_by(|candidate| candidate.as_str().cmp(name))
            .is_ok()
    }

    /// Declared names that had a value at startup, in sorted order.
    pub fn present_names(&self) -> Vec<String> {
        self.values.keys().cloned().collect()
    }

    /// Declared names that had no value at startup, in sorted order.
    pub fn absent_names(&self) -> Vec<String> {
        self.declared_names
            .iter()
            .filter(|name| !self.values.contains_key(name.as_str()))
            .cloned()
            .collect()
    }

    /// Admission of one requesting principal to the provider source.
    ///
    /// `requested` is the exact name set an execution policy fixes, or
    /// `None` when the policy admits whatever the workload later declares,
    /// in which case every declared name is in play. Requested names the
    /// node does not declare are left to delivery, which reports them
    /// outside the declaration. Every declared name in play must list the
    /// principal or a capability the principal holds exactly; otherwise
    /// admission is refused naming the first such name. The returned names,
    /// sorted, are what the admitted authority seals.
    pub fn admit_recipient(
        &self,
        principal: &str,
        capabilities: &[String],
        requested: Option<&[String]>,
    ) -> Result<Vec<String>> {
        let mut admitted = Vec::new();
        for entry in &self.declared {
            if let Some(requested) = requested
                && !requested.iter().any(|name| name == &entry.name)
            {
                continue;
            }
            if let Some(refusal) = judge_recipient(&entry.recipients, principal, capabilities) {
                bail!("{}", refusal.message(&entry.name, principal));
            }
            admitted.push(entry.name.clone());
        }
        Ok(admitted)
    }

    /// Requested names the live declaration carries whose current recipient
    /// rule does not admit `principal` holding `capabilities`, in request
    /// order, each with the reason it is refused. Names the live declaration
    /// lacks are not judged here; delivery reports them missing.
    pub fn refused_recipient_names(
        &self,
        requested: &[String],
        principal: &str,
        capabilities: &[String],
    ) -> Vec<(String, RecipientRefusal)> {
        requested
            .iter()
            .filter_map(|name| {
                let index = self
                    .declared
                    .binary_search_by(|entry| entry.name.as_str().cmp(name.as_str()))
                    .ok()?;
                judge_recipient(&self.declared[index].recipients, principal, capabilities)
                    .map(|refusal| (name.clone(), refusal))
            })
            .collect()
    }

    /// Copy the values of the requested declared names that are present.
    /// The caller owns zeroizing every returned value.
    pub(super) fn read_declared(&self, requested: &[String]) -> HashMap<String, String> {
        requested
            .iter()
            .filter(|name| self.declares(name))
            .filter_map(|name| {
                self.values
                    .get(name.as_str())
                    .map(|value| (name.clone(), value.as_str().to_owned()))
            })
            .collect()
    }
}

/// Why one declared provider-environment name is not delivered to a
/// requesting principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipientRefusal {
    /// Neither the principal nor any capability it holds, exactly or by a
    /// covering wildcard, is listed for the name.
    NotListed,
    /// The principal holds a wildcard grant that covers the listed
    /// `capability`, but not the capability itself. A recipient capability
    /// names a deliberate grant of one secret; a broad wildcard grant made
    /// for other reasons never carries it.
    WildcardOnly { capability: String },
}

impl RecipientRefusal {
    /// The refusal text for `name` requested by `principal`. Never carries
    /// a value.
    pub fn message(&self, name: &str, principal: &str) -> String {
        match self {
            Self::NotListed => format!(
                "provider-environment name `{name}` is not delivered to principal `{principal}`: \
                 the node policy lists neither that principal nor a capability it holds"
            ),
            Self::WildcardOnly { capability } => format!(
                "provider environment {name} requires the exact recipient capability \
                 {capability}; wildcard grants do not satisfy it (principal `{principal}`)"
            ),
        }
    }
}

/// Judge one name's recipient rule. A listed principal admits; otherwise a
/// listed capability admits only when the principal holds that exact
/// capability string. A wildcard grant covering a listed capability is
/// refused by name rather than reported as unlisted.
fn judge_recipient(
    recipients: &ProviderEnvironmentRecipients,
    principal: &str,
    capabilities: &[String],
) -> Option<RecipientRefusal> {
    if recipients
        .principals
        .iter()
        .any(|candidate| candidate == principal)
    {
        return None;
    }
    if recipients
        .capabilities
        .iter()
        .any(|listed| capabilities.iter().any(|held| held == listed))
    {
        return None;
    }
    recipients
        .capabilities
        .iter()
        .find(|listed| {
            capabilities
                .iter()
                .any(|held| ryeos_runtime::authorizer::cap_matches(held, listed))
        })
        .map(|listed| RecipientRefusal::WildcardOnly {
            capability: listed.clone(),
        })
        .or(Some(RecipientRefusal::NotListed))
}

impl std::fmt::Debug for ProviderEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderEnvironment")
            .field("declared", &self.declared)
            .field("present_names", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Test construction of a declaration whose every name lists `recipients`
/// as recipient principals.
#[cfg(test)]
pub(crate) fn test_declaration(names: &[&str], recipients: &[&str]) -> Vec<ProviderEnvironmentName> {
    names
        .iter()
        .map(|name| ProviderEnvironmentName {
            name: (*name).to_owned(),
            recipients: ProviderEnvironmentRecipients {
                principals: recipients.iter().map(|value| (*value).to_owned()).collect(),
                capabilities: Vec::new(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(declared: &[&str], values: &[(&str, &str)]) -> Result<ProviderEnvironment> {
        ProviderEnvironment::captured(
            test_declaration(declared, &["fp:operator"]),
            values
                .iter()
                .map(|(name, value)| ((*name).to_owned(), Zeroizing::new((*value).to_owned())))
                .collect(),
        )
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn capability_declaration() -> ProviderEnvironment {
        ProviderEnvironment::captured(
            vec![
                ProviderEnvironmentName {
                    name: "A_KEY".to_owned(),
                    recipients: ProviderEnvironmentRecipients {
                        principals: strings(&["fp:alice"]),
                        capabilities: Vec::new(),
                    },
                },
                ProviderEnvironmentName {
                    name: "B_KEY".to_owned(),
                    recipients: ProviderEnvironmentRecipients {
                        principals: Vec::new(),
                        capabilities: strings(&["ryeos.receive.provider_env.b_key"]),
                    },
                },
            ],
            vec![
                ("A_KEY".to_owned(), Zeroizing::new("value-a".to_owned())),
                ("B_KEY".to_owned(), Zeroizing::new("value-b".to_owned())),
            ],
        )
        .unwrap()
    }

    #[test]
    fn values_are_bound_to_the_declaration_and_never_debug_printed() {
        let environment = captured(&["A_KEY", "B_KEY"], &[("A_KEY", "value-a")]).unwrap();
        assert_eq!(environment.present_names(), ["A_KEY"]);
        assert_eq!(environment.absent_names(), ["B_KEY"]);
        assert!(!format!("{environment:?}").contains("value-a"));
        assert!(captured(&["B_KEY", "A_KEY"], &[]).is_err());
        assert!(captured(&["A_KEY"], &[("OTHER", "value")]).is_err());
        assert!(captured(&["A_KEY"], &[("A_KEY", "1"), ("A_KEY", "2")]).is_err());
    }

    #[test]
    fn a_declared_name_without_recipients_is_refused() {
        let error = ProviderEnvironment::captured(test_declaration(&["A_KEY"], &[]), Vec::new())
            .unwrap_err();
        assert!(format!("{error:#}").contains("A_KEY"), "{error:#}");
    }

    #[test]
    fn reads_return_only_requested_declared_present_values() {
        let environment = captured(&["A_KEY", "B_KEY"], &[("A_KEY", "value-a")]).unwrap();
        let read = environment.read_declared(&["A_KEY".to_owned(), "C_KEY".to_owned()]);
        assert_eq!(read.len(), 1);
        assert_eq!(read.get("A_KEY").map(String::as_str), Some("value-a"));
        assert!(ProviderEnvironment::none().read_declared(&["A_KEY".to_owned()]).is_empty());
    }

    #[test]
    fn admission_names_the_refused_name_and_principal_never_the_value() {
        let environment = capability_declaration();
        // Listed principal, exact request.
        assert_eq!(
            environment
                .admit_recipient("fp:alice", &[], Some(&strings(&["A_KEY"])))
                .unwrap(),
            ["A_KEY"]
        );
        // Any other principal is refused for that name.
        let error = environment
            .admit_recipient("fp:mallory", &[], Some(&strings(&["A_KEY"])))
            .unwrap_err()
            .to_string();
        assert!(error.contains("`A_KEY`") && error.contains("`fp:mallory`"), "{error}");
        assert!(!error.contains("value-a") && !error.contains("value-b"), "{error}");
        // A capability recipient admits holders of the exact capability and
        // nobody else.
        assert_eq!(
            environment
                .admit_recipient(
                    "fp:bob",
                    &strings(&["ryeos.receive.provider_env.b_key"]),
                    Some(&strings(&["B_KEY"]))
                )
                .unwrap(),
            ["B_KEY"]
        );
        let error = environment
            .admit_recipient(
                "fp:bob",
                &strings(&["ryeos.execute.tool.*"]),
                Some(&strings(&["B_KEY"])),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("`B_KEY`") && error.contains("`fp:bob`"), "{error}");
        // A principal listed for one name is not a recipient of another.
        let error = environment
            .admit_recipient("fp:alice", &[], Some(&strings(&["A_KEY", "B_KEY"])))
            .unwrap_err()
            .to_string();
        assert!(error.contains("`B_KEY`") && error.contains("`fp:alice`"), "{error}");
        // An open declaration puts every declared name in play.
        assert!(environment.admit_recipient("fp:alice", &[], None).is_err());
        assert_eq!(
            environment
                .admit_recipient(
                    "fp:alice",
                    &strings(&["ryeos.receive.provider_env.b_key"]),
                    None
                )
                .unwrap(),
            ["A_KEY", "B_KEY"]
        );
        // Undeclared requested names are left to delivery.
        assert!(
            environment
                .admit_recipient("fp:alice", &[], Some(&strings(&["OTHER_KEY"])))
                .unwrap()
                .is_empty()
        );
        assert!(
            ProviderEnvironment::none()
                .admit_recipient("fp:anyone", &[], None)
                .unwrap()
                .is_empty()
        );
    }

    const WILDCARD_GRANTS: [&str; 5] = [
        "*",
        "ryeos.*",
        "ryeos.receive.*",
        "ryeos.receive.provider_env.*",
        "ryeos.receive.provider_env.b_?ey",
    ];

    #[test]
    fn admission_accepts_only_the_exact_recipient_capability() {
        let environment = capability_declaration();
        // The exact capability is admitted, also alongside wildcards.
        assert_eq!(
            environment
                .admit_recipient(
                    "fp:bob",
                    &strings(&["*", "ryeos.receive.provider_env.b_key"]),
                    Some(&strings(&["B_KEY"]))
                )
                .unwrap(),
            ["B_KEY"]
        );
        for wildcard in WILDCARD_GRANTS {
            let error = environment
                .admit_recipient("fp:bob", &strings(&[wildcard]), Some(&strings(&["B_KEY"])))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(
                    "provider environment B_KEY requires the exact recipient capability \
                     ryeos.receive.provider_env.b_key; wildcard grants do not satisfy it"
                ),
                "{wildcard}: {error}"
            );
            assert!(!error.contains("value-b"), "{error}");
            // An open declaration refuses the same way.
            let error = environment
                .admit_recipient("fp:alice", &strings(&[wildcard]), None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("wildcard grants do not satisfy it"), "{wildcard}: {error}");
        }
    }

    #[test]
    fn live_re_judgement_refuses_each_wildcard_by_name() {
        let environment = capability_declaration();
        let requested = strings(&["A_KEY", "B_KEY"]);
        assert!(
            environment
                .refused_recipient_names(
                    &strings(&["B_KEY"]),
                    "fp:bob",
                    &strings(&["ryeos.receive.provider_env.b_key"])
                )
                .is_empty()
        );
        for wildcard in WILDCARD_GRANTS {
            assert_eq!(
                environment.refused_recipient_names(&requested, "fp:bob", &strings(&[wildcard])),
                vec![
                    ("A_KEY".to_owned(), RecipientRefusal::NotListed),
                    (
                        "B_KEY".to_owned(),
                        RecipientRefusal::WildcardOnly {
                            capability: "ryeos.receive.provider_env.b_key".to_owned(),
                        }
                    ),
                ],
                "{wildcard}"
            );
        }
    }
}
