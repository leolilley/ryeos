//! Per-name ceilings on the shared sealed vault.
//!
//! The vault is one store. The node's signed `sealed_vault_names` policy says,
//! per name, who may receive the value into a launch and who may set or
//! delete it. A name the policy does not list is operator-only: only the
//! node's configured local operator may receive or write it. Every judgement
//! here is made from the principal and the capabilities it holds, never from
//! values, and a refusal names the fix.

use ryeos_node_runtime::node_policy::sections::recipients::NameRecipients;
use ryeos_node_runtime::node_policy::sections::sealed_vault_names::SealedVaultNamesPolicy;

use super::provider_env::{RecipientRefusal, judge_recipient};

/// What a principal is trying to do with one sealed name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultNameAction {
    /// Have the value delivered into a launch.
    Receive,
    /// Set or delete the value.
    Write,
}

/// Why a principal may not act on one sealed vault name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultNameRefusal {
    pub kind: VaultNameRefusalKind,
    pub action: VaultNameAction,
    pub name: String,
    pub principal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultNameRefusalKind {
    /// The policy lists no ceiling for the name and the principal is not the
    /// configured local operator.
    NoCeiling,
    /// The name's ceiling lists neither the principal nor a capability it
    /// holds.
    NotListed,
    /// The principal holds a wildcard grant covering the listed capability,
    /// but not the capability itself.
    WildcardOnly { capability: String },
}

impl VaultNameRefusal {
    /// The stable machine code of this refusal.
    pub fn code(&self) -> &'static str {
        match (&self.kind, self.action) {
            (VaultNameRefusalKind::NoCeiling, _) => "vault_name_has_no_ceiling",
            (_, VaultNameAction::Receive) => "vault_name_not_delivered",
            (_, VaultNameAction::Write) => "vault_name_not_writable",
        }
    }

    /// The refusal text, naming the cure. Never carries a value.
    pub fn message(&self) -> String {
        let name = &self.name;
        let principal = &self.principal;
        let verb = match self.action {
            VaultNameAction::Receive => "receive",
            VaultNameAction::Write => "write",
        };
        let list = match self.action {
            VaultNameAction::Receive => "recipients",
            VaultNameAction::Write => "writers",
        };
        let fix = format!(
            "the node operator applies the signed `sealed_vault_names` policy with an entry for \
             `{name}` whose `{list}` lists the principal or an exact capability it holds \
             (`ryeos node policy-apply sealed_vault_names <source.yaml>`, node stopped)"
        );
        match &self.kind {
            VaultNameRefusalKind::NoCeiling => format!(
                "vault name `{name}` has no ceiling, so only the configured local operator may \
                 {verb} it and principal `{principal}` may not: {fix}"
            ),
            VaultNameRefusalKind::NotListed => {
                let passive = match self.action {
                    VaultNameAction::Receive => "delivered to",
                    VaultNameAction::Write => "written by",
                };
                format!(
                    "vault name `{name}` may not be {passive} principal `{principal}`: its \
                     ceiling lists neither that principal nor a capability it holds: {fix}"
                )
            }
            VaultNameRefusalKind::WildcardOnly { capability } => format!(
                "vault name `{name}` requires the exact {list} capability `{capability}`; a \
                 wildcard grant does not satisfy it (principal `{principal}`)"
            ),
        }
    }
}

impl std::fmt::Display for VaultNameRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for VaultNameRefusal {}

/// The node's sealed-vault ceilings and the principal that holds every name
/// that has none.
#[derive(Debug, Clone)]
pub struct VaultCeilings {
    policy: SealedVaultNamesPolicy,
    operator_principal: String,
}

impl VaultCeilings {
    /// Bind the node's signed policy to its configured local operator.
    pub fn new(policy: SealedVaultNamesPolicy, operator_principal: String) -> anyhow::Result<Self> {
        policy.validate()?;
        if operator_principal.is_empty() || operator_principal.trim() != operator_principal {
            anyhow::bail!("sealed vault operator principal is not canonical");
        }
        Ok(Self {
            policy,
            operator_principal,
        })
    }

