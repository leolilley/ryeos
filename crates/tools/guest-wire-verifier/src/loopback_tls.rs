//! A tiny in-process HTTPS (HTTP/1.1 over TLS 1.3) server bound to 127.0.0.1
//! with an ephemeral, hand-built Ed25519 PKI. A client that trusts only the
//! generated root certificate can verify the server by IP address.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use lillux::crypto::{Signer, SigningKey, generate_random_bytes, generate_signing_key};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const HEAD_LIMIT: usize = 16 * 1024;
const CONN_TIMEOUT: Duration = Duration::from_secs(10);
const ACCEPT_POLL: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// DER helpers
// ---------------------------------------------------------------------------

const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
const OID_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
const OID_EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];
const OID_SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];

/// Encode one definite-length TLV.
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let len = content.len();
    let mut out = Vec::with_capacity(len + 6);
    out.push(tag);
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = (len as u64).to_be_bytes();
        let first = bytes
            .iter()
            .position(|b| *b != 0)
            .unwrap_or(bytes.len() - 1);
        let sig = &bytes[first..];
        out.push(0x80 | sig.len() as u8);
        out.extend_from_slice(sig);
    }
    out.extend_from_slice(content);
    out
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &concat(parts))
}

fn oid(content: &[u8]) -> Vec<u8> {
    tlv(0x06, content)
}

fn bit_string(unused_bits: u8, content: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(content.len() + 1);
    body.push(unused_bits);
    body.extend_from_slice(content);
    tlv(0x03, &body)
}

/// Minimal-length DER INTEGER for an unsigned big-endian value.
fn unsigned_integer(be: &[u8]) -> Vec<u8> {
    let first = be.iter().position(|b| *b != 0).unwrap_or(be.len());
    let trimmed = &be[first..];
    let mut body = Vec::with_capacity(trimmed.len() + 1);
    if trimmed.is_empty() {
        body.push(0);
    } else {
        if trimmed[0] & 0x80 != 0 {
            body.push(0);
        }
        body.extend_from_slice(trimmed);
    }
    tlv(0x02, &body)
}

fn alg_id_ed25519() -> Vec<u8> {
    seq(&[&oid(OID_ED25519)])
}

fn name_cn(cn: &str) -> Vec<u8> {
    let atv = seq(&[&oid(OID_CN), &tlv(0x0c, cn.as_bytes())]);
    let rdn = tlv(0x31, &atv);
    seq(&[&rdn])
}

fn extension(oid_content: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let octets = tlv(0x04, value);
    if critical {
        seq(&[&oid(oid_content), &tlv(0x01, &[0xff]), &octets])
    } else {
        seq(&[&oid(oid_content), &octets])
    }
}

fn spki_ed25519(public: &[u8; 32]) -> Vec<u8> {
    seq(&[&alg_id_ed25519(), &bit_string(0, public)])
}

fn random_serial() -> Vec<u8> {
    let mut raw = generate_random_bytes::<16>();
    raw[0] &= 0x7f;
    if raw[0] == 0 {
        raw[0] = 1;
    }
    unsigned_integer(&raw)
}

struct CertSpec<'a> {
    subject_cn: &'a str,
    issuer_cn: &'a str,
    subject_public: [u8; 32],
    extensions: Vec<Vec<u8>>,
}

/// Build and sign a v3 certificate; `issuer_key` signs the TBS.
fn build_certificate(spec: &CertSpec<'_>, issuer_key: &SigningKey) -> Vec<u8> {
    let version = tlv(0xa0, &tlv(0x02, &[2]));
    let validity = seq(&[&tlv(0x17, b"240101000000Z"), &tlv(0x17, b"491231235959Z")]);
    let ext_refs: Vec<&[u8]> = spec.extensions.iter().map(|e| e.as_slice()).collect();
    let extensions = tlv(0xa3, &seq(&ext_refs));
    let tbs = seq(&[
        &version,
        &random_serial(),
        &alg_id_ed25519(),
        &name_cn(spec.issuer_cn),
        &validity,
        &name_cn(spec.subject_cn),
        &spki_ed25519(&spec.subject_public),
        &extensions,
    ]);
    let signature = issuer_key.sign(&tbs).to_bytes();
    seq(&[&tbs, &alg_id_ed25519(), &bit_string(0, &signature)])
}

