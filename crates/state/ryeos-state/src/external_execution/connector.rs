//! Bounded local transport between a trusted controller-side connector and
//! RyeOS's durable external-execution owner.
//!
//! This is deliberately not the occurrence transport. The connector has no
//! cloud, node, signing, publication, or candidate authority. It authenticates
//! one local process to one already-admitted execution and relays only the
//! exec-server byte stream. A dropped connection is terminal uncertainty; a
//! new connection never gains replay authority for input already accepted.

use std::io::{Read, Write};

use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::{Zeroize as _, Zeroizing};

use super::{MAX_CHUNK_BYTES, hash};

pub const EXTERNAL_CONNECTOR_PROTOCOL: &str = "ryeos.external-candidate.connector.v1";
pub const MAX_CONNECTOR_WIRE_BYTES: usize = 384 * 1024;

/// Secret first message on a fresh owner-private local connection. This type
/// intentionally has no `Debug` implementation.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalConnectorHello {
    pub schema: u32,
    pub protocol: String,
    pub placement_thread_id: String,
    pub execution_binding_hash: String,
    capability: String,
}

impl ExternalConnectorHello {
    pub fn new(
        placement_thread_id: String,
        execution_binding_hash: String,
        capability: String,
    ) -> Result<Self> {
        let hello = Self {
            schema: 1,
            protocol: EXTERNAL_CONNECTOR_PROTOCOL.into(),
            placement_thread_id,
            execution_binding_hash,
            capability,
        };
        hello.validate()?;
        Ok(hello)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.protocol == EXTERNAL_CONNECTOR_PROTOCOL,
            "unsupported external connector protocol"
        );
        bounded_text(
            "external connector placement",
            &self.placement_thread_id,
            256,
        )?;
        hash(&self.execution_binding_hash)?;
        decode_capability(&self.capability)?;
        Ok(())
    }

    pub fn capability_hash(&self) -> Result<String> {
        let capability = decode_capability(&self.capability)?;
        Ok(lillux::sha256_hex(capability.as_slice()))
    }
}

impl Drop for ExternalConnectorHello {
    fn drop(&mut self) {
        self.capability.zeroize();
    }
}

/// Connector-to-controller messages after authentication. Local sequence is
/// process-local and permits exactly one outstanding input acknowledgement.
/// It is not durable replay authority after disconnect.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalConnectorClientFrame {
    ProtocolBytes {
        local_sequence: u64,
        bytes_base64: String,
    },
    InputClosed {
        local_sequence: u64,
    },
}

impl ExternalConnectorClientFrame {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::ProtocolBytes {
                local_sequence,
                bytes_base64,
            } => {
                ensure!(
                    *local_sequence > 0,
                    "external connector input sequence is zero"
                );
                decode_chunk(bytes_base64, false)?;
            }
            Self::InputClosed { local_sequence } => {
                ensure!(
                    *local_sequence > 0,
                    "external connector input sequence is zero"
                );
            }
        }
        Ok(())
    }

    pub fn local_sequence(&self) -> u64 {
        match self {
            Self::ProtocolBytes { local_sequence, .. } | Self::InputClosed { local_sequence } => {
                *local_sequence
            }
        }
    }

    pub fn protocol_bytes(&self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::ProtocolBytes { bytes_base64, .. } => {
                Ok(Some(decode_chunk(bytes_base64, false)?))
            }
            Self::InputClosed { .. } => Ok(None),
        }
    }
}

/// Controller-to-connector messages. Faults are closed classifications rather
/// than internal error strings, so durable authority and secrets cannot leak
/// into provider-visible stderr.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalConnectorServerFrame {
    Ready {
        placement_thread_id: String,
        execution_binding_hash: String,
    },
    InputApplied {
        local_sequence: u64,
        remote_sequence: u64,
        remote_frame_digest: String,
    },
    ProtocolBytes {
        remote_sequence: u64,
        remote_frame_digest: String,
        bytes_base64: String,
    },
    ProtocolEof {
        remote_sequence: u64,
        remote_frame_digest: String,
    },
    Fault {
        code: ExternalConnectorFault,
    },
}

