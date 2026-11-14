//! Who may act on one sealed name: the shared ceiling shape.
//!
//! Provider-delivered environment names and sealed vault names both carry
//! per-name ceilings written as principals and exact capabilities. A listed
//! principal passes; otherwise the principal passes when it holds one listed
//! capability exactly. Wildcard patterns never appear in the lists, and a
//! wildcard grant covering a listed capability does not satisfy it.

use anyhow::bail;
use serde::{Deserialize, Serialize};

pub const MAX_NAME_RECIPIENTS: usize = 64;
pub const MAX_NAME_RECIPIENT_BYTES: usize = 256;

/// Recipient or writer ceiling for one sealed name. A requesting principal is a
/// recipient when it is listed in `principals`, or when its admitted
/// capabilities satisfy any one entry of `capabilities`. Both lists are
/// open strings, sorted and unique; at least one entry is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameRecipients {
    pub principals: Vec<String>,
    pub capabilities: Vec<String>,
}

impl NameRecipients {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.principals.is_empty() && self.capabilities.is_empty() {
            bail!("at least one recipient principal or capability is required");
        }
        validate_recipient_list("principal", &self.principals)?;
        validate_recipient_list("capability", &self.capabilities)?;
        for principal in &self.principals {
            if principal.chars().any(char::is_whitespace) {
                bail!("recipient principal `{principal}` contains whitespace");
            }
        }
        for capability in &self.capabilities {
            // A recipient capability names one exact capability, and only a
            // grant of that exact capability qualifies; a wildcard grant
            // covering it does not. A wildcard here would admit every holder
            // of any matching grant.
            if capability.contains('*') || capability.contains('?') {
                bail!("recipient capability `{capability}` must not be a wildcard pattern");
            }
            ryeos_runtime::authorizer::validate_scope_pattern(capability).map_err(|error| {
                anyhow::anyhow!("recipient capability `{capability}` is refused: {error}")
            })?;
            if ryeos_runtime::authorizer::Capability::parse(capability).is_err() {
                bail!("recipient capability `{capability}` must name verb, kind and subject");
            }
        }
        Ok(())
    }
}

fn validate_recipient_list(label: &str, values: &[String]) -> anyhow::Result<()> {
    if values.len() > MAX_NAME_RECIPIENTS {
        bail!("more than {MAX_NAME_RECIPIENTS} recipient {label} entries");
    }
    let mut previous: Option<&str> = None;
    for value in values {
        if value.is_empty() || value.trim() != value {
            bail!("recipient {label} entries must be non-empty and trimmed");
        }
        if value.len() > MAX_NAME_RECIPIENT_BYTES {
            bail!("recipient {label} `{value}` exceeds {MAX_NAME_RECIPIENT_BYTES} bytes");
        }
        if value.chars().any(char::is_control) {
            bail!("recipient {label} entries must not contain control characters");
        }
        if previous.is_some_and(|candidate| candidate >= value.as_str()) {
            bail!("recipient {label} entries must be sorted and unique");
        }
        previous = Some(value);
    }
    Ok(())
}