// ---------------------------------------------------------------------------
// Ephemeral PKI
// ---------------------------------------------------------------------------

/// A self-signed root plus a loopback server leaf, generated in memory.
pub struct EphemeralPki {
    root_der: Vec<u8>,
    leaf_der: Vec<u8>,
    leaf_key_pkcs8: Vec<u8>,
}

impl EphemeralPki {
    pub fn generate() -> anyhow::Result<Self> {
        let root_key = generate_signing_key();
        let leaf_key = generate_signing_key();

        let root_spec = CertSpec {
            subject_cn: "ryeos guest-wire root",
            issuer_cn: "ryeos guest-wire root",
            subject_public: root_key.verifying_key().to_bytes(),
            extensions: vec![
                extension(OID_BASIC_CONSTRAINTS, true, &seq(&[&tlv(0x01, &[0xff])])),
                // keyCertSign (bit 5) | cRLSign (bit 6): 0b0000_0110, one unused bit.
                extension(OID_KEY_USAGE, true, &bit_string(1, &[0x06])),
            ],
        };
        let root_der = build_certificate(&root_spec, &root_key);

        let leaf_spec = CertSpec {
            subject_cn: "ryeos guest-wire loopback",
            issuer_cn: "ryeos guest-wire root",
            subject_public: leaf_key.verifying_key().to_bytes(),
            extensions: vec![
                // cA defaults to FALSE, so DER encodes an empty sequence.
                extension(OID_BASIC_CONSTRAINTS, false, &seq(&[])),
                // digitalSignature (bit 0): 0b1000_0000, seven unused bits.
                extension(OID_KEY_USAGE, true, &bit_string(7, &[0x80])),
                extension(OID_EXT_KEY_USAGE, false, &seq(&[&oid(OID_SERVER_AUTH)])),
                // One iPAddress general name (context tag 7): 127.0.0.1.
                extension(OID_SAN, false, &seq(&[&tlv(0x87, &[127, 0, 0, 1])])),
            ],
        };
        let leaf_der = build_certificate(&leaf_spec, &root_key);
        // The root signing key is dropped here, after it has signed the leaf.
        drop(root_key);

        // PKCS#8 v1 (no embedded public key): the ring provider loads Ed25519
        // keys through a loader that accepts both v1 and v2 forms and derives
        // the public key from the seed.
        let mut pkcs8 = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        pkcs8.extend_from_slice(&leaf_key.to_bytes());

        Ok(Self {
            root_der,
            leaf_der,
            leaf_key_pkcs8: pkcs8,
        })
    }