impl ExternalConnectorServerFrame {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Ready {
                placement_thread_id,
                execution_binding_hash,
            } => {
                bounded_text("external connector placement", placement_thread_id, 256)?;
                hash(execution_binding_hash)?;
            }
            Self::InputApplied {
                local_sequence,
                remote_sequence,
                remote_frame_digest,
            } => {
                ensure!(
                    *local_sequence > 0 && *remote_sequence > 0,
                    "external connector acknowledgement has a zero sequence"
                );
                hash(remote_frame_digest)?;
            }
            Self::ProtocolBytes {
                remote_sequence,
                remote_frame_digest,
                bytes_base64,
            } => {
                ensure!(
                    *remote_sequence > 0,
                    "external connector output sequence is zero"
                );
                hash(remote_frame_digest)?;
                decode_chunk(bytes_base64, false)?;
            }
            Self::ProtocolEof {
                remote_sequence,
                remote_frame_digest,
            } => {
                ensure!(
                    *remote_sequence > 0,
                    "external connector EOF sequence is zero"
                );
                hash(remote_frame_digest)?;
            }
            Self::Fault { .. } => {}
        }
        Ok(())
    }

    pub fn protocol_bytes(&self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::ProtocolBytes { bytes_base64, .. } => {
                Ok(Some(decode_chunk(bytes_base64, false)?))
            }
            _ => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalConnectorFault {
    AuthenticationRefused,
    AuthorityExpired,
    InputUncertain,
    OutputUncertain,
    ExecutionRevoked,
    ProtocolViolation,
    ControllerUnavailable,
}

pub fn read_external_connector_hello(reader: &mut impl Read) -> Result<ExternalConnectorHello> {
    let hello: ExternalConnectorHello = read_canonical_frame(reader)?;
    hello.validate()?;
    Ok(hello)
}

pub fn write_external_connector_hello(
    writer: &mut impl Write,
    hello: &ExternalConnectorHello,
) -> Result<()> {
    hello.validate()?;
    write_canonical_frame(writer, hello)
}

pub fn read_external_connector_client_frame(
    reader: &mut impl Read,
) -> Result<ExternalConnectorClientFrame> {
    let frame: ExternalConnectorClientFrame = read_canonical_frame(reader)?;
    frame.validate()?;
    Ok(frame)
}

pub fn write_external_connector_client_frame(
    writer: &mut impl Write,
    frame: &ExternalConnectorClientFrame,
) -> Result<()> {
    frame.validate()?;
    write_canonical_frame(writer, frame)
}

pub fn read_external_connector_server_frame(
    reader: &mut impl Read,
) -> Result<ExternalConnectorServerFrame> {
    let frame: ExternalConnectorServerFrame = read_canonical_frame(reader)?;
    frame.validate()?;
    Ok(frame)
}

pub fn write_external_connector_server_frame(
    writer: &mut impl Write,
    frame: &ExternalConnectorServerFrame,
) -> Result<()> {
    frame.validate()?;
    write_canonical_frame(writer, frame)
}

fn write_canonical_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let body = lillux::canonical_json(&serde_json::to_value(value)?)?;
    ensure!(
        !body.is_empty() && body.len() <= MAX_CONNECTOR_WIRE_BYTES,
        "external connector frame exceeds its wire bound"
    );
    writer.write_all(&u32::try_from(body.len())?.to_be_bytes())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn read_canonical_frame<T: DeserializeOwned + Serialize>(reader: &mut impl Read) -> Result<T> {
    let mut length = [0_u8; 4];
    reader
        .read_exact(&mut length)
        .context("read external connector frame length")?;
    let length = usize::try_from(u32::from_be_bytes(length))?;
    ensure!(
        (1..=MAX_CONNECTOR_WIRE_BYTES).contains(&length),
        "external connector frame length exceeds its wire bound"
    );
    let mut body = vec![0_u8; length];
    reader
        .read_exact(&mut body)
        .context("read external connector frame body")?;
    let value: T = serde_json::from_slice(&body).context("decode external connector frame")?;
    ensure!(
        lillux::canonical_json(&serde_json::to_value(&value)?)?.as_bytes() == body,
        "external connector frame is not canonical"
    );
    Ok(value)
}

fn decode_capability(value: &str) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        value.len() == 44,
        "external connector capability has the wrong length"
    );
    let bytes = Zeroizing::new(
        STANDARD
            .decode(value)
            .context("decode external connector capability")?,
    );
    ensure!(
        bytes.len() == 32 && STANDARD.encode(bytes.as_slice()) == value,
        "external connector capability is not canonical"
    );
    Ok(bytes)
}

