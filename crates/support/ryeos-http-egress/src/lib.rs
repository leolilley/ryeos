//! Proxy settings for the HTTP clients RyeOS builds.
//!
//! `reqwest` reads `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` from the
//! environment and exempts only what `NO_PROXY` names. A container with an
//! egress proxy and a `NO_PROXY` that does not cover loopback therefore sent
//! every call to the node's own local daemon to the proxy, where it hung. A
//! call to loopback never leaves the machine, so no environment may route it
//! anywhere: every client RyeOS builds takes its proxies from here, with
//! `localhost`, `127.0.0.0/8` and `::1` exempt first and the operator's own
//! `NO_PROXY` entries kept after them.
//!
//! A client whose endpoint is always local (a daemon socket's HTTP face, a
//! loopback app-server) uses [`loopback_client_builder`], which takes no
//! proxy at all.

use reqwest::{NoProxy, Proxy};

/// What is never proxied, whatever the environment says.
pub const LOOPBACK_EXEMPTIONS: &str = "localhost,127.0.0.0/8,::1";

/// The proxies the environment asks for, each exempting loopback and the
/// operator's own `NO_PROXY` entries. `lookup` reads one environment variable.
pub fn proxies_from(lookup: &dyn Fn(&str) -> Option<String>) -> Vec<Proxy> {
    let var = |upper: &str, lower: &str| {
        lookup(upper)
            .or_else(|| lookup(lower))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let exemptions = match var("NO_PROXY", "no_proxy") {
        Some(operator) => format!("{LOOPBACK_EXEMPTIONS},{operator}"),
        None => LOOPBACK_EXEMPTIONS.to_owned(),
    };
    let no_proxy = || NoProxy::from_string(&exemptions);
    let mut proxies = Vec::new();
    let mut add = |made: reqwest::Result<Proxy>| {
        if let Ok(proxy) = made {
            proxies.push(proxy.no_proxy(no_proxy()));
        }
    };
    if let Some(url) = var("HTTP_PROXY", "http_proxy") {
        add(Proxy::http(url));
    }
    if let Some(url) = var("HTTPS_PROXY", "https_proxy") {
        add(Proxy::https(url));
    }
    if let Some(url) = var("ALL_PROXY", "all_proxy") {
        add(Proxy::all(url));
    }
    proxies
}

/// The proxy host (`host` or `host:port`) that would carry a request to `url`
/// under the environment `lookup` reads, or `None` when it goes direct. It
/// follows the same variables and exemptions as [`proxies_from`]: the scheme's
/// own proxy, else `ALL_PROXY`; loopback and the operator's `NO_PROXY` entries
/// (`*`, an address, a CIDR range, or a domain that matches itself and its
/// subdomains) go direct.
pub fn proxy_for(url: &reqwest::Url, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let var = |upper: &str, lower: &str| {
        lookup(upper)
            .or_else(|| lookup(lower))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let proxy = match url.scheme() {
        "https" => var("HTTPS_PROXY", "https_proxy"),
        "http" => var("HTTP_PROXY", "http_proxy"),
        _ => None,
    }
    .or_else(|| var("ALL_PROXY", "all_proxy"))?;
    let host = url
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let exemptions = match var("NO_PROXY", "no_proxy") {
        Some(operator) => format!("{LOOPBACK_EXEMPTIONS},{operator}"),
        None => LOOPBACK_EXEMPTIONS.to_owned(),
    };
    if exemptions
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| no_proxy_matches(entry, host))
    {
        return None;
    }
    let with_scheme = if proxy.contains("://") {
        proxy
    } else {
        format!("http://{proxy}")
    };
    Some(match reqwest::Url::parse(&with_scheme) {
        Ok(parsed) => match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_owned(),
            _ => "an unparseable proxy address".to_owned(),
        },
        Err(_) => "an unparseable proxy address".to_owned(),
    })
}

fn no_proxy_matches(entry: &str, host: &str) -> bool {
    if entry == "*" {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if let Some((network, bits)) = entry.split_once('/') {
            return match (network.parse::<std::net::IpAddr>(), bits.parse::<u32>()) {
                (Ok(network), Ok(bits)) => cidr_contains(network, bits, ip),
                _ => false,
            };
        }
        return entry.parse::<std::net::IpAddr>().is_ok_and(|e| e == ip);
    }
    let entry = entry.trim_start_matches('.').to_ascii_lowercase();
    let host = host.to_ascii_lowercase();
    host == entry || host.ends_with(&format!(".{entry}"))
}

fn cidr_contains(network: std::net::IpAddr, bits: u32, ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr::{V4, V6};
    let (a, b, width) = match (network, ip) {
        (V4(n), V4(i)) => (u128::from(u32::from(n)), u128::from(u32::from(i)), 32),
        (V6(n), V6(i)) => (u128::from(n), u128::from(i), 128),
        _ => return false,
    };
    if bits > width {
        return false;
    }
    let mask = if bits == 0 {
        0
    } else {
        let ones = if bits == 128 {
            !0_u128
        } else {
            (1_u128 << bits) - 1
        };
        ones << (width - bits)
    };
    (a & mask) == (b & mask)
}

/// An async client builder with the proxies `lookup` asks for and loopback exempt.
pub fn client_builder_from(lookup: &dyn Fn(&str) -> Option<String>) -> reqwest::ClientBuilder {
    proxies_from(lookup)
        .into_iter()
        .fold(reqwest::Client::builder().no_proxy(), |builder, proxy| {
            builder.proxy(proxy)
        })
}

/// The proxies of this process's environment, loopback exempt.
pub fn proxies() -> Vec<Proxy> {
    proxies_from(&|name| std::env::var(name).ok())
}

