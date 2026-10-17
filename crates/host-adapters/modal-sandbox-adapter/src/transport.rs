//! The facade transport over the shared HTTP/1.1 client: the facade origin
//! derived from the workspace, the proxy credentials from the sealed
//! credential descriptor, and the operation's deadlines.

use std::io::Read as _;

use anyhow::{Result, ensure};
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_http_transport::{
    ContactState, Deadlines, Header, HttpClient, HttpRequest, Limits, RequestBodySource,
};
use zeroize::Zeroizing;

use crate::facade::{FacadeTransport, MAX_REPLY_BYTES, RawReply, TransportFailure};
use crate::provider_spec::RouteName;

const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Modal's proxy rejects a request without the workspace proxy token pair.
const KEY_HEADER: &str = "Modal-Key";
const SECRET_HEADER: &str = "Modal-Secret";

/// A Modal workspace proxy auth token, read from the sealed credential as
/// `<token id>:<token secret>`.
pub struct ProxyToken {
    key: Zeroizing<String>,
    secret: Zeroizing<String>,
}

impl ProxyToken {
    pub fn parse(credential: &Zeroizing<String>) -> Result<Self> {
        let (key, secret) = credential
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("Modal credential is not <token id>:<token secret>"))?;
        let plain = |part: &str| {
            !part.is_empty()
                && part.len() <= 256
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        };
        ensure!(
            plain(key) && plain(secret),
            "Modal credential token id or secret is malformed"
        );
        Ok(Self {
            key: Zeroizing::new(key.to_owned()),
            secret: Zeroizing::new(secret.to_owned()),
        })
    }
}

/// The facade's origin: `https://<workspace>--<endpoint label>.modal.run`.
pub fn facade_host(workspace: &str, endpoint_label: &str) -> String {
    format!("{workspace}--{endpoint_label}.modal.run")
}

pub struct HttpFacadeTransport<'a> {
    pub network: &'a NetworkContext,
    pub host: String,
    pub token: ProxyToken,
    pub tls_roots_der: Vec<Vec<u8>>,
    pub deadline: MonotonicDeadline,
    pub cancellation: &'a NetworkCancellation,
}

impl FacadeTransport for HttpFacadeTransport<'_> {
    fn post(&self, route: RouteName, body: &[u8]) -> Result<RawReply, TransportFailure> {
        let url = url::Url::parse(&format!("https://{}{}", self.host, route.path()))
            .map_err(|_| TransportFailure::NoRequestSent)?;
        if url.scheme() != "https"
            || url.host_str() != Some(self.host.as_str())
            || url.port_or_known_default() != Some(443)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != route.path()
        {
            return Err(TransportFailure::NoRequestSent);
        }
        let limits = limits_for(route);
        let request = HttpRequest {
            method: "POST".into(),
            url,
            headers: vec![
                Header::new("Accept", "application/json"),
                Header::new("Content-Type", "application/json"),
                Header::new_sensitive(KEY_HEADER, Zeroizing::new(self.token.key.as_bytes().to_vec())),
                Header::new_sensitive(
                    SECRET_HEADER,
                    Zeroizing::new(self.token.secret.as_bytes().to_vec()),
                ),
            ],
            body: RequestBodySource::from_bytes(body.to_vec()),
            tls_roots_der: self.tls_roots_der.clone(),
            limits,
            deadlines: Deadlines::new(SETUP_TIMEOUT, IDLE_TIMEOUT, self.deadline),
            cancellation: self.cancellation.clone(),
        };
        let response = HttpClient::new(self.network.clone())
            .execute(request)
            .map_err(|error| match error.contact_state() {
                ContactState::NoRequestSent => TransportFailure::NoRequestSent,
                ContactState::RequestMayHaveBeenSent => TransportFailure::MayHaveBeenSent,
            })?;
        let json_content_type = has_json_content_type(&response.headers);
        let mut bytes = Vec::new();
        response
            .body
            .take(MAX_REPLY_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| TransportFailure::MayHaveBeenSent)?;
        Ok(RawReply {
            status: response.status,
            json_content_type,
            body: bytes,
        })
    }
}

/// The request limits for one route: the control-plane defaults with the
/// request body bound raised to what this route's payload needs.
fn limits_for(route: RouteName) -> Limits {
    let mut limits = Limits::control_plane();
    limits.request_body_bytes = route.request_body_limit();
    limits.response_body_bytes = MAX_REPLY_BYTES as u64;
    limits.response_body_wire_bytes = MAX_REPLY_BYTES as u64 * 2;
    limits
}

fn has_json_content_type(headers: &[Header]) -> bool {
    let mut values = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("content-type"));
    let Some(header) = values.next() else {
        return false;
    };
    if values.next().is_some()
        || !header
            .value()
            .iter()
            .all(|byte| *byte == b'\t' || (0x20..=0x7e).contains(byte))
    {
        return false;
    }
    std::str::from_utf8(header.value()).is_ok_and(is_json_content_type_value)
}

fn is_json_content_type_value(value: &str) -> bool {
    let mut parts = value.split(';');
    if !parts
        .next()
        .is_some_and(|media| trim_ows(media).eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    let Some(parameter) = parts.next() else {
        return true;
    };
    if parts.next().is_some() {
        return false;
    }
    let Some((name, value)) = trim_ows(parameter).split_once('=') else {
        return false;
    };
    let value = trim_ows(value);
    trim_ows(name).eq_ignore_ascii_case("charset")
        && (value.eq_ignore_ascii_case("utf-8") || value.eq_ignore_ascii_case("\"utf-8\""))
}

fn trim_ows(value: &str) -> &str {
    value.trim_matches(|character| matches!(character, ' ' | '\t'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_token_is_id_colon_secret() {
        let token = ProxyToken::parse(&Zeroizing::new("wk-abc:ws-def_1".to_owned())).unwrap();
        assert_eq!(token.key.as_str(), "wk-abc");
        assert_eq!(token.secret.as_str(), "ws-def_1");
        for bad in ["", "nocolon", ":secret", "id:", "a:b:c", "a b:c", "id:sec ret"] {
            assert!(
                ProxyToken::parse(&Zeroizing::new(bad.to_owned())).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn limits_follow_the_route_body_bound() {
        for route in [RouteName::Create, RouteName::Stage, RouteName::Upload, RouteName::RunOnce] {
            assert_eq!(limits_for(route).request_body_bytes, route.request_body_limit());
        }
        assert!(limits_for(RouteName::Stage).request_body_bytes > 32 * 1024 * 1024);
    }

    #[test]
    fn facade_host_joins_workspace_and_label() {
        assert_eq!(
            facade_host("acme", "ryeos-lease"),
            "acme--ryeos-lease.modal.run"
        );
    }

    #[test]
    fn json_content_type_is_exact() {
        for good in ["application/json", "Application/JSON; charset=utf-8", "application/json;charset=\"utf-8\""] {
            assert!(is_json_content_type_value(good), "{good}");
        }
        for bad in ["text/json", "application/json; charset=latin1", "application/json; a=b; c=d", "application/jsonx"] {
            assert!(!is_json_content_type_value(bad), "{bad}");
        }
    }
}