    /// DER of the self-signed root certificate (the only trust anchor a client needs).
    pub fn root_der(&self) -> &[u8] {
        &self.root_der
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

pub struct HttpsRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

pub struct LoopbackHttpsServer {
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    port: u16,
}

enum ReadOutcome {
    Request(HttpsRequest),
    Reject(u16, &'static str),
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Read and validate one request. `Err` means the connection failed mid-request.
fn read_request<S: Read>(stream: &mut S, max_body_bytes: usize) -> std::io::Result<ReadOutcome> {
    let mut acc: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find_head_end(&acc) {
            break pos;
        }
        if acc.len() > HEAD_LIMIT {
            return Ok(ReadOutcome::Reject(400, "request head too large"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        acc.extend_from_slice(&chunk[..n]);
    };
    if head_end > HEAD_LIMIT {
        return Ok(ReadOutcome::Reject(400, "request head too large"));
    }
    let head_bytes = &acc[..head_end];
    if !head_bytes.is_ascii() {
        return Ok(ReadOutcome::Reject(400, "non-ascii request head"));
    }
    let head = match std::str::from_utf8(head_bytes) {
        Ok(h) => h,
        Err(_) => return Ok(ReadOutcome::Reject(400, "non-ascii request head")),
    };
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let parts: Vec<&str> = request_line.split(' ').collect();
    if parts.len() != 3 {
        return Ok(ReadOutcome::Reject(400, "malformed request line"));
    }
    let (method, path, version) = (parts[0], parts[1], parts[2]);
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Ok(ReadOutcome::Reject(400, "malformed method"));
    }
    if path.is_empty() || !path.starts_with('/') {
        return Ok(ReadOutcome::Reject(400, "malformed path"));
    }
    if version != "HTTP/1.1" {
        return Ok(ReadOutcome::Reject(400, "HTTP/1.1 required"));
    }

    let mut content_length: Option<u64> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Ok(ReadOutcome::Reject(400, "malformed header line"));
        };
        if name.is_empty()
            || name
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Ok(ReadOutcome::Reject(400, "malformed header name"));
        }
        let lname = name.to_ascii_lowercase();
        let value = value.trim();
        match lname.as_str() {
            "transfer-encoding" => {
                return Ok(ReadOutcome::Reject(400, "transfer-encoding not supported"));
            }
            "expect" => return Ok(ReadOutcome::Reject(400, "expect not supported")),
            "content-length" => {
                if content_length.is_some() {
                    return Ok(ReadOutcome::Reject(400, "duplicate content-length"));
                }
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Ok(ReadOutcome::Reject(400, "malformed content-length"));
                }
                match value.parse::<u64>() {
                    Ok(v) => content_length = Some(v),
                    Err(_) => return Ok(ReadOutcome::Reject(400, "malformed content-length")),
                }
            }
            _ => {}
        }
    }

    if method == "POST" && content_length.is_none() {
        return Ok(ReadOutcome::Reject(400, "content-length required"));
    }
    let need = content_length.unwrap_or(0);
    if need > max_body_bytes as u64 {
        return Ok(ReadOutcome::Reject(413, "body too large"));
    }
    let need = need as usize;

    let mut body: Vec<u8> = acc[head_end + 4..].to_vec();
    if body.len() > need {
        return Ok(ReadOutcome::Reject(400, "unexpected bytes after body"));
    }
    let have = body.len();
    body.resize(need, 0);
    stream.read_exact(&mut body[have..])?;

    Ok(ReadOutcome::Request(HttpsRequest {
        method: method.to_string(),
        path: path.to_string(),
        body,
    }))
}

fn write_reply<S: Write>(stream: &mut S, status: u16, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        status,
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn reject_body(message: &str) -> Vec<u8> {
    serde_json::json!({ "error": message })
        .to_string()
        .into_bytes()
}

type TlsStream = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

fn finish(mut stream: TlsStream) {
    stream.conn.send_close_notify();
    // A failed flush only means the peer is gone; nothing more to do.
    let _ = stream.flush();
}

impl LoopbackHttpsServer {
    pub fn bind(pki: &EphemeralPki) -> anyhow::Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .context("select TLS 1.3")?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(pki.leaf_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pki.leaf_key_pkcs8.clone())),
            )
            .context("install loopback leaf certificate")?;
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("bind loopback listener")?;
        listener
            .set_nonblocking(true)
            .context("set listener non-blocking")?;
        let port = listener.local_addr().context("listener address")?.port();
        Ok(Self {
            listener,
            config: Arc::new(config),
            port,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Serve connections strictly one at a time until `deadline` elapses, `stop`
    /// becomes true, or `handler` returns an error (which is returned; the
    /// triggering connection is closed without a reply). Connection-level
    /// failures are dropped and serving continues.
    pub fn serve<H>(
        &self,
        deadline: Instant,
        stop: &AtomicBool,
        max_body_bytes: usize,
        mut handler: H,
    ) -> anyhow::Result<()>
    where
        H: FnMut(HttpsRequest) -> anyhow::Result<HttpReply>,
    {
        loop {
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Ok(());
            }
            let tcp = match self.listener.accept() {
                Ok((tcp, _)) => tcp,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(anyhow::Error::new(e).context("accept loopback connection")),
            };
            if tcp.set_nonblocking(false).is_err()
                || tcp.set_read_timeout(Some(CONN_TIMEOUT)).is_err()
                || tcp.set_write_timeout(Some(CONN_TIMEOUT)).is_err()
            {
                continue;
            }
            let Ok(conn) = rustls::ServerConnection::new(Arc::clone(&self.config)) else {
                continue;
            };
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            match read_request(&mut stream, max_body_bytes) {
                Err(_) => continue,
                Ok(ReadOutcome::Reject(status, message)) => {
                    if write_reply(&mut stream, status, &reject_body(message)).is_ok() {
                        finish(stream);
                    }
                }
                Ok(ReadOutcome::Request(request)) => {
                    let reply = handler(request)?;
                    if write_reply(&mut stream, reply.status, &reply.body).is_ok() {
                        finish(stream);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn client_config(root: &[u8]) -> anyhow::Result<Arc<rustls::ClientConfig>> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(root.to_vec()))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Arc::new(config))
    }

    fn exchange(root: &[u8], port: u16, request: &[u8]) -> anyhow::Result<Vec<u8>> {
        let config = client_config(root)?;
        let name = rustls::pki_types::ServerName::IpAddress(rustls::pki_types::IpAddr::from(
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
        ));
        let conn = rustls::ClientConnection::new(config, name)?;
        let tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        tls.write_all(request)?;
        tls.flush()?;
        let mut out = Vec::new();
        match tls.read_to_end(&mut out) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !out.is_empty() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(out)
    }

    type ServerJoin = std::thread::JoinHandle<anyhow::Result<()>>;

    fn spawn_server(
        server: Arc<LoopbackHttpsServer>,
        stop: Arc<AtomicBool>,
        max_body: usize,
        calls: Arc<AtomicUsize>,
    ) -> ServerJoin {
        std::thread::spawn(move || {
            server.serve(
                Instant::now() + Duration::from_secs(60),
                &stop,
                max_body,
                |req| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let body = serde_json::json!({
                        "path": req.path,
                        "len": req.body.len(),
                    })
                    .to_string()
                    .into_bytes();
                    Ok(HttpReply { status: 200, body })
                },
            )
        })
    }

    fn split_response(raw: &[u8]) -> (String, Vec<u8>) {
        let pos = find_head_end(raw).unwrap_or(raw.len());
        let head = String::from_utf8_lossy(&raw[..pos]).to_string();
        let body = raw.get(pos + 4..).unwrap_or(&[]).to_vec();
        (head, body)
    }

    #[test]
    fn generate_yields_distinct_roots() -> anyhow::Result<()> {
        let a = EphemeralPki::generate()?;
        let b = EphemeralPki::generate()?;
        assert_ne!(a.root_der(), b.root_der());
        Ok(())
    }

    #[test]
    fn round_trip_over_tls() -> anyhow::Result<()> {
        let pki = EphemeralPki::generate()?;
        let server = Arc::new(LoopbackHttpsServer::bind(&pki)?);
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let join = spawn_server(
            Arc::clone(&server),
            Arc::clone(&stop),
            1024,
            Arc::clone(&calls),
        );

        let payload = b"hello world";
        let mut request = format!(
            "POST /x HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        request.extend_from_slice(payload);
        let result = exchange(pki.root_der(), server.port(), &request);

        stop.store(true, Ordering::Release);
        let served = join
            .join()
            .map_err(|_| anyhow::anyhow!("server thread panicked"))?;
        served?;
        let raw = result?;

        let (head, body) = split_response(&raw);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "head: {head}");
        let value: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(value["path"], "/x");
        assert_eq!(value["len"], payload.len());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn client_with_different_root_fails_handshake() -> anyhow::Result<()> {
        let pki = EphemeralPki::generate()?;
        let other = EphemeralPki::generate()?;
        let server = Arc::new(LoopbackHttpsServer::bind(&pki)?);
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let join = spawn_server(
            Arc::clone(&server),
            Arc::clone(&stop),
            1024,
            Arc::clone(&calls),
        );

        let request = b"POST /x HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let result = exchange(other.root_der(), server.port(), request);

        stop.store(true, Ordering::Release);
        let served = join
            .join()
            .map_err(|_| anyhow::anyhow!("server thread panicked"))?;
        served?;
        assert!(result.is_err(), "handshake with foreign root must fail");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn oversize_content_length_is_rejected_without_handler() -> anyhow::Result<()> {
        let pki = EphemeralPki::generate()?;
        let server = Arc::new(LoopbackHttpsServer::bind(&pki)?);
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let join = spawn_server(
            Arc::clone(&server),
            Arc::clone(&stop),
            1024,
            Arc::clone(&calls),
        );

        let request = b"POST /x HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-length: 1000000\r\nconnection: close\r\n\r\n";
        let result = exchange(pki.root_der(), server.port(), request);

        stop.store(true, Ordering::Release);
        let served = join
            .join()
            .map_err(|_| anyhow::anyhow!("server thread panicked"))?;
        served?;
        let raw = result?;

        let (head, _body) = split_response(&raw);
        assert!(
            head.starts_with("HTTP/1.1 413") || head.starts_with("HTTP/1.1 400"),
            "head: {head}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        Ok(())
    }
}