/// An async client builder with the environment's proxies and loopback exempt.
pub fn client_builder() -> reqwest::ClientBuilder {
    proxies()
        .into_iter()
        .fold(reqwest::Client::builder().no_proxy(), |builder, proxy| {
            builder.proxy(proxy)
        })
}

/// An async client builder for an endpoint that is always local: no proxy.
pub fn loopback_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().no_proxy()
}

/// A blocking client builder with the environment's proxies and loopback exempt.
#[cfg(feature = "blocking")]
pub fn blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    proxies().into_iter().fold(
        reqwest::blocking::Client::builder().no_proxy(),
        |builder, proxy| builder.proxy(proxy),
    )
}

/// A blocking client builder for an endpoint that is always local: no proxy.
#[cfg(feature = "blocking")]
pub fn loopback_blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder().no_proxy()
}

#[cfg(test)]
mod tests {
    #[test]
    fn proxy_for_follows_the_scheme_variable_and_the_exemptions() {
        let url = |text: &str| reqwest::Url::parse(text).unwrap();
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        let none = vars(&[]);
        assert_eq!(proxy_for(&url("https://pull.example/x"), &none), None);
        let https = vars(&[("HTTPS_PROXY", "http://proxy.corp:3128")]);
        assert_eq!(
            proxy_for(&url("https://pull.example/x"), &https).as_deref(),
            Some("proxy.corp:3128")
        );
        assert_eq!(proxy_for(&url("http://pull.example/x"), &https), None);
        let all = vars(&[("ALL_PROXY", "proxy.corp")]);
        assert_eq!(
            proxy_for(&url("http://pull.example/x"), &all).as_deref(),
            Some("proxy.corp")
        );
        let exempt = vars(&[
            ("HTTPS_PROXY", "http://proxy.corp:3128"),
            ("NO_PROXY", "example.org, .pull.example,10.0.0.0/8"),
        ]);
        for direct in [
            "https://pull.example/x",
            "https://a.pull.example/x",
            "https://example.org/x",
            "https://10.1.2.3/x",
            "https://localhost/x",
            "https://127.0.0.1/x",
        ] {
            assert_eq!(proxy_for(&url(direct), &exempt), None, "{direct}");
        }
        assert!(proxy_for(&url("https://other.example/x"), &exempt).is_some());
        assert!(proxy_for(&url("https://11.0.0.1/x"), &exempt).is_some());
        let star = vars(&[("HTTPS_PROXY", "proxy.corp"), ("NO_PROXY", "*")]);
        assert_eq!(proxy_for(&url("https://any.example/x"), &star), None);
    }

    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A one-shot HTTP server: answers every request `200 ok` and sends each
    /// request line down `seen`.
    async fn serve(listener: TcpListener, seen: tokio::sync::mpsc::UnboundedSender<String>) {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let _ = seen.send(text.lines().next().unwrap_or_default().to_owned());
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    }

    fn env<'a>(pairs: &'a [(&'a str, String)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn client(proxies: Vec<Proxy>) -> reqwest::Client {
        proxies
            .into_iter()
            .fold(reqwest::Client::builder().no_proxy(), |builder, proxy| {
                builder.proxy(proxy)
            })
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    /// A port nothing listens on.
    async fn dead_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn a_dead_proxy_in_the_environment_does_not_stop_a_loopback_call() {
        let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = local.local_addr().unwrap().port();
        let (sender, _seen) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(serve(local, sender));
        let dead = format!("http://127.0.0.1:{}", dead_port().await);
        let pairs = [
            ("HTTP_PROXY", dead.clone()),
            ("HTTPS_PROXY", dead.clone()),
            ("ALL_PROXY", dead),
        ];
        // NO_PROXY is empty: nothing exempts loopback but this crate.
        let body = client(proxies_from(&env(&pairs)))
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .expect("a loopback call never uses the proxy")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");
        let body = client(proxies_from(&env(&pairs)))
            .get(format!("http://localhost:{port}/health"))
            .send()
            .await
            .expect("localhost is exempt too")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");

        // The control: the same environment through plain reqwest proxies does
        // route the call to the dead proxy and fail, so the test would notice
        // the exemption going missing.
        let plain = client(vec![
            Proxy::http(format!("http://127.0.0.1:{}", dead_port().await)).unwrap(),
        ]);
        assert!(
            plain
                .get(format!("http://127.0.0.1:{port}/health"))
                .send()
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_call_off_the_machine_still_uses_the_proxy() {
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        let (sender, mut seen) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(serve(proxy, sender));
        let pairs = [("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}"))];
        let response = client(proxies_from(&env(&pairs)))
            .get("http://203.0.113.9/path")
            .send()
            .await
            .expect("the proxy answers");
        assert_eq!(response.text().await.unwrap(), "ok");
        let line = seen.recv().await.unwrap();
        assert_eq!(line, "GET http://203.0.113.9/path HTTP/1.1");
    }

    #[tokio::test]
    async fn the_operators_no_proxy_entries_are_kept_beside_loopback() {
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        let (sender, mut seen) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(serve(proxy, sender));
        let pairs = [
            ("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}")),
            ("NO_PROXY", "203.0.113.0/24".to_owned()),
        ];
        // The operator exempted this range: the call goes direct (to a
        // black-hole address, so it fails to connect) and the proxy sees none.
        let result = client(proxies_from(&env(&pairs)))
            .get("http://203.0.113.9/path")
            .send()
            .await;
        assert!(result.is_err());
        assert!(seen.try_recv().is_err(), "the proxy must not see the call");
    }

    #[test]
    fn an_environment_without_proxies_gives_none() {
        assert!(proxies_from(&|_| None).is_empty());
        assert!(proxies_from(&|_| Some("  ".to_owned())).is_empty());
    }
}