fn decode_chunk(value: &str, allow_empty: bool) -> Result<Vec<u8>> {
    ensure!(
        value.len() <= MAX_CHUNK_BYTES.div_ceil(3) * 4,
        "external connector chunk exceeds its bound"
    );
    let bytes = STANDARD
        .decode(value)
        .context("decode external connector chunk")?;
    ensure!(
        bytes.len() <= MAX_CHUNK_BYTES
            && (allow_empty || !bytes.is_empty())
            && STANDARD.encode(&bytes) == value,
        "external connector chunk is empty, oversized, or noncanonical"
    );
    Ok(bytes)
}

fn bounded_text(label: &str, value: &str, maximum: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        bail!("{label} is not canonical");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> ExternalConnectorHello {
        ExternalConnectorHello::new(
            "T-connector".into(),
            "a".repeat(64),
            STANDARD.encode([7_u8; 32]),
        )
        .unwrap()
    }

    #[test]
    fn hello_and_role_frames_roundtrip_canonically() {
        let hello = hello();
        let mut wire = Vec::new();
        write_external_connector_hello(&mut wire, &hello).unwrap();
        assert!(read_external_connector_hello(&mut wire.as_slice()).unwrap() == hello);

        let client = ExternalConnectorClientFrame::ProtocolBytes {
            local_sequence: 1,
            bytes_base64: STANDARD.encode(b"request\n"),
        };
        let mut wire = Vec::new();
        write_external_connector_client_frame(&mut wire, &client).unwrap();
        let decoded = read_external_connector_client_frame(&mut wire.as_slice()).unwrap();
        assert!(decoded == client);
        assert_eq!(decoded.protocol_bytes().unwrap().unwrap(), b"request\n");

        let server = ExternalConnectorServerFrame::ProtocolBytes {
            remote_sequence: 7,
            remote_frame_digest: "b".repeat(64),
            bytes_base64: STANDARD.encode(b"response\n"),
        };
        let mut wire = Vec::new();
        write_external_connector_server_frame(&mut wire, &server).unwrap();
        let decoded = read_external_connector_server_frame(&mut wire.as_slice()).unwrap();
        assert!(decoded == server);
        assert_eq!(decoded.protocol_bytes().unwrap().unwrap(), b"response\n");
    }

    #[test]
    fn secret_and_chunk_bounds_fail_before_framing() {
        let mut bad = hello();
        bad.capability = STANDARD.encode([0_u8; 31]);
        assert!(write_external_connector_hello(&mut Vec::new(), &bad).is_err());
        for bytes in [Vec::new(), vec![0; MAX_CHUNK_BYTES + 1]] {
            let frame = ExternalConnectorClientFrame::ProtocolBytes {
                local_sequence: 1,
                bytes_base64: STANDARD.encode(bytes),
            };
            let mut wire = Vec::new();
            assert!(write_external_connector_client_frame(&mut wire, &frame).is_err());
            assert!(wire.is_empty());
        }
        let exact = ExternalConnectorClientFrame::ProtocolBytes {
            local_sequence: 1,
            bytes_base64: STANDARD.encode(vec![0; MAX_CHUNK_BYTES]),
        };
        write_external_connector_client_frame(&mut Vec::new(), &exact).unwrap();
    }

    #[test]
    fn decoder_refuses_noncanonical_unknown_and_truncated_frames() {
        let body = serde_json::to_vec_pretty(&hello()).unwrap();
        let mut wire = Vec::new();
        wire.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
        wire.extend_from_slice(&body);
        assert!(read_external_connector_hello(&mut wire.as_slice()).is_err());

        let body = br#"{"capability":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=","execution_binding_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","extra":true,"placement_thread_id":"T-connector","protocol":"ryeos.external-candidate.connector.v1","schema":1}"#;
        let mut wire = Vec::new();
        wire.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
        wire.extend_from_slice(body);
        assert!(read_external_connector_hello(&mut wire.as_slice()).is_err());

        let mut wire = Vec::new();
        wire.extend_from_slice(&10_u32.to_be_bytes());
        wire.extend_from_slice(b"short");
        assert!(read_external_connector_hello(&mut wire.as_slice()).is_err());
    }
}