    /// The ceilings of the node's compiled policy generation, bound to the
    /// operator whose signing key is at `operator_signing_key_path`.
    pub fn from_node_policy(
        snapshot: &ryeos_node_runtime::node_policy::NodePolicySnapshot,
        operator_signing_key_path: &std::path::Path,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let policy = snapshot.require::<SealedVaultNamesPolicy>()?.clone();
        let operator = ryeos_node_runtime::identity::NodeIdentity::load(operator_signing_key_path)
            .context("load the configured local operator identity for the sealed vault")?;
        Self::new(policy, operator.principal_id())
    }

    /// No name carries a ceiling: only `operator_principal` may act on any.
    pub fn operator_only(operator_principal: impl Into<String>) -> Self {
        Self {
            policy: SealedVaultNamesPolicy::operator_only(),
            operator_principal: operator_principal.into(),
        }
    }

    pub fn operator_principal(&self) -> &str {
        &self.operator_principal
    }

    pub fn policy(&self) -> &SealedVaultNamesPolicy {
        &self.policy
    }

    /// Judge `principal`, holding `scopes`, receiving `name` into a launch.
    pub fn judge_receive(
        &self,
        name: &str,
        principal: &str,
        scopes: &[String],
    ) -> Result<(), VaultNameRefusal> {
        self.judge(name, principal, scopes, VaultNameAction::Receive)
    }

    /// Judge `principal`, holding `scopes`, setting or deleting `name`.
    pub fn judge_write(
        &self,
        name: &str,
        principal: &str,
        scopes: &[String],
    ) -> Result<(), VaultNameRefusal> {
        self.judge(name, principal, scopes, VaultNameAction::Write)
    }

    /// Whether `principal` may receive or write `name`; what a listing shows.
    pub fn is_visible(&self, name: &str, principal: &str, scopes: &[String]) -> bool {
        self.judge_receive(name, principal, scopes).is_ok()
            || self.judge_write(name, principal, scopes).is_ok()
    }

    fn judge(
        &self,
        name: &str,
        principal: &str,
        scopes: &[String],
        action: VaultNameAction,
    ) -> Result<(), VaultNameRefusal> {
        let refuse = |kind| VaultNameRefusal {
            kind,
            action,
            name: name.to_owned(),
            principal: principal.to_owned(),
        };
        let Some(entry) = self.policy.entry(name) else {
            return if principal == self.operator_principal {
                Ok(())
            } else {
                Err(refuse(VaultNameRefusalKind::NoCeiling))
            };
        };
        let ceiling: &NameRecipients = match action {
            VaultNameAction::Receive => &entry.recipients,
            VaultNameAction::Write => &entry.writers,
        };
        match judge_recipient(ceiling, principal, scopes) {
            None => Ok(()),
            Some(RecipientRefusal::NotListed) => Err(refuse(VaultNameRefusalKind::NotListed)),
            Some(RecipientRefusal::WildcardOnly { capability }) => {
                Err(refuse(VaultNameRefusalKind::WildcardOnly { capability }))
            }
        }
    }

