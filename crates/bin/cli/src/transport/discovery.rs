use crate::error::CliTransportError;

/// Result of audience discovery: the remote's signing audience plus the exact
/// validated base URL used for both discovery and the subsequent signed call.
#[derive(Debug)]
pub struct DiscoveredAudience {
    pub principal_id: String,
    pub base_url: String,
}

/// Discover the daemon's principal_id by calling GET /public-key at one exact
/// admitted origin. Discovery is unsigned, so redirects are never authority:
/// a 3xx response fails instead of selecting a different host, scheme, port,
/// or path prefix for the subsequent signed request.
pub async fn discover_audience(daemon_url: &str) -> Result<DiscoveredAudience, CliTransportError> {
    let base_url = normalize_daemon_base_url(daemon_url)?;
    let url = format!("{base_url}/public-key");

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
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
        base_url,
    })
}

fn normalize_daemon_base_url(daemon_url: &str) -> Result<String, CliTransportError> {
    let fail = |detail: String| CliTransportError::AudienceDiscoveryFailed {
        // Input has not passed the credential/query/component checks yet.
        // Never retain it in an error: a rejected URL may itself contain a
        // password or token.
        url: "<rejected-daemon-url>".to_owned(),
        detail,
    };
    if daemon_url
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(fail(
            "daemon URL contains control or whitespace characters".to_owned(),
        ));
    }
    let mut parsed = reqwest::Url::parse(daemon_url)
        .map_err(|error| fail(format!("invalid daemon URL: {error}")))?;
    if parsed.username() != "" || parsed.password().is_some() {
        return Err(fail("daemon URL must not contain credentials".to_owned()));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(fail(
            "daemon base URL must not contain a query or fragment".to_owned(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| fail("daemon URL has no host".to_owned()))?;
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    match parsed.scheme() {
        "https" => {}
        "http" if is_loopback => {}
        scheme => {
            return Err(fail(format!(
                "daemon URL must use HTTPS except HTTP loopback (got scheme `{scheme}`)"
            )));
        }
    }
    let normalized_path = parsed.path().trim_end_matches('/').to_owned();
    parsed.set_path(&normalized_path);
    Ok(parsed.to_string().trim_end_matches('/').to_owned())
}

#[cfg(test)]
mod tests {
    use super::normalize_daemon_base_url as base;

    #[test]
    fn accepts_exact_https_origin() {
        assert_eq!(
            base("https://node.example.com").unwrap(),
            "https://node.example.com"
        );
    }

    #[test]
    fn preserves_host_path_prefix() {
        assert_eq!(base("https://host/prefix").unwrap(), "https://host/prefix");
    }

    #[test]
    fn tolerates_trailing_slash() {
        assert_eq!(base("https://host/").unwrap(), "https://host");
    }

    #[test]
    fn preserves_explicit_port() {
        // The local-daemon shape must round-trip unchanged.
        assert_eq!(
            base("http://127.0.0.1:7400").unwrap(),
            "http://127.0.0.1:7400"
        );
    }

    #[test]
    fn rejects_query_fragment_and_credentials() {
        assert!(base("https://host?x=1").is_err());
        assert!(base("https://host#frag").is_err());
        assert!(base("https://user:secret@host").is_err());

        for rejected in [
            "https://host?token=do-not-retain",
            "https://user:do-not-retain@host",
            "https://user:do-not-retain@[",
        ] {
            let rendered = base(rejected).unwrap_err().to_string();
            assert!(!rendered.contains("do-not-retain"), "{rendered}");
        }
    }

    #[test]
    fn rejects_plaintext_non_loopback() {
        assert!(base("http://node.example.com").is_err());
        assert!(base("http://192.0.2.10:7400").is_err());
        assert!(base("http://0.0.0.0:7400").is_err());
        assert!(base("http://localhost:7400").is_ok());
        assert!(base("http://[::1]:7400").is_ok());
    }

    #[test]
    fn preserves_an_encoded_path_prefix() {
        assert_eq!(
            base("https://host/%70refix").unwrap(),
            "https://host/%70refix"
        );
    }
}
