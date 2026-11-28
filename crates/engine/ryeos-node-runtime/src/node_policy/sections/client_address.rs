//! Node-owned client-address policy: which peers may vouch for the address a
//! request came from, and which header they use to say so.
//!
//! The section is required in every profile. An empty `trusted_proxies` list
//! means the socket peer is the address.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "client_address";

/// The header a trusted proxy uses to carry the client address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAddressHeader {
    Forwarded,
    XForwardedFor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAddressPolicy {
    pub schema: u32,
    pub trusted_proxies: Vec<String>,
    pub header: ClientAddressHeader,
}

/// A CIDR is `<ip>/<prefix>`; the prefix is required and bounded by the
/// address family.
fn validate_cidr(cidr: &str) -> anyhow::Result<()> {
    let Some((address, prefix)) = cidr.split_once('/') else {
        bail!("trusted proxy `{cidr}` must be a CIDR (`<address>/<prefix>`)");
    };
    let address: IpAddr = address
        .parse()
        .with_context(|| format!("trusted proxy `{cidr}` has an invalid address"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("trusted proxy `{cidr}` has an invalid prefix length"))?;
    let max = if address.is_ipv4() { 32 } else { 128 };
    if prefix > max {
        bail!("trusted proxy `{cidr}` prefix length exceeds {max}");
    }
    Ok(())
}

impl ClientAddressPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("client_address policy schema is not current");
        }
        for cidr in &self.trusted_proxies {
            validate_cidr(cidr)?;
        }
        Ok(())
    }
}

/// Where a request came from, as far as the node can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAddress {
    /// A client address on the network. An unresolved request is represented
    /// by the absence of a `SourceAddress`, never by a stand-in value.
    Network(IpAddr),
}

/// True when `ip` lies inside the `<address>/<prefix>` network. An entry that
/// does not parse, or is of the other address family, contains nothing.
fn cidr_contains(cidr: &str, ip: IpAddr) -> bool {
    let Some((network, prefix)) = cidr.split_once('/') else {
        return false;
    };
    let (Ok(network), Ok(prefix)) = (network.parse::<IpAddr>(), prefix.parse::<u32>()) else {
        return false;
    };
    match (network.to_canonical(), ip.to_canonical()) {
        (IpAddr::V4(network), IpAddr::V4(ip)) if prefix <= 32 => {
            let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
            u32::from(network) & mask == u32::from(ip) & mask
        }
        (IpAddr::V6(network), IpAddr::V6(ip)) if prefix <= 128 => {
            let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
            u128::from(network) & mask == u128::from(ip) & mask
        }
        _ => false,
    }
}

/// Parse a bare address, optionally bracketed (IPv6) and optionally followed
/// by a port.
fn parse_node(node: &str) -> Option<IpAddr> {
    if let Some(rest) = node.strip_prefix('[') {
        let (inner, tail) = rest.split_once(']')?;
        if !(tail.is_empty()
            || tail
                .strip_prefix(':')
                .is_some_and(|port| port.parse::<u16>().is_ok()))
        {
            return None;
        }
        return inner.parse().ok();
    }
    if let Some((host, port)) = node.split_once(':') {
        // A single colon is `<ipv4>:<port>`; more than one is a bare IPv6.
        if !port.contains(':') {
            port.parse::<u16>().ok()?;
            return host.parse().ok();
        }
    }
    node.parse().ok()
}