    /// The stored names that have no ceiling, in the order given.
    pub fn names_without_ceiling<'a>(&self, stored: &'a [String]) -> Vec<&'a str> {
        stored
            .iter()
            .map(String::as_str)
            .filter(|name| self.policy.entry(name).is_none())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ryeos_node_runtime::node_policy::sections::sealed_vault_names::SealedVaultName;

    use super::*;

    const OPERATOR: &str = "fp:operator";

    fn recipients(principals: &[&str], capabilities: &[&str]) -> NameRecipients {
        NameRecipients {
            principals: principals.iter().map(|value| (*value).to_owned()).collect(),
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    fn ceilings(entries: Vec<SealedVaultName>) -> VaultCeilings {
        VaultCeilings::new(
            SealedVaultNamesPolicy {
                schema: 1,
                names: entries,
            },
            OPERATOR.to_owned(),
        )
        .unwrap()
    }

    fn scopes(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn api_key(recipient: NameRecipients, writer: NameRecipients) -> SealedVaultName {
        SealedVaultName {
            name: "API_KEY".to_owned(),
            recipients: recipient,
            writers: writer,
        }
    }

    #[test]
    fn sealed_name_delivered_to_listed_principal() {
        let ceilings = ceilings(vec![api_key(
            recipients(&["fp:b"], &[]),
            recipients(&[OPERATOR], &[]),
        )]);
        ceilings.judge_receive("API_KEY", "fp:b", &[]).unwrap();
    }

    #[test]
    fn sealed_name_delivered_to_exact_capability_holder() {
        let ceilings = ceilings(vec![api_key(
            recipients(&[], &["ryeos.receive.vault.api_key"]),
            recipients(&[OPERATOR], &[]),
        )]);
        ceilings
            .judge_receive("API_KEY", "fp:c", &scopes(&["ryeos.receive.vault.api_key"]))
            .unwrap();
        let refusal = ceilings
            .judge_receive("API_KEY", "fp:c", &scopes(&["ryeos.receive.vault.*"]))
            .unwrap_err();
        assert_eq!(refusal.code(), "vault_name_not_delivered");
        assert!(matches!(
            refusal.kind,
            VaultNameRefusalKind::WildcardOnly { .. }
        ));
    }

    #[test]
    fn sealed_name_refused_for_unlisted_principal() {
        let ceilings = ceilings(vec![api_key(
            recipients(&["fp:b"], &[]),
            recipients(&[OPERATOR], &[]),
        )]);
        let refusal = ceilings
            .judge_receive("API_KEY", "fp:other", &[])
            .unwrap_err();
        assert_eq!(refusal.code(), "vault_name_not_delivered");
        let text = refusal.message();
        assert!(
            text.contains("API_KEY") && text.contains("fp:other"),
            "{text}"
        );
        assert!(text.contains("policy-apply sealed_vault_names"), "{text}");
        // Listing the operator as writer-only does not make it a recipient.
        assert!(ceilings.judge_receive("API_KEY", OPERATOR, &[]).is_err());
    }

    #[test]
    fn unlisted_name_operator_only_by_default() {
        let ceilings = ceilings(Vec::new());
        ceilings.judge_receive("ANY_NAME", OPERATOR, &[]).unwrap();
        ceilings.judge_write("ANY_NAME", OPERATOR, &[]).unwrap();
        for action in [
            ceilings.judge_receive("ANY_NAME", "fp:b", &scopes(&["*"])),
            ceilings.judge_write("ANY_NAME", "fp:b", &scopes(&["*"])),
        ] {
            let refusal = action.unwrap_err();
            assert_eq!(refusal.code(), "vault_name_has_no_ceiling");
            assert!(
                refusal
                    .message()
                    .contains("policy-apply sealed_vault_names")
            );
        }
    }

    #[test]
    fn writer_ceiling_refuses_overwrite_by_other_principal() {
        let ceilings = ceilings(vec![api_key(
            recipients(&["fp:b"], &[]),
            recipients(&[OPERATOR], &[]),
        )]);
        ceilings.judge_write("API_KEY", OPERATOR, &[]).unwrap();
        let refusal = ceilings
            .judge_write("API_KEY", "fp:b", &scopes(&["*"]))
            .unwrap_err();
        assert_eq!(refusal.code(), "vault_name_not_writable");
    }

    #[test]
    fn list_shows_only_receivable_or_writable_names() {
        let mut second = api_key(recipients(&["fp:x"], &[]), recipients(&["fp:b"], &[]));
        second.name = "OTHER_KEY".to_owned();
        let ceilings = ceilings(vec![
            api_key(recipients(&["fp:b"], &[]), recipients(&[OPERATOR], &[])),
            second,
        ]);
        // fp:b receives API_KEY and writes OTHER_KEY; UNLISTED has no ceiling.
        assert!(ceilings.is_visible("API_KEY", "fp:b", &[]));
        assert!(ceilings.is_visible("OTHER_KEY", "fp:b", &[]));
        assert!(!ceilings.is_visible("UNLISTED", "fp:b", &[]));
        assert!(ceilings.is_visible("UNLISTED", OPERATOR, &[]));
        assert!(!ceilings.is_visible("API_KEY", "fp:z", &[]));
        assert_eq!(
            ceilings.names_without_ceiling(&["API_KEY".to_owned(), "UNLISTED".to_owned()]),
            vec!["UNLISTED"]
        );
    }
}
