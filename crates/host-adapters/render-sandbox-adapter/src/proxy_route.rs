//! Exact, fail-closed Render Sandbox proxy destination policy.
//!
//! The pinned early-access CLI gives one run-URI example and uses a mock
//! upload URI of this shape. Neither is a normative provider guarantee. This
//! module does not contact that URI or establish that live Render will use it.
//! It only prevents a future activation implementation from forwarding a
//! short-lived bearer token to a different destination.

use anyhow::{Result, ensure};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProxyOperation<'a> {
    UploadFile { remote_path: &'a str },
    RunStream,
}

pub(crate) fn validate_proxy_route(
    uri: &str,
    method: &str,
    occurrence_id: &str,
    region: &str,
    operation: ProxyOperation<'_>,
) -> Result<Url> {
    ensure!(
        valid_dns_label(occurrence_id, 128) && occurrence_id.starts_with("sbx-"),
        "Render proxy occurrence is invalid"
    );
    ensure!(
        valid_dns_label(region, 64),
        "Render proxy region is invalid"
    );
    ensure!(uri.len() <= 4096, "Render proxy URI exceeds its bound");

    // Construct rather than merely suffix-check the authority: the bearer
    // token must never be forwarded to a sibling Sandbox or another host.
    let host = format!("{occurrence_id}.{region}.sandbox.onrender.com");
    let mut expected = Url::parse(&format!("https://{host}/"))?;
    match operation {
        ProxyOperation::UploadFile { remote_path } => {
            ensure!(method == "PUT", "Render upload proxy method changed");
            ensure!(
                valid_remote_path(remote_path),
                "Render upload path is invalid"
            );
            expected.set_path("/files/upload");
            expected.query_pairs_mut().append_pair("path", remote_path);
        }
        ProxyOperation::RunStream => {
            ensure!(method == "POST", "Render run proxy method changed");
            expected.set_path("/runs/stream");
        }
    }
    // Exact serialization rejects credentials, ports, fragments, alternate
    // encodings, duplicate query parameters, and a changed provider route.
    ensure!(uri == expected.as_str(), "Render proxy URI changed");
    Ok(expected)
}

fn valid_dns_label(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_remote_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= 2048
        && path.split('/').skip(1).all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OCCURRENCE: &str = "sbx-abc123";
    const REGION: &str = "oregon";

    #[test]
    fn accepts_only_exact_example_proxy_routes() {
        assert!(
            validate_proxy_route(
                "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream",
                "POST",
                OCCURRENCE,
                REGION,
                ProxyOperation::RunStream,
            )
            .is_ok()
        );
        assert!(validate_proxy_route(
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2Fryeos-bootstrap",
            "PUT",
            OCCURRENCE,
            REGION,
            ProxyOperation::UploadFile { remote_path: "/tmp/ryeos-bootstrap" },
        )
        .is_ok());
    }

    #[test]
    fn rejects_proxy_authority_and_route_substitution() {
        let baseline = "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream";
        for candidate in [
            "http://sbx-abc123.oregon.sandbox.onrender.com/runs/stream",
            "https://sbx-other.oregon.sandbox.onrender.com/runs/stream",
            "https://sbx-abc123.singapore.sandbox.onrender.com/runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com.evil.test/runs/stream",
            "https://user@sbx-abc123.oregon.sandbox.onrender.com/runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com:444/runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com:443/runs/stream",
            "https://SBX-ABC123.oregon.sandbox.onrender.com/runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com./runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream?next=evil",
            "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream#fragment",
            "https://sbx-abc123.oregon.sandbox.onrender.com/runs/%73tream",
        ] {
            assert!(
                validate_proxy_route(
                    candidate,
                    "POST",
                    OCCURRENCE,
                    REGION,
                    ProxyOperation::RunStream
                )
                .is_err(),
                "accepted {candidate}"
            );
        }
        assert!(
            validate_proxy_route(
                baseline,
                "PUT",
                OCCURRENCE,
                REGION,
                ProxyOperation::RunStream
            )
            .is_err()
        );
        assert!(
            validate_proxy_route(
                baseline,
                "POST",
                "sbx-other",
                REGION,
                ProxyOperation::RunStream
            )
            .is_err()
        );
        assert!(
            validate_proxy_route(
                baseline,
                "POST",
                &format!("sbx-{}", "a".repeat(129)),
                REGION,
                ProxyOperation::RunStream
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_upload_path_and_query_ambiguity() {
        let baseline = "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2Fryeos-bootstrap";
        for path in [
            "tmp/file",
            "/",
            "//tmp/file",
            "/tmp/../file",
            "/tmp//file",
            "/tmp/a b",
        ] {
            assert!(
                validate_proxy_route(
                    baseline,
                    "PUT",
                    OCCURRENCE,
                    REGION,
                    ProxyOperation::UploadFile { remote_path: path },
                )
                .is_err()
            );
        }
        for uri in [
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2Fother",
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2Fryeos-bootstrap&path=%2Ftmp%2Fother",
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=/tmp/ryeos-bootstrap",
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/%75pload?path=%2Ftmp%2Fryeos-bootstrap",
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2F%2e%2e%2Fryeos-bootstrap",
        ] {
            assert!(
                validate_proxy_route(
                    uri,
                    "PUT",
                    OCCURRENCE,
                    REGION,
                    ProxyOperation::UploadFile {
                        remote_path: "/tmp/ryeos-bootstrap"
                    },
                )
                .is_err()
            );
        }
        assert!(
            validate_proxy_route(
                "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream",
                "PUT",
                OCCURRENCE,
                REGION,
                ProxyOperation::UploadFile {
                    remote_path: "/tmp/ryeos-bootstrap"
                },
            )
            .is_err()
        );
    }
}
