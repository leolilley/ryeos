//! Portable authenticated-principal vocabulary shared by ingress, scheduling,
//! and execution admission.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizedKeyPrincipalClass {
    LocalClient,
    RemoteNode,
    RemoteOperator,
}

impl AuthorizedKeyPrincipalClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalClient => "local_client",
            Self::RemoteNode => "remote_node",
            Self::RemoteOperator => "remote_operator",
        }
    }

    pub const fn is_remote(self) -> bool {
        matches!(self, Self::RemoteNode | Self::RemoteOperator)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardingAuthorityEvidence {
    pub source_node_fingerprint: String,
    pub source_node_grant_hash: String,
}

/// Stable grant-generation evidence produced by authenticated RyeOS request
/// verification. Ephemeral request timestamps, nonces, and signatures are
/// deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedGrantAuthority {
    pub principal_grant_hash: String,
    pub forwarding: Option<ForwardingAuthorityEvidence>,
}

impl AuthenticatedGrantAuthority {
    pub fn validate_for_class(&self, class: AuthorizedKeyPrincipalClass) -> anyhow::Result<()> {
        validate_sha256("principal grant hash", &self.principal_grant_hash)?;
        match (class, self.forwarding.as_ref()) {
            (AuthorizedKeyPrincipalClass::RemoteOperator, Some(forwarding)) => {
                validate_sha256(
                    "forwarding source-node fingerprint",
                    &forwarding.source_node_fingerprint,
                )?;
                validate_sha256(
                    "forwarding source-node grant hash",
                    &forwarding.source_node_grant_hash,
                )?;
            }
            (AuthorizedKeyPrincipalClass::RemoteOperator, None) => {
                anyhow::bail!("remote-operator grant authority requires source-node grant evidence")
            }
            (_, Some(_)) => {
                anyhow::bail!("only remote-operator grant authority may carry source-node evidence")
            }
            (_, None) => {}
        }
        Ok(())
    }
}

fn validate_sha256(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("{label} must be lowercase SHA-256 hex");
    }
    Ok(())
}

/// Validate the one wire spelling accepted for authenticated RyeOS site
/// identities.
pub fn validate_canonical_site_id(site_id: &str) -> anyhow::Result<()> {
    let Some(name) = site_id.strip_prefix("site:") else {
        anyhow::bail!("site id must begin with `site:`");
    };
    if name.is_empty() || site_id.len() > 255 {
        anyhow::bail!("site id must contain a name and be at most 255 bytes");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        anyhow::bail!(
            "site id may contain only ASCII letters, digits, `.`, `_`, and `-` after `site:`"
        );
    }
    Ok(())
}
