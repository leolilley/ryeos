use crate::error::CliTransportError;

/// Result of audience discovery: the remote's signing audience plus the
/// **effective base URL** the daemon actually answered on, after any
/// redirects were followed.
///
/// Discovery is an *unsigned* GET, so it may safely follow an http→https edge
/// redirect. Subsequent **signed** requests must target `effective_base_url`
/// directly and must NOT rely on a redirect: a signature is bound to
/// method/path/body, and a redirected POST can be downgraded to GET
/// (301/302/303), invalidating it. Threading the post-redirect base here is
/// what lets the signed dispatch hit the canonical origin in one hop.
pub struct DiscoveredAudience {
    pub principal_id: String,
    pub effective_base_url: String,
}

/// Discover the daemon's principal_id by calling GET /public-key, and report
/// the effective base URL after redirects.
///
/// Uses `reqwest` — the same TLS-and-redirect-capable client the rest of the
/// remote path uses (mirrors `ryeos-api`'s `RemoteClient::get_public_key`) — so
/// an `https://` node URL negotiates TLS on 443 and an http→https edge `301` is
/// followed, instead of the prior hand-rolled plaintext client that could only
/// reach `:80` and died on the redirect.
pub async fn discover_audience(daemon_url: &str) -> Result<DiscoveredAudience, CliTransportError> {
    let url = format!("{}/public-key", daemon_url.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("client build: {e}"),
        })?;

    let resp = client
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("request: {e}"),
        })?;

    // The origin the daemon answered on, post-redirect, minus the
    // `/public-key` probe path — so signed dispatch targets it directly.
    let effective_base_url = effective_base_from_public_key_url(resp.url().as_str())?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("read body: {e}"),
        })?;

    if !status.is_success() {
        return Err(CliTransportError::AudienceDiscoveryFailed {
            url,
            detail: format!("HTTP {}: {}", status, body.trim()),
        });
    }

    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("JSON decode: {e}"),
        })?;

    let principal_id = value
        .get("principal_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| CliTransportError::AudienceDiscoveryFailed {
            url,
            detail: "response missing 'principal_id' field".into(),
        })?;

    Ok(DiscoveredAudience {
        principal_id,
        effective_base_url,
    })
}

/// Derive the effective base URL from the (post-redirect) `/public-key` URL the
/// daemon answered on: drop any query/fragment, strip a trailing slash and the
/// `/public-key` probe path, preserving any host path prefix and explicit port.
///
/// REQUIRES the resolved path to end in `/public-key`. An unexpected redirect
/// target (a login page, a different path, a query-bearing URL whose path is
/// not `/public-key`) is rejected here — discovery fails loudly rather than
/// letting the dispatcher append signed paths onto a wrong base and mis-target
/// the signed request.
fn effective_base_from_public_key_url(public_key_url: &str) -> Result<String, CliTransportError> {
    // The base is `scheme://host[:port][/prefix]`; query/fragment belong to the
    // probe request, not the base. Strip fragment, then query, then derive.
    let without_fragment = public_key_url.split('#').next().unwrap_or(public_key_url);
    let path_part = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let trimmed = path_part.trim_end_matches('/');
    trimmed
        .strip_suffix("/public-key")
        .map(str::to_string)
        .ok_or_else(|| CliTransportError::AudienceDiscoveryFailed {
            url: public_key_url.to_string(),
            detail: format!(
                "discovery resolved to an unexpected URL whose path is not /public-key: \
                 {public_key_url}"
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::effective_base_from_public_key_url as base;

    #[test]
    fn strips_public_key_suffix() {
        assert_eq!(
            base("https://node.example.com/public-key").unwrap(),
            "https://node.example.com"
        );
    }

    #[test]
    fn preserves_host_path_prefix() {
        // A configured URL with a path prefix keeps it after redirect.
        assert_eq!(
            base("https://host/prefix/public-key").unwrap(),
            "https://host/prefix"
        );
    }

    #[test]
    fn tolerates_trailing_slash() {
        assert_eq!(base("https://host/public-key/").unwrap(), "https://host");
    }

    #[test]
    fn preserves_explicit_port() {
        // The local-daemon shape must round-trip unchanged.
        assert_eq!(
            base("http://127.0.0.1:7400/public-key").unwrap(),
            "http://127.0.0.1:7400"
        );
    }

    #[test]
    fn drops_query_and_fragment() {
        assert_eq!(base("https://host/public-key?x=1").unwrap(), "https://host");
        assert_eq!(
            base("https://host/public-key#frag").unwrap(),
            "https://host"
        );
    }

    #[test]
    fn rejects_unexpected_target() {
        // A redirect to a non-/public-key path must fail discovery, not be
        // silently used as a dispatch base.
        assert!(base("https://host/login").is_err());
        assert!(base("https://host/public-key-ish/other").is_err());
    }

    #[test]
    fn percent_encoding_is_conservative() {
        // Suffix matching is exact/string-based: a percent-encoded `public-key`
        // is NOT treated as the probe path, so it fails closed rather than
        // matching a decoded form the daemon never served.
        assert!(base("https://host/%70ublic-key").is_err());
        assert!(base("https://host/prefix%2Fpublic-key").is_err());
        // A percent-encoded PREFIX is preserved byte-for-byte (no decoding).
        assert_eq!(
            base("https://host/%70refix/public-key").unwrap(),
            "https://host/%70refix"
        );
    }
}