/// Split on `separator` outside double-quoted strings.
fn split_unquoted(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch == separator && !quoted {
            parts.push(&text[start..index]);
            start = index + ch.len_utf8();
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Addresses named by a `Forwarded` header, in order. Elements without a
/// `for` parameter and `for` values that are not addresses (`unknown`,
/// obfuscated `_` identifiers) are skipped; any other unparsable value makes
/// the whole header malformed.
fn forwarded_addresses(header: &str) -> Option<Vec<IpAddr>> {
    let mut addresses = Vec::new();
    for element in split_unquoted(header, ',') {
        for pair in split_unquoted(element, ';') {
            let Some((name, value)) = pair.split_once('=') else {
                if pair.trim().is_empty() {
                    continue;
                }
                return None;
            };
            if !name.trim().eq_ignore_ascii_case("for") {
                continue;
            }
            let value = value.trim();
            let value = match value.strip_prefix('"') {
                Some(inner) => inner.strip_suffix('"')?,
                None => value,
            };
            if value.eq_ignore_ascii_case("unknown") || value.starts_with('_') {
                continue;
            }
            addresses.push(parse_node(value)?);
        }
    }
    Some(addresses)
}

/// Addresses named by an `X-Forwarded-For` header, in order. Any entry that is
/// not an address makes the header malformed.
fn x_forwarded_for_addresses(header: &str) -> Option<Vec<IpAddr>> {
    header
        .split(',')
        .map(|entry| parse_node(entry.trim()))
        .collect()
}

impl ClientAddressPolicy {
    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted_proxies
            .iter()
            .any(|cidr| cidr_contains(cidr, ip))
    }

    /// The address a request came from. `peer` is the TCP socket peer.
    /// `header_value` is
    /// the configured header's value (multiple header lines joined with a
    /// comma). The header is read only when the peer is a trusted proxy; the
    /// answer is then the right-most address in it that is not itself a
    /// trusted proxy. An absent or malformed header, or one naming only
    /// trusted proxies, yields the peer.
    pub fn resolve_source(&self, peer: SocketAddr, header_value: Option<&str>) -> SourceAddress {
        let peer = peer.ip().to_canonical();
        if !self.is_trusted(peer) {
            return SourceAddress::Network(peer);
        }
        let chain = header_value.and_then(|value| match self.header {
            ClientAddressHeader::Forwarded => forwarded_addresses(value),
            ClientAddressHeader::XForwardedFor => x_forwarded_for_addresses(value),
        });
        let client = chain
            .into_iter()
            .flatten()
            .rev()
            .map(|ip| ip.to_canonical())
            .find(|ip| !self.is_trusted(*ip));
        SourceAddress::Network(client.unwrap_or(peer))
    }

    /// Name of the HTTP header that carries the chain, lower case.
    pub fn header_name(&self) -> &'static str {
        match self.header {
            ClientAddressHeader::Forwarded => "forwarded",
            ClientAddressHeader::XForwardedFor => "x-forwarded-for",
        }
    }
}

impl TypedNodePolicy for ClientAddressPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct ClientAddressPolicySection;

