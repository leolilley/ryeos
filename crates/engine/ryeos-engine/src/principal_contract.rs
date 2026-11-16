//! Portable authenticated-principal vocabulary shared by ingress, scheduling,
//! and execution admission.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizedKeyPrincipalClass {
    LocalClient,
    RemoteNode,
    RemotePrincipal,
}

impl AuthorizedKeyPrincipalClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalClient => "local_client",
            Self::RemoteNode => "remote_node",
            Self::RemotePrincipal => "remote_principal",
        }
    }

    pub const fn is_remote(self) -> bool {
        matches!(self, Self::RemoteNode | Self::RemotePrincipal)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardingAuthorityEvidence {
    pub source_node_fingerprint: String,
    pub source_node_grant_hash: String,
    /// Digest of the principal-signed delegation the forwarded request
    /// carried, when the source node acted for the principal under one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_digest: Option<String>,
    /// Unix time after which that delegation no longer admits anything.
    /// Present exactly when `delegation_digest` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_not_after: Option<u64>,
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
            (AuthorizedKeyPrincipalClass::RemotePrincipal, Some(forwarding)) => {
                validate_sha256(
                    "forwarding source-node fingerprint",
                    &forwarding.source_node_fingerprint,
                )?;
                validate_sha256(
                    "forwarding source-node grant hash",
                    &forwarding.source_node_grant_hash,
                )?;
                match (
                    forwarding.delegation_digest.as_deref(),
                    forwarding.delegation_not_after,
                ) {
                    (Some(digest), Some(_)) => {
                        validate_sha256("forwarding delegation digest", digest)?;
                    }
                    (None, None) => {}
                    _ => anyhow::bail!(
                        "forwarding delegation digest and expiry must be recorded together"
                    ),
                }
            }
            (AuthorizedKeyPrincipalClass::RemotePrincipal, None) => {
                anyhow::bail!(
                    "remote-principal grant authority requires source-node grant evidence"
                )
            }
            (_, Some(_)) => {
                anyhow::bail!(
                    "only remote-principal grant authority may carry source-node evidence"
                )
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