impl NodePolicySection for ClientAddressPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: ClientAddressPolicy = serde_json::from_value(body.clone())
            .context("failed to parse client_address policy record")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.into(),
            source_file: format!("/node/policies/{SECTION_NAME}.yaml").into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn body(proxies: Value, header: &str) -> Value {
        serde_json::json!({"schema": 1, "trusted_proxies": proxies, "header": header})
    }

    #[test]
    fn empty_and_valid_proxies_compile() {
        let section = ClientAddressPolicySection;
        assert_eq!(section.name(), SECTION_NAME);
        assert!(
            section
                .parse(&ctx(), &body(serde_json::json!([]), "forwarded"))
                .is_ok()
        );
        let proxies = serde_json::json!(["10.0.0.0/8", "::1/128", "fd00::/8"]);
        let parsed = section
            .parse(&ctx(), &body(proxies, "x_forwarded_for"))
            .unwrap();
        let record = parsed
            .as_any()
            .downcast_ref::<ClientAddressPolicy>()
            .unwrap();
        assert_eq!(record.header, ClientAddressHeader::XForwardedFor);
    }

    #[test]
    fn bad_cidrs_are_refused() {
        for bad in [
            "10.0.0.1",
            "10.0.0.0/33",
            "::/129",
            "nope/8",
            "10.0.0.0/x",
            "/8",
            "",
        ] {
            let body = body(serde_json::json!([bad]), "forwarded");
            assert!(
                ClientAddressPolicySection.parse(&ctx(), &body).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn unknown_header_and_unknown_field_are_refused() {
        let section = ClientAddressPolicySection;
        assert!(
            section
                .parse(&ctx(), &body(serde_json::json!([]), "x-forwarded-for"))
                .is_err()
        );
        assert!(
            section
                .parse(&ctx(), &body(serde_json::json!([]), "via"))
                .is_err()
        );
        let mut extra = body(serde_json::json!([]), "forwarded");
        extra["trust_all"] = Value::Bool(true);
        assert!(section.parse(&ctx(), &extra).is_err());
    }

    #[test]
    fn missing_fields_and_stale_schema_are_refused() {
        let section = ClientAddressPolicySection;
        assert!(
            section
                .parse(
                    &ctx(),
                    &serde_json::json!({"schema": 1, "header": "forwarded"})
                )
                .is_err()
        );
        assert!(
            section
                .parse(
                    &ctx(),
                    &serde_json::json!({"schema": 1, "trusted_proxies": []})
                )
                .is_err()
        );
        let mut stale = body(serde_json::json!([]), "forwarded");
        stale["schema"] = Value::from(2);
        assert!(section.parse(&ctx(), &stale).is_err());
    }

    fn policy(proxies: &[&str], header: ClientAddressHeader) -> ClientAddressPolicy {
        ClientAddressPolicy {
            schema: 1,
            trusted_proxies: proxies.iter().map(|p| p.to_string()).collect(),
            header,
        }
    }

    fn peer(text: &str) -> SocketAddr {
        format!("{text}:443").parse().unwrap()
    }

    fn net(text: &str) -> SourceAddress {
        SourceAddress::Network(text.parse().unwrap())
    }

    #[test]
    fn untrusted_peer_header_is_never_read() {
        let policy = policy(&["10.0.0.0/8"], ClientAddressHeader::XForwardedFor);
        assert_eq!(
            policy.resolve_source(peer("203.0.113.9"), Some("198.51.100.1")),
            net("203.0.113.9")
        );
    }

    #[test]
    fn trusted_peer_takes_rightmost_untrusted_hop() {
        let policy = policy(&["10.0.0.0/8"], ClientAddressHeader::XForwardedFor);
        assert_eq!(
            policy.resolve_source(peer("10.0.0.1"), Some("198.51.100.7, 10.0.0.2")),
            net("198.51.100.7")
        );
    }

    #[test]
    fn forwarded_header_parses_quotes_brackets_ports_and_unknown() {
        let policy = policy(&["10.0.0.0/8"], ClientAddressHeader::Forwarded);
        let header = r#"for="[2001:db8::1]:4711";proto=https, for=10.0.0.2"#;
        assert_eq!(
            policy.resolve_source(peer("10.0.0.1"), Some(header)),
            net("2001:db8::1")
        );
        assert_eq!(
            policy.resolve_source(peer("10.0.0.1"), Some("for=192.0.2.5:80, for=unknown")),
            net("192.0.2.5")
        );
    }

    #[test]
    fn trusted_peer_with_bad_or_missing_header_gives_peer() {
        let xff = policy(&["10.0.0.0/8"], ClientAddressHeader::XForwardedFor);
        assert_eq!(xff.resolve_source(peer("10.0.0.1"), None), net("10.0.0.1"));
        assert_eq!(
            xff.resolve_source(peer("10.0.0.1"), Some("198.51.100.7, garbage")),
            net("10.0.0.1")
        );
        let fwd = policy(&["10.0.0.0/8"], ClientAddressHeader::Forwarded);
        assert_eq!(
            fwd.resolve_source(peer("10.0.0.1"), Some("for=\"not-an-address")),
            net("10.0.0.1")
        );
    }
}
