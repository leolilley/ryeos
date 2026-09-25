//! HTTP transport owned by the protected external-candidate supervisor.
//!
//! The client accepts only a node-authorized HTTPS origin and explicit root
//! bundle from the sealed supervisor bootstrap. It never uses ambient proxy,
//! CA, credential, redirect, or endpoint configuration.

pub mod entrypoint;
mod http_transport;
pub mod runtime;
mod tls;

use anyhow::{Context as _, Result, ensure};
use http_transport::{BoundedHttpClient, HttpResponse};
use lillux::time::Duration;
use ryeos_external_execution::transport::{
    ExternalExecutionChannelTransport, ExternalTransportExchange, ExternalTransportFrame,
    ExternalTransportStepFailure,
};
use ryeos_state::external_execution::ExecutionChannelBinding;
use ryeos_state::external_execution::transport::{
    ExternalChannelAttachRequest, ExternalChannelAttachResponse, ExternalChannelExchangeRequest,
    ExternalChannelExchangeResponse, ExternalChannelResponseFrame, ExternalSupervisorBootstrap,
};

const ATTACH_RESPONSE_BYTES: u64 = 64 * 1024;

/// Stable certificate material shared only by repository-composed transport
/// tests. These identities are not installed, trusted, or reachable in a
/// production build unless the explicit `test-support` feature is selected.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    pub const TEST_CA_DER_BASE64: &str = "MIIDETCCAfmgAwIBAgIUX+scmKJ6HD/VzI8cSkNb1CDQYYMwDQYJKoZIhvcNAQELBQAwGDEWMBQGA1UEAwwNUnllT1MgVGVzdCBDQTAeFw0yNjA5MjAxMzQ1NDVaFw0zNjA5MTcxMzQ1NDVaMBgxFjAUBgNVBAMMDVJ5ZU9TIFRlc3QgQ0EwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQC+O+75C261AEfbAhLd1BO0VJmKyR4jk6bDQ1EU3druPMqf6qfvcfFye+FqR2mTyjRw0Lzw+WpEfpUT2Vo7qQVbSMnsaw9do1OK+gI2A+L2bWLH2uCN4nLULEbN91COVmnzY19sQz1esCDkGAza8RbgZjXOabK7Nil7R1HyFtlWj96eik5OEGjEpdAJQpsT9hhJXslyMBmSTtccR3Zl5fL7hbBnI8aUG5EbgPg/SWrtCN2M25RmmFdPbBz5aspSfsv8G4LsqH6NaDKwTC0iliR79C/D+wCFKcnOZIvHCDhTgAcNi0I44W0J0MlAm96wXmmv2Im1UF6DagU5cH/avTuVAgMBAAGjUzBRMB0GA1UdDgQWBBQM8Xib0/5JOrMFvPu3AFMR4v+m/DAfBgNVHSMEGDAWgBQM8Xib0/5JOrMFvPu3AFMR4v+m/DAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQCaxmJjrKa4su45TmAYnZPDxRqvgDomTmO89BBPXnY5qHTUZ2bfu6o3vtm5tRywiQXpQkzYIEqYJbT2RndFxgwPigyUqKviA+URXSMX7C8dn01eUtqIe73XYnsBey57JjnRgYtERBytFCGFaaqrreT+Tf1ZV4mjrqqgTyEXxr5/L+TtyYq1D4b4dWSkyEuf8qPFP36RGkVxC0dDzhwXC5AiewkPgGTiQvzcWVB+FyYKmrVMdkR6o5+1Chw7IJiPZSm4/JLX3UQb+Wc/+Lc7lMx4APqExKlW0KLTLCmYZ6gnff6bva6DLcYbZuxu486fc7DPFK3hNqLzrjkaAq5MlFqH";
    pub const TEST_SERVER_DER_BASE64: &str = "MIIDJzCCAg+gAwIBAgIUO9YtUXKy4lBXHfmjbWP+VWuMp2IwDQYJKoZIhvcNAQELBQAwGDEWMBQGA1UEAwwNUnllT1MgVGVzdCBDQTAeFw0yNjA5MjAxMzQ1NDVaFw0zNjA5MTcxMzQ1NDVaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBALmtC3mDEpcyntYNCnHHF97p3C3gcKVTWHOaahFOrcylv4uLnIlkNS//HBGPvWrL4r+7JUGiMyHny5h8zzhXsfBB8AnhqqaYdnpBbthh6mEJmQ9xrB8x8Zxe/aShIDeVViIAa4mMSBok9nCdW6KZc/1LmteIG70ulpDDbl4zHWIp/vJU6rrQzJbzyVfGFwKCD6hwQfhp9RMOmogaC6CtRhsEDqTmjpRHnmXTKbxDXHd22LgxwWqqPhh7nhBpab90B6YF6krKsCwXBO6lW6IOSSS6zODXkVi0DEUCuWqOmSEbETp5DwIeQXCzZPBJMO3seYSMsO/4sWnbZ1gmjQdq9CkCAwEAAaNtMGswFAYDVR0RBA0wC4IJbG9jYWxob3N0MBMGA1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBTbuqZMwWmYDtA+W2h5EwZ8pEfnojAfBgNVHSMEGDAWgBQM8Xib0/5JOrMFvPu3AFMR4v+m/DANBgkqhkiG9w0BAQsFAAOCAQEAOA9Wly9zikvpml9I5pcBG9BuZwS3W9y5jk+nVbgEMHndcHxbBttRMw7EcvqHku8YkOIqnrGQm27CtomoH7RdzOjG1pxtDH4hrEpPWpP/PJpnbMNAvMHamABNQDQTQT8sIIJdDMtCIN2/sqaryAt2PTf7tdEZR4OApFD83UQ1Ba7Xauxct8aVsgaijdjnxnK+z3/5Czx+lwBl5mxTwfnYn8DBckjF0lRAKUcQ3A1Vo680zwg52hiGXvmCTHjWiYMK3suLtYdE0HKIp2xphEmpdQNdQW7VUOHq3xD+QSBfpbTkGRKpqyaXfN4sORg72d5J3pYPydwn8v2BBcbcSpkFmA==";
    pub const TEST_SERVER_KEY_DER_BASE64: &str = "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC5rQt5gxKXMp7WDQpxxxfe6dwt4HClU1hzmmoRTq3Mpb+Li5yJZDUv/xwRj71qy+K/uyVBojMh58uYfM84V7HwQfAJ4aqmmHZ6QW7YYephCZkPcawfMfGcXv2koSA3lVYiAGuJjEgaJPZwnVuimXP9S5rXiBu9LpaQw25eMx1iKf7yVOq60MyW88lXxhcCgg+ocEH4afUTDpqIGgugrUYbBA6k5o6UR55l0ym8Q1x3dti4McFqqj4Ye54QaWm/dAemBepKyrAsFwTupVuiDkkkuszg15FYtAxFArlqjpkhGxE6eQ8CHkFws2TwSTDt7HmEjLDv+LFp22dYJo0HavQpAgMBAAECggEAKPhbCNP4PS6pR7gW7uYsiT53HBRjJsfOQ6v17Z270eVc77C9uL9I0S9shR9/f1o/zWjBHstolvmrvhkELH2FQOt7yOJnol0P/4gCqnJookLY6ER/415E3ulC9JmtHzavi88l63Lt0f8H9e9y8d0EcAbHwvlAja0DAixtZRHIUQls57TbuVPfDRkGJ7q9fTaX3VcuOp57Uhv2K6nKPuqhLWpA++Tw3+VmGCx5yAPhmqKPUmYnytC1iW/adHi2TfF+O1SONegoEJtCPfN9xcugOz7FdLaGyGrLRAw2JDFK96JZMj6APvnwgUS3cJCXPfigl1SaGGOONQXx7AuWDUE0hQKBgQDlEwuzNAalh/31MVDbqk4d+k6lk/EARF/ezcecIZhukDFy3N6LI6n/oYakEJcDp9ZsXQFjw9Tf6g3dwxUIBdnv/Rk4D+aEiJoMAaIJdCcjNNJe0B3ZMW2rpDRQGuSQWXZ04Uhpu+bWozEqMNzqFr56/yHi/ikcdGsecZMvDalohwKBgQDPgB64+9R3coT4Dkuk466ES70IAuHUIp/JhmH67cLt9AXQzHDM4f9JYEkoz5AQigQyamwuHsdXT2Jn1eLN+9BdtoGjTGSRvBtltKtzA02O70VExhvItN6J3Fma47j9ha1FOPVnRUrqykzzAtWqMkNH3VNpW4U4OKdFRZPX5cPZzwKBgBeJo3QgbmZn2NJu5M4Na8VsyNP+pY7Pd8JfBpmmYhFKQ6p3w24slfUsVbdZ9QptHn03+UKVBrSTSiV1PB386+3a5dJ638bSenGtYUbzZmoZrVwMqmR8zbYLQ0zP1ph2eNN9qoEiy49WaWDacHilKaFdwc+fKf5AgBk6tlLpZnTVAoGAVUFP3jNiLZ248m51OA9wUd0IkvUUMmPzgQqc0UvFTp13kj2djyDAEjbkeEcn6xO5+7jsL9rnjoEIbp9bq8Rt7UMiaqTloVdHbndYBk5yHGtE66f2HHXsBXqqulAcXtYAxjNL6R14VZW/Hg2pGl/CcxGFxwEacGoemACpaQh3etMCgYA2fdOocmWnWgSTxGEf7ohIUnK2TTfpae39zl+tOVY74MKLvBycg5qogjoJgYWx490Segvk+z9HCGOxd9MJ0CXssHVf9wWrHwNKQ/IkcruT2LNWW9kxyisSSjzHQiq+t+pSDmaj80Y28fDlhWkyTJgYP3o+sOp1kWSBVfV0faeHYw==";
}

pub struct AttachedExternalExecutionChannel {
    client: BoundedHttpClient,
    exchange_url: url::Url,
    placement_thread_id: String,
    occurrence_id: String,
    maximum_response_bytes: u64,
    request_timeout: Duration,
}

pub fn attach_external_execution_channel(
    bootstrap: &ExternalSupervisorBootstrap,
    supervisor_signing_key: &lillux::crypto::SigningKey,
    network_inputs: &ryeos_state::external_execution::transport::ExternalCapturedNetworkInputs,
) -> Result<(ExecutionChannelBinding, AttachedExternalExecutionChannel)> {
    // The controller's durable registration transaction owns first-use expiry.
    // An identical retry must remain possible after a successful registration
    // whose HTTP response was lost, even when the bootstrap deadline has since
    // passed. The caller must preserve this exact key and request across retry.
    let request = bootstrap.attachment_request(supervisor_signing_key)?;
    attach_external_execution_channel_exact(
        bootstrap,
        supervisor_signing_key,
        &request,
        network_inputs,
    )
}

/// Retry one exact durably retained attachment request. This never regenerates
/// a supervisor key or changes request bytes after an ambiguous response.
pub fn attach_external_execution_channel_exact(
    bootstrap: &ExternalSupervisorBootstrap,
    supervisor_signing_key: &lillux::crypto::SigningKey,
    request: &ExternalChannelAttachRequest,
    network_inputs: &ryeos_state::external_execution::transport::ExternalCapturedNetworkInputs,
) -> Result<(ExecutionChannelBinding, AttachedExternalExecutionChannel)> {
    bootstrap.validate()?;
    request.validate_for_bootstrap(bootstrap)?;
    let supervisor_public_key = request.supervisor_public_key.clone();
    ensure!(
        supervisor_public_key
            == ryeos_state::external_execution::encode_channel_public_key(
                &supervisor_signing_key.verifying_key(),
            )?,
        "external attachment request changed its retained supervisor key"
    );
    let client = build_client(bootstrap, network_inputs)?;
    let request_bytes = request.canonical_bytes()?;
    let response = client
        .post_json(
            &bootstrap.controller.attach_url()?,
            request_bytes,
            ATTACH_RESPONSE_BYTES,
            lillux::time::MonotonicDeadline::after(Duration::from_millis(u64::from(
                bootstrap.controller.request_timeout_ms,
            ))),
        )
        .map_err(anyhow::Error::new)
        .context("send external channel attachment")?;
    let response: ExternalChannelAttachResponse = decode_response(
        response,
        ATTACH_RESPONSE_BYTES,
        "external channel attachment",
    )?;
    response.validate_for_bootstrap(bootstrap, &supervisor_public_key)?;
    let channel = attached_channel(bootstrap, client)?;
    Ok((response.binding, channel))
}

/// Reconstruct only the HTTP transport for an already durably retained exact
/// binding. This performs no attachment mutation and is therefore the sole
/// pre-launch recovery path from the outer journal's `attached` stage.
pub fn reconnect_external_execution_channel(
    bootstrap: &ExternalSupervisorBootstrap,
    supervisor_signing_key: &lillux::crypto::SigningKey,
    binding: &ExecutionChannelBinding,
    network_inputs: &ryeos_state::external_execution::transport::ExternalCapturedNetworkInputs,
) -> Result<AttachedExternalExecutionChannel> {
    bootstrap.validate_attached_binding(
        binding,
        &ryeos_state::external_execution::encode_channel_public_key(
            &supervisor_signing_key.verifying_key(),
        )?,
    )?;
    attached_channel(bootstrap, build_client(bootstrap, network_inputs)?)
}

fn attached_channel(
    bootstrap: &ExternalSupervisorBootstrap,
    client: BoundedHttpClient,
) -> Result<AttachedExternalExecutionChannel> {
    Ok(AttachedExternalExecutionChannel {
        client,
        exchange_url: bootstrap.controller.exchange_url()?,
        placement_thread_id: bootstrap.placement_thread_id.clone(),
        occurrence_id: bootstrap.occurrence_id.clone(),
        maximum_response_bytes: bootstrap.controller.maximum_response_bytes,
        request_timeout: Duration::from_millis(u64::from(bootstrap.controller.request_timeout_ms)),
    })
}

fn build_client(
    bootstrap: &ExternalSupervisorBootstrap,
    network_inputs: &ryeos_state::external_execution::transport::ExternalCapturedNetworkInputs,
) -> Result<BoundedHttpClient> {
    network_inputs.validate_for(&bootstrap.controller.network_inputs)?;
    let network = lillux::network::NetworkContext::from_config_bytes(
        &network_inputs.resolver_bytes()?,
        &network_inputs.hosts_bytes()?,
    )?;
    BoundedHttpClient::new(bootstrap, network)
}

fn canonical_request_bytes(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    Ok(lillux::canonical_json(&serde_json::to_value(value)?)?.into_bytes())
}

fn decode_response<T: serde::de::DeserializeOwned>(
    response: HttpResponse,
    maximum_bytes: u64,
    label: &str,
) -> Result<T> {
    ensure!(
        (200..300).contains(&response.status),
        "{label} returned HTTP {}",
        response.status
    );
    let bytes = response.body;
    ensure!(
        u64::try_from(bytes.len())? <= maximum_bytes,
        "{label} response exceeds its bound"
    );
    serde_json::from_slice(&bytes).with_context(|| format!("decode {label} response"))
}

impl ExternalExecutionChannelTransport for AttachedExternalExecutionChannel {
    fn exchange(&mut self, canonical_supervisor_frame: &[u8]) -> Result<ExternalTransportExchange> {
        self.exchange_until(
            canonical_supervisor_frame,
            lillux::time::MonotonicDeadline::after(self.request_timeout),
        )
        .map_err(anyhow::Error::new)
    }

    fn exchange_until(
        &mut self,
        canonical_supervisor_frame: &[u8],
        deadline: lillux::time::MonotonicDeadline,
    ) -> std::result::Result<ExternalTransportExchange, ExternalTransportStepFailure> {
        let request = ExternalChannelExchangeRequest::from_frame(
            self.placement_thread_id.clone(),
            self.occurrence_id.clone(),
            canonical_supervisor_frame,
        )
        .map_err(ExternalTransportStepFailure::Fatal)?;
        let request_bytes =
            canonical_request_bytes(&request).map_err(ExternalTransportStepFailure::Fatal)?;
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return Err(ExternalTransportStepFailure::AmbiguousTransport(
                anyhow::anyhow!("external exchange deadline elapsed before contact"),
            ));
        }
        let response = self.client.post_json(
            &self.exchange_url,
            request_bytes,
            self.maximum_response_bytes,
            deadline.min(lillux::time::MonotonicDeadline::after(self.request_timeout)),
        )?;
        let response = decode_exchange_response(response, self.maximum_response_bytes)?;
        Ok(ExternalTransportExchange {
            schema: response.schema,
            incoming_new: response.incoming_new,
            incoming_sequence: response.incoming_sequence,
            incoming_frame_digest: response.incoming_frame_digest,
            acknowledgement_frame_digest: response.acknowledgement_frame_digest,
            outbound_frames: response
                .outbound_frames
                .into_iter()
                .map(decode_frame)
                .collect::<Result<Vec<_>>>()
                .map_err(ExternalTransportStepFailure::Fatal)?,
            urgent_revocation_frame: response
                .urgent_revocation_frame
                .map(decode_frame)
                .transpose()
                .map_err(ExternalTransportStepFailure::Fatal)?,
        })
    }
}

fn decode_exchange_response(
    response: HttpResponse,
    maximum_bytes: u64,
) -> std::result::Result<ExternalChannelExchangeResponse, ExternalTransportStepFailure> {
    if !(200..300).contains(&response.status) {
        return Err(ExternalTransportStepFailure::Fatal(anyhow::anyhow!(
            "external channel exchange returned HTTP {}",
            response.status
        )));
    }
    let bytes = response.body;
    if u64::try_from(bytes.len())
        .map_err(|error| ExternalTransportStepFailure::Fatal(anyhow::Error::new(error)))?
        > maximum_bytes
    {
        return Err(ExternalTransportStepFailure::Fatal(anyhow::anyhow!(
            "external channel exchange response exceeds its bound"
        )));
    }
    let decoded: ExternalChannelExchangeResponse = serde_json::from_slice(&bytes)
        .context("decode external channel exchange response")
        .map_err(ExternalTransportStepFailure::Fatal)?;
    decoded
        .validate_shape()
        .map_err(ExternalTransportStepFailure::Fatal)?;
    Ok(decoded)
}

fn decode_frame(frame: ExternalChannelResponseFrame) -> Result<ExternalTransportFrame> {
    let canonical_wire = frame.decode_frame()?;
    Ok(ExternalTransportFrame {
        sequence: frame.sequence,
        frame_digest: frame.frame_digest,
        canonical_wire,
    })
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use ryeos_state::external_execution::admission::{
        AdmittedExternalCandidateProgram, ExternalCandidateProcFilesystem,
        ExternalCandidateRequirement, ExternalCandidateRuntimeRecipe, PROTOCOL,
    };
    use ryeos_state::external_execution::encode_channel_public_key;
    use ryeos_state::external_execution::supervisor_journal::{
        ExternalSupervisorJournalRecovery, PreparedExternalSupervisorJournal,
    };
    use ryeos_state::external_execution::transport::EXTERNAL_CHANNEL_TRANSPORT_SCHEMA;

    const TEST_CA_DER: &str = "MIIDETCCAfmgAwIBAgIUX+scmKJ6HD/VzI8cSkNb1CDQYYMwDQYJKoZIhvcNAQELBQAwGDEWMBQGA1UEAwwNUnllT1MgVGVzdCBDQTAeFw0yNjA5MjAxMzQ1NDVaFw0zNjA5MTcxMzQ1NDVaMBgxFjAUBgNVBAMMDVJ5ZU9TIFRlc3QgQ0EwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQC+O+75C261AEfbAhLd1BO0VJmKyR4jk6bDQ1EU3druPMqf6qfvcfFye+FqR2mTyjRw0Lzw+WpEfpUT2Vo7qQVbSMnsaw9do1OK+gI2A+L2bWLH2uCN4nLULEbN91COVmnzY19sQz1esCDkGAza8RbgZjXOabK7Nil7R1HyFtlWj96eik5OEGjEpdAJQpsT9hhJXslyMBmSTtccR3Zl5fL7hbBnI8aUG5EbgPg/SWrtCN2M25RmmFdPbBz5aspSfsv8G4LsqH6NaDKwTC0iliR79C/D+wCFKcnOZIvHCDhTgAcNi0I44W0J0MlAm96wXmmv2Im1UF6DagU5cH/avTuVAgMBAAGjUzBRMB0GA1UdDgQWBBQM8Xib0/5JOrMFvPu3AFMR4v+m/DAfBgNVHSMEGDAWgBQM8Xib0/5JOrMFvPu3AFMR4v+m/DAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQCaxmJjrKa4su45TmAYnZPDxRqvgDomTmO89BBPXnY5qHTUZ2bfu6o3vtm5tRywiQXpQkzYIEqYJbT2RndFxgwPigyUqKviA+URXSMX7C8dn01eUtqIe73XYnsBey57JjnRgYtERBytFCGFaaqrreT+Tf1ZV4mjrqqgTyEXxr5/L+TtyYq1D4b4dWSkyEuf8qPFP36RGkVxC0dDzhwXC5AiewkPgGTiQvzcWVB+FyYKmrVMdkR6o5+1Chw7IJiPZSm4/JLX3UQb+Wc/+Lc7lMx4APqExKlW0KLTLCmYZ6gnff6bva6DLcYbZuxu486fc7DPFK3hNqLzrjkaAq5MlFqH";
    const TEST_SERVER_DER: &str = "MIIDJzCCAg+gAwIBAgIUO9YtUXKy4lBXHfmjbWP+VWuMp2IwDQYJKoZIhvcNAQELBQAwGDEWMBQGA1UEAwwNUnllT1MgVGVzdCBDQTAeFw0yNjA5MjAxMzQ1NDVaFw0zNjA5MTcxMzQ1NDVaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBALmtC3mDEpcyntYNCnHHF97p3C3gcKVTWHOaahFOrcylv4uLnIlkNS//HBGPvWrL4r+7JUGiMyHny5h8zzhXsfBB8AnhqqaYdnpBbthh6mEJmQ9xrB8x8Zxe/aShIDeVViIAa4mMSBok9nCdW6KZc/1LmteIG70ulpDDbl4zHWIp/vJU6rrQzJbzyVfGFwKCD6hwQfhp9RMOmogaC6CtRhsEDqTmjpRHnmXTKbxDXHd22LgxwWqqPhh7nhBpab90B6YF6krKsCwXBO6lW6IOSSS6zODXkVi0DEUCuWqOmSEbETp5DwIeQXCzZPBJMO3seYSMsO/4sWnbZ1gmjQdq9CkCAwEAAaNtMGswFAYDVR0RBA0wC4IJbG9jYWxob3N0MBMGA1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBTbuqZMwWmYDtA+W2h5EwZ8pEfnojAfBgNVHSMEGDAWgBQM8Xib0/5JOrMFvPu3AFMR4v+m/DANBgkqhkiG9w0BAQsFAAOCAQEAOA9Wly9zikvpml9I5pcBG9BuZwS3W9y5jk+nVbgEMHndcHxbBttRMw7EcvqHku8YkOIqnrGQm27CtomoH7RdzOjG1pxtDH4hrEpPWpP/PJpnbMNAvMHamABNQDQTQT8sIIJdDMtCIN2/sqaryAt2PTf7tdEZR4OApFD83UQ1Ba7Xauxct8aVsgaijdjnxnK+z3/5Czx+lwBl5mxTwfnYn8DBckjF0lRAKUcQ3A1Vo680zwg52hiGXvmCTHjWiYMK3suLtYdE0HKIp2xphEmpdQNdQW7VUOHq3xD+QSBfpbTkGRKpqyaXfN4sORg72d5J3pYPydwn8v2BBcbcSpkFmA==";
    const TEST_EXPIRED_SERVER_DER: &str = "MIIDFTCCAf2gAwIBAgICEAAwDQYJKoZIhvcNAQELBQAwGDEWMBQGA1UEAwwNUnllT1MgVGVzdCBDQTAeFw0yNDAxMDEwMDAwMDBaFw0yNTAxMDEwMDAwMDBaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBALmtC3mDEpcyntYNCnHHF97p3C3gcKVTWHOaahFOrcylv4uLnIlkNS//HBGPvWrL4r+7JUGiMyHny5h8zzhXsfBB8AnhqqaYdnpBbthh6mEJmQ9xrB8x8Zxe/aShIDeVViIAa4mMSBok9nCdW6KZc/1LmteIG70ulpDDbl4zHWIp/vJU6rrQzJbzyVfGFwKCD6hwQfhp9RMOmogaC6CtRhsEDqTmjpRHnmXTKbxDXHd22LgxwWqqPhh7nhBpab90B6YF6krKsCwXBO6lW6IOSSS6zODXkVi0DEUCuWqOmSEbETp5DwIeQXCzZPBJMO3seYSMsO/4sWnbZ1gmjQdq9CkCAwEAAaNtMGswFAYDVR0RBA0wC4IJbG9jYWxob3N0MBMGA1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBTbuqZMwWmYDtA+W2h5EwZ8pEfnojAfBgNVHSMEGDAWgBQM8Xib0/5JOrMFvPu3AFMR4v+m/DANBgkqhkiG9w0BAQsFAAOCAQEAH6t0SUMY9LXf61O/XFG6QwAXSLTnZHIb22AO9rZJsVPm4nUSViNGV8HzAGaHTiD5UACAb6iCU7cC1QuoGoxoCLkW/aigoYNYqgnGn/IH0Hw76IQO/xb09JAVF2HAATTILwhcZUv6JI0wCei9aLL+lsFRdoBdnKxLbrq3twiBIIbP+tF85NdJDu8d/hocctxrtcLs+7QGHDvVrnccsc/s8LWZm0lOzyQ5x+/0rWgOc4Yh4j8Cm420ifNQZc/ZL2He/13xlyFVwdXeamkPMd77mBF1FIpVDyD/vEXakK+ksMvUWCW5fN9wTmscilikQWAqzYQlkvrZATnHIs56IB1ehg==";
    const TEST_WRONG_CA_DER: &str = "MIIDHTCCAgWgAwIBAgIUcyh+Kng6fHpN9Q3VlFdkDKwyCaAwDQYJKoZIhvcNAQELBQAwHjEcMBoGA1UEAwwTV3JvbmctUnllT1MtVGVzdC1DQTAeFw0yNjA5MjAxNDAwMzJaFw0zNjA5MTcxNDAwMzJaMB4xHDAaBgNVBAMME1dyb25nLVJ5ZU9TLVRlc3QtQ0EwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQC8V3sqPuzqiXRJr5eWdi/Ndw7txQ9QsQCoTwjTG/M9esgx2dFYRH3XCFszzCpj0cS9WDidyFwFsqVnyOhHuY0ZV25ZO6qJipodMHWLA0dt6AlJ5RCULwfmXCHHmGtebRZmPzfocFgEtG/up1pN0K18BYJemHrWpqJWHlWlb4lgrsCldS5nrX593kCz6qrPYun72+ps/E5CVhNIPXIe9GIDmg+ev7hR2iq8twhMf8mRikV3Zqc7Bek7JNKEbGQXZsSzakV49b+/zPhFOGspYycaDGya6EAkWFrbSb+CmUK1PEr2HlHCW7Ui6Dm+yh3EKo8RWdNXi0j46cdCAp0ShRxjAgMBAAGjUzBRMB0GA1UdDgQWBBQmooMsZU2HX/NG85+piMVRZb2jQzAfBgNVHSMEGDAWgBQmooMsZU2HX/NG85+piMVRZb2jQzAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQBAxyrUhVe8Fwh0h+8BrJdA33aOZhx/+uPKZmQYBrsfnn2LEgJUxNDucWeILiJNrYljDLbGRqWB61gomBFgJP3dV6QEK30tL2oQgsO/VQlsbeXkEUKCBfDKzQU+ARKrKFQIlE153mjVHdgek5YNA5Kt2iSV6JHZT9Jm5vpXNsbBb04SOkvpJOMhL7CapiWdseU/Gg4TrRzNbiwGDJpPnjYmBj5ygcbn2GFGgxNF3pHBHGf5mQqQp4ZalvDFpyUZFsHfu43mfFjhtu8b6Ara4y2pJDf7ZRqCgxFPQUBFlF82NEbRlEuXCH3zXbeCwOMk3yrf3N3oYBCK4kFyPT3FPaKL";
    const TEST_SERVER_KEY_DER: &str = "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC5rQt5gxKXMp7WDQpxxxfe6dwt4HClU1hzmmoRTq3Mpb+Li5yJZDUv/xwRj71qy+K/uyVBojMh58uYfM84V7HwQfAJ4aqmmHZ6QW7YYephCZkPcawfMfGcXv2koSA3lVYiAGuJjEgaJPZwnVuimXP9S5rXiBu9LpaQw25eMx1iKf7yVOq60MyW88lXxhcCgg+ocEH4afUTDpqIGgugrUYbBA6k5o6UR55l0ym8Q1x3dti4McFqqj4Ye54QaWm/dAemBepKyrAsFwTupVuiDkkkuszg15FYtAxFArlqjpkhGxE6eQ8CHkFws2TwSTDt7HmEjLDv+LFp22dYJo0HavQpAgMBAAECggEAKPhbCNP4PS6pR7gW7uYsiT53HBRjJsfOQ6v17Z270eVc77C9uL9I0S9shR9/f1o/zWjBHstolvmrvhkELH2FQOt7yOJnol0P/4gCqnJookLY6ER/415E3ulC9JmtHzavi88l63Lt0f8H9e9y8d0EcAbHwvlAja0DAixtZRHIUQls57TbuVPfDRkGJ7q9fTaX3VcuOp57Uhv2K6nKPuqhLWpA++Tw3+VmGCx5yAPhmqKPUmYnytC1iW/adHi2TfF+O1SONegoEJtCPfN9xcugOz7FdLaGyGrLRAw2JDFK96JZMj6APvnwgUS3cJCXPfigl1SaGGOONQXx7AuWDUE0hQKBgQDlEwuzNAalh/31MVDbqk4d+k6lk/EARF/ezcecIZhukDFy3N6LI6n/oYakEJcDp9ZsXQFjw9Tf6g3dwxUIBdnv/Rk4D+aEiJoMAaIJdCcjNNJe0B3ZMW2rpDRQGuSQWXZ04Uhpu+bWozEqMNzqFr56/yHi/ikcdGsecZMvDalohwKBgQDPgB64+9R3coT4Dkuk466ES70IAuHUIp/JhmH67cLt9AXQzHDM4f9JYEkoz5AQigQyamwuHsdXT2Jn1eLN+9BdtoGjTGSRvBtltKtzA02O70VExhvItN6J3Fma47j9ha1FOPVnRUrqykzzAtWqMkNH3VNpW4U4OKdFRZPX5cPZzwKBgBeJo3QgbmZn2NJu5M4Na8VsyNP+pY7Pd8JfBpmmYhFKQ6p3w24slfUsVbdZ9QptHn03+UKVBrSTSiV1PB386+3a5dJ638bSenGtYUbzZmoZrVwMqmR8zbYLQ0zP1ph2eNN9qoEiy49WaWDacHilKaFdwc+fKf5AgBk6tlLpZnTVAoGAVUFP3jNiLZ248m51OA9wUd0IkvUUMmPzgQqc0UvFTp13kj2djyDAEjbkeEcn6xO5+7jsL9rnjoEIbp9bq8Rt7UMiaqTloVdHbndYBk5yHGtE66f2HHXsBXqqulAcXtYAxjNL6R14VZW/Hg2pGl/CcxGFxwEacGoemACpaQh3etMCgYA2fdOocmWnWgSTxGEf7ohIUnK2TTfpae39zl+tOVY74MKLvBycg5qogjoJgYWx490Segvk+z9HCGOxd9MJ0CXssHVf9wWrHwNKQ/IkcruT2LNWW9kxyisSSjzHQiq+t+pSDmaj80Y28fDlhWkyTJgYP3o+sOp1kWSBVfV0faeHYw==";

    fn test_network_inputs(
        bootstrap: &ExternalSupervisorBootstrap,
    ) -> ryeos_state::external_execution::transport::ExternalCapturedNetworkInputs {
        ryeos_state::external_execution::transport::ExternalCapturedNetworkInputs::from_bytes(
            &bootstrap.controller.network_inputs,
            b"nameserver 127.0.0.1\n",
            b"127.0.0.1 localhost\n",
        )
        .unwrap()
    }

    fn bootstrap(origin: String, roots: Vec<String>) -> ExternalSupervisorBootstrap {
        let runtime_recipe = ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        };
        let runtime_recipe_digest = runtime_recipe.digest().unwrap();
        let guest_inputs = ryeos_external_execution_contract::ExternalGuestInputProjection {
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: ryeos_external_execution_contract::GuestBaseSnapshotInput {
                descriptor: 55,
                snapshot_hash: "c".repeat(64),
                closure_digest: "5".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![ryeos_external_execution_contract::GuestMountInput {
                role: ryeos_external_execution_contract::GuestMountRole::Product,
                authority_id: "runtime".into(),
                descriptor: 64,
                destination: "/runtime".into(),
                kind: ryeos_external_execution_contract::GuestMountKind::Directory,
                access: ryeos_external_execution_contract::GuestMountAccess::ReadOnly,
                normalized_mode: None,
                content_authority:
                    ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                        manifest_kind:
                            ryeos_external_execution_contract::GuestProductManifestKind::Content,
                        manifest_hash: "e".repeat(64),
                        manifest_descriptor: 65,
                        manifest_bytes: 256,
                    },
                bytes: 1,
            }],
            executable_search: vec!["/runtime/bin".into()],
            environment: BTreeMap::new(),
        };
        let guest_input_identity = guest_inputs.identity_digest().unwrap();
        let requirement = ExternalCandidateRequirement {
            schema: 6,
            required_lifecycle_capabilities: Default::default(),
            protocol: PROTOCOL.into(),
            connector_protocol:
                ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
            execution_route: ryeos_state::external_execution::admission::ExternalCandidateExecutionRoute::ConnectorOnly,
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "runtime".into(),
            runtime_recipe,
        };
        let qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        ExternalSupervisorBootstrap {
            schema: 7,
            controller:
                ryeos_state::external_execution::transport::ExternalControllerTransportContract {
                    schema: 2,
                    network_inputs: ryeos_state::external_execution::transport::ExternalNetworkInputPolicy {
                        resolver: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                            source: "/fixture/resolver".into(), max_bytes: 64 * 1024,
                        },
                        hosts: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                            source: "/fixture/hosts".into(), max_bytes: 64 * 1024,
                        },
                    },
                    https_origin: origin,
                    route_contract:
                        ryeos_state::external_execution::transport::EXTERNAL_CHANNEL_ROUTE_CONTRACT
                            .into(),
                    tls_root_bundle_digest:
                        ryeos_state::external_execution::transport::external_tls_root_bundle_digest(
                            &roots,
                        )
                        .unwrap(),
                    connect_timeout_ms: 1_000,
                    request_timeout_ms: 2_000,
                    maximum_response_bytes: 64 * 1024,
                },
            tls_root_certificates_der_base64: roots,
            placement_thread_id: "T-placement".into(),
            occurrence_id: "occurrence-one".into(),
            allocation_request_digest: "a".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            launcher_artifact_hash: "4".repeat(64),
            candidate_program: AdmittedExternalCandidateProgram {
                requirement,
                qualification_use,
                runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
                runtime_manifest_hash: "e".repeat(64),
                runtime_witness_hash: "1".repeat(64),
                qualification_attestation_hash: "2".repeat(64),
                selection_identity_digest: "3".repeat(64),
           runtime_recipe_digest,
            }.into(),
           guest_input_identity,
            guest_inputs,
            owner_public_key: encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[51; 32]).verifying_key(),
            )
            .unwrap(),
            bootstrap_capability: STANDARD.encode([52_u8; 32]),
            attachment_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap()
                + 60_000,
            execution_timeout_seconds: 60,
            post_execution_timeout_seconds: 120,
            candidate_export_max_bytes: 512 * 1024,
            channel_max_bytes: 1024 * 1024,
        }
    }

    type HttpHandler = Box<dyn Fn(Vec<u8>) -> Vec<u8> + Send>;

    fn tls_server_config(certificate_der: &str) -> Arc<rustls::ServerConfig> {
        let certificate =
            rustls::pki_types::CertificateDer::from(STANDARD.decode(certificate_der).unwrap());
        let private_key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                STANDARD.decode(TEST_SERVER_KEY_DER).unwrap(),
            ));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certificate], private_key)
                .unwrap(),
        )
    }

    fn read_http_request(
        tls: &mut rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>,
    ) -> Result<Vec<u8>> {
        let mut request = Vec::new();
        let header_end = loop {
            ensure!(request.len() <= 1024 * 1024, "test request exceeded bound");
            let mut chunk = [0_u8; 4096];
            let count = tls.read(&mut chunk)?;
            ensure!(count > 0, "test request ended before its headers");
            request.extend_from_slice(&chunk[..count]);
            if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end])?;
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim().parse::<usize>())
            })
            .transpose()?
            .unwrap_or(0);
        ensure!(
            content_length <= 1024 * 1024,
            "test request body exceeded bound"
        );
        while request.len().saturating_sub(header_end) < content_length {
            let mut chunk = [0_u8; 4096];
            let count = tls.read(&mut chunk)?;
            ensure!(count > 0, "test request ended before its body");
            request.extend_from_slice(&chunk[..count]);
        }
        request.truncate(header_end + content_length);
        Ok(request)
    }

    fn request_body(request: &[u8]) -> &[u8] {
        let header_end = request
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap()
            + 4;
        &request[header_end..]
    }

    fn serve_tls(
        listener: TcpListener,
        certificate_der: &'static str,
        handlers: Vec<HttpHandler>,
    ) -> std::thread::JoinHandle<usize> {
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let config = tls_server_config(certificate_der);
            let mut requests = 0;
            for handler in handlers {
                let deadline = Instant::now() + Duration::from_secs(5);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break Some(stream),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                break None;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("test listener failed: {error}"),
                    }
                };
                let Some(stream) = stream else { break };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let connection = rustls::ServerConnection::new(config.clone()).unwrap();
                let mut tls = rustls::StreamOwned::new(connection, stream);
                let Ok(request) = read_http_request(&mut tls) else {
                    continue;
                };
                requests += 1;
                let response = handler(request);
                if !response.is_empty() {
                    let _ = tls.write_all(&response);
                    let _ = tls.flush();
                }
            }
            requests
        })
    }

    fn http_response(status: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn binding_for_request(
        bootstrap_value: &serde_json::Value,
        request: &[u8],
    ) -> (ExternalSupervisorBootstrap, ExecutionChannelBinding) {
        let bootstrap: ExternalSupervisorBootstrap =
            serde_json::from_value(bootstrap_value.clone()).unwrap();
        let attach: ExternalChannelAttachRequest =
            serde_json::from_slice(request_body(request)).unwrap();
        attach.validate_shape().unwrap();
        assert_eq!(attach.bootstrap_capability, bootstrap.bootstrap_capability);
        let issued_at_ms = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        let binding = ExecutionChannelBinding {
            schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
            placement_thread_id: bootstrap.placement_thread_id.clone(),
            allocation_request_digest: bootstrap.allocation_request_digest.clone(),
            occurrence_id: bootstrap.occurrence_id.clone(),
            admitted_capsule_hash: bootstrap.admitted_capsule_hash.clone(),
            base_snapshot_hash: bootstrap.base_snapshot_hash.clone(),
            execution_binding_hash: bootstrap.execution_binding_hash.clone(),
            supervisor_runtime_hash: bootstrap.supervisor_runtime_hash.clone(),
            candidate_program_digest: bootstrap.candidate_program.digest().unwrap(),
            channel_nonce: "f".repeat(64),
            owner_public_key: bootstrap.owner_public_key.clone(),
            supervisor_public_key: attach.supervisor_public_key,
            issued_at_ms,
            execution_deadline_ms: issued_at_ms
                + i64::from(bootstrap.execution_timeout_seconds) * 1_000,
            expires_at_ms: issued_at_ms
                + i64::from(
                    bootstrap.execution_timeout_seconds + bootstrap.post_execution_timeout_seconds,
                ) * 1_000,
            candidate_export_max_bytes: bootstrap.candidate_export_max_bytes,
            max_frames: bootstrap.binding_max_frames().unwrap(),
            max_bytes: bootstrap.channel_max_bytes,
        };
        (bootstrap, binding)
    }

    fn attach_success_response(bootstrap: serde_json::Value, request: Vec<u8>) -> Vec<u8> {
        let (_, binding) = binding_for_request(&bootstrap, &request);
        let response = ExternalChannelAttachResponse {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            binding_digest: binding.digest().unwrap(),
            binding,
        };
        http_response("200 OK", &canonical_request_bytes(&response).unwrap(), "")
    }

    fn mutated_attach_response(
        bootstrap: serde_json::Value,
        request: Vec<u8>,
        mutation: &str,
    ) -> Vec<u8> {
        let (_, mut binding) = binding_for_request(&bootstrap, &request);
        match mutation {
            "placement" => binding.placement_thread_id = "T-other".into(),
            "occurrence" => binding.occurrence_id = "occurrence-other".into(),
            "request" => binding.allocation_request_digest = "0".repeat(64),
            "capsule" => binding.admitted_capsule_hash = "1".repeat(64),
            "base" => binding.base_snapshot_hash = "2".repeat(64),
            "binding" => binding.execution_binding_hash = "3".repeat(64),
            "runtime" => binding.supervisor_runtime_hash = "4".repeat(64),
            "program" => binding.candidate_program_digest = "5".repeat(64),
            "owner_key" => {
                binding.owner_public_key = encode_channel_public_key(
                    &lillux::crypto::SigningKey::from_bytes(&[61; 32]).verifying_key(),
                )
                .unwrap();
            }
            "supervisor_key" => {
                binding.supervisor_public_key = encode_channel_public_key(
                    &lillux::crypto::SigningKey::from_bytes(&[62; 32]).verifying_key(),
                )
                .unwrap();
            }
            "execution_window" => {
                binding.execution_deadline_ms += 1_000;
                binding.expires_at_ms += 1_000;
            }
            "post_window" => binding.expires_at_ms += 1_000,
            "max_bytes" => binding.max_bytes += 1,
            "max_frames" => binding.max_frames += 1,
            other => panic!("unknown binding mutation {other}"),
        }
        binding.validate().unwrap();
        let response = ExternalChannelAttachResponse {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            binding_digest: binding.digest().unwrap(),
            binding,
        };
        http_response("200 OK", &canonical_request_bytes(&response).unwrap(), "")
    }

    fn unknown_field_attach_response(bootstrap: serde_json::Value, request: Vec<u8>) -> Vec<u8> {
        let (_, binding) = binding_for_request(&bootstrap, &request);
        let mut response = serde_json::to_value(ExternalChannelAttachResponse {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            binding_digest: binding.digest().unwrap(),
            binding,
        })
        .unwrap();
        response
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), serde_json::Value::Bool(true));
        http_response(
            "200 OK",
            lillux::canonical_json(&response).unwrap().as_bytes(),
            "",
        )
    }

    #[test]
    fn malformed_admitted_root_refuses_before_any_request() {
        let roots = vec![STANDARD.encode(b"not a certificate")];
        let bootstrap = bootstrap("https://controller.example".into(), roots);
        assert!(build_client(&bootstrap, &test_network_inputs(&bootstrap)).is_err());
    }

    #[test]
    fn real_https_attachment_uses_only_admitted_root_and_exact_binding() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let root = TEST_CA_DER.to_owned();
        let bootstrap = bootstrap(format!("https://localhost:{}", address.port()), vec![root]);
        let server_bootstrap = serde_json::to_value(&bootstrap).unwrap();
        let server = serve_tls(
            listener,
            TEST_SERVER_DER,
            vec![Box::new(move |request| {
                attach_success_response(server_bootstrap.clone(), request)
            })],
        );
        let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
        let (binding, _) = attach_external_execution_channel(
            &bootstrap,
            &supervisor_key,
            &test_network_inputs(&bootstrap),
        )
        .unwrap();
        assert_eq!(binding.placement_thread_id, bootstrap.placement_thread_id);
        assert_eq!(binding.occurrence_id, bootstrap.occurrence_id);
        assert_eq!(server.join().unwrap(), 1);
    }

    #[test]
    fn wrong_root_hostname_and_expired_certificate_send_no_http_request() {
        for (name, roots, host, certificate) in [
            (
                "wrong root",
                vec![TEST_WRONG_CA_DER.to_owned()],
                "localhost",
                TEST_SERVER_DER,
            ),
            (
                "wrong hostname",
                vec![TEST_CA_DER.to_owned()],
                "127.0.0.1",
                TEST_SERVER_DER,
            ),
            (
                "expired certificate",
                vec![TEST_CA_DER.to_owned()],
                "localhost",
                TEST_EXPIRED_SERVER_DER,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let bootstrap = bootstrap(format!("https://{host}:{port}"), roots);
            let server = serve_tls(
                listener,
                certificate,
                vec![Box::new(|_| panic!("rejected TLS peer received HTTP"))],
            );
            let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
            assert!(
                attach_external_execution_channel(
                    &bootstrap,
                    &supervisor_key,
                    &test_network_inputs(&bootstrap)
                )
                .is_err(),
                "{name} unexpectedly attached"
            );
            assert_eq!(server.join().unwrap(), 0, "{name} exposed an HTTP request");
        }
    }

    #[test]
    fn redirects_are_not_followed() {
        let redirect_target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_port = redirect_target.local_addr().unwrap().port();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bootstrap = bootstrap(
            format!("https://localhost:{port}"),
            vec![TEST_CA_DER.to_owned()],
        );
        let server = serve_tls(
            listener,
            TEST_SERVER_DER,
            vec![Box::new(move |_| {
                format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: https://localhost:{target_port}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .into_bytes()
            })],
        );
        let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
        assert!(
            attach_external_execution_channel(
                &bootstrap,
                &supervisor_key,
                &test_network_inputs(&bootstrap)
            )
            .is_err()
        );
        assert_eq!(server.join().unwrap(), 1);
        redirect_target.set_nonblocking(true).unwrap();
        assert_eq!(
            redirect_target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn malformed_or_oversized_http_responses_are_rejected() {
        let oversized = vec![b'x'; ATTACH_RESPONSE_BYTES as usize + 1];
        let mut chunked = format!("{:x}\r\n", oversized.len()).into_bytes();
        chunked.extend_from_slice(&oversized);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let mut chunked_response =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        chunked_response.extend_from_slice(&chunked);
        let responses = vec![
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 65537\r\nConnection: close\r\n\r\n".to_vec(),
                Some("exceeds its bound"),
            ),
            (chunked_response, Some("exceeds its bound")),
            (http_response("200 OK", b"{", ""), None),
            (http_response("503 Service Unavailable", b"{}", ""), None),
        ];
        for (response, expected_error) in responses {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let bootstrap = bootstrap(
                format!("https://localhost:{port}"),
                vec![TEST_CA_DER.to_owned()],
            );
            let server = serve_tls(
                listener,
                TEST_SERVER_DER,
                vec![Box::new(move |_| response.clone())],
            );
            let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
            let Err(error) = attach_external_execution_channel(
                &bootstrap,
                &supervisor_key,
                &test_network_inputs(&bootstrap),
            ) else {
                panic!("malformed or oversized response was accepted")
            };
            if let Some(expected_error) = expected_error {
                assert!(
                    format!("{error:#}").contains(expected_error),
                    "wrong response-bound failure: {error:#}"
                );
            }
            assert_eq!(server.join().unwrap(), 1);
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bootstrap = bootstrap(
            format!("https://localhost:{port}"),
            vec![TEST_CA_DER.to_owned()],
        );
        let server_bootstrap = serde_json::to_value(&bootstrap).unwrap();
        let server = serve_tls(
            listener,
            TEST_SERVER_DER,
            vec![Box::new(move |request| {
                unknown_field_attach_response(server_bootstrap.clone(), request)
            })],
        );
        let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
        let Err(error) = attach_external_execution_channel(
            &bootstrap,
            &supervisor_key,
            &test_network_inputs(&bootstrap),
        ) else {
            panic!("unknown response field was accepted")
        };
        assert!(
            format!("{error:#}").contains("unknown field"),
            "closed response shape was not the refusal: {error:#}"
        );
        assert_eq!(server.join().unwrap(), 1);
    }

    #[test]
    fn every_attached_binding_coordinate_is_joined_to_bootstrap() {
        for mutation in [
            "placement",
            "occurrence",
            "request",
            "capsule",
            "base",
            "binding",
            "runtime",
            "program",
            "owner_key",
            "supervisor_key",
            "execution_window",
            "post_window",
            "max_bytes",
            "max_frames",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let bootstrap = bootstrap(
                format!("https://localhost:{port}"),
                vec![TEST_CA_DER.to_owned()],
            );
            let server_bootstrap = serde_json::to_value(&bootstrap).unwrap();
            let server = serve_tls(
                listener,
                TEST_SERVER_DER,
                vec![Box::new(move |request| {
                    mutated_attach_response(server_bootstrap.clone(), request, mutation)
                })],
            );
            let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
            assert!(
                attach_external_execution_channel(
                    &bootstrap,
                    &supervisor_key,
                    &test_network_inputs(&bootstrap)
                )
                .is_err(),
                "substituted {mutation} was accepted"
            );
            assert_eq!(server.join().unwrap(), 1);
        }
    }

    #[test]
    fn lost_attach_response_retries_same_key_after_bootstrap_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut bootstrap = bootstrap(
            format!("https://localhost:{port}"),
            vec![TEST_CA_DER.to_owned()],
        );
        bootstrap.attachment_deadline_ms =
            i64::try_from(lillux::time::timestamp_millis()).unwrap() + 100;
        let server_bootstrap = serde_json::to_value(&bootstrap).unwrap();
        let retained_response = Arc::new(std::sync::Mutex::new(None::<Vec<u8>>));
        let retained_request = Arc::new(std::sync::Mutex::new(None::<Vec<u8>>));
        let first_retained = retained_response.clone();
        let second_retained = retained_response.clone();
        let first_request = retained_request.clone();
        let second_request = retained_request.clone();
        let server = serve_tls(
            listener,
            TEST_SERVER_DER,
            vec![
                Box::new(move |request| {
                    *first_request.lock().unwrap() = Some(request_body(&request).to_vec());
                    *first_retained.lock().unwrap() =
                        Some(attach_success_response(server_bootstrap.clone(), request));
                    Vec::new()
                }),
                Box::new(move |request| {
                    assert_eq!(
                        request_body(&request),
                        second_request.lock().unwrap().as_deref().unwrap()
                    );
                    let attach: ExternalChannelAttachRequest =
                        serde_json::from_slice(request_body(&request)).unwrap();
                    assert_eq!(
                        attach.supervisor_public_key,
                        encode_channel_public_key(
                            &lillux::crypto::SigningKey::from_bytes(&[53; 32]).verifying_key()
                        )
                        .unwrap()
                    );
                    second_retained.lock().unwrap().clone().unwrap()
                }),
            ],
        );
        let outer = tempfile::tempdir().unwrap();
        let outer_directory = lillux::PinnedDirectory::open(outer.path())
            .unwrap()
            .unwrap();
        outer_directory.tighten_owner_private_directory().unwrap();
        let captured_network_inputs = test_network_inputs(&bootstrap);
        let journal = PreparedExternalSupervisorJournal::create(
            outer_directory,
            bootstrap,
            captured_network_inputs,
            lillux::crypto::SigningKey::from_bytes(&[53; 32]),
        )
        .unwrap();
        let store_identity = journal.store_identity().clone();
        assert!(
            attach_external_execution_channel_exact(
                journal.bootstrap(),
                journal.supervisor_signing_key(),
                journal.attachment_request(),
                journal.captured_network_inputs(),
            )
            .is_err()
        );
        drop(journal);
        std::thread::sleep(Duration::from_millis(150));
        let ExternalSupervisorJournalRecovery::Prepared(recovered) =
            ExternalSupervisorJournalRecovery::open(
                lillux::PinnedDirectory::open(outer.path())
                    .unwrap()
                    .unwrap(),
                &store_identity,
            )
            .unwrap()
        else {
            panic!("ambiguous attachment did not recover its prepared authority")
        };
        assert!(
            i64::try_from(lillux::time::timestamp_millis()).unwrap()
                >= recovered.bootstrap().attachment_deadline_ms
        );
        let (binding, _) = attach_external_execution_channel_exact(
            recovered.bootstrap(),
            recovered.supervisor_signing_key(),
            recovered.attachment_request(),
            recovered.captured_network_inputs(),
        )
        .unwrap();
        assert_eq!(
            binding.supervisor_public_key,
            encode_channel_public_key(&recovered.supervisor_signing_key().verifying_key()).unwrap()
        );
        recovered.record_binding(binding).unwrap();
        assert_eq!(server.join().unwrap(), 2);
    }

    #[test]
    fn exchange_uses_shared_wire_decoder_and_rejects_malformed_frames() {
        for malformed in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let bootstrap = bootstrap(
                format!("https://localhost:{port}"),
                vec![TEST_CA_DER.to_owned()],
            );
            let server_bootstrap = serde_json::to_value(&bootstrap).unwrap();
            let server = serve_tls(
                listener,
                TEST_SERVER_DER,
                vec![
                    Box::new(move |request| {
                        attach_success_response(server_bootstrap.clone(), request)
                    }),
                    Box::new(move |request| {
                        assert!(
                            std::str::from_utf8(&request).unwrap().starts_with(
                                "POST /external-execution/channel/exchange HTTP/1.1\r\n"
                            )
                        );
                        let exchange: ExternalChannelExchangeRequest =
                            serde_json::from_slice(request_body(&request)).unwrap();
                        exchange.validate_shape().unwrap();
                        let payload = br#"{"owner":"frame"}"#;
                        let frame = if malformed {
                            ExternalChannelResponseFrame {
                                sequence: 1,
                                frame_digest: lillux::sha256_hex(payload),
                                frame_base64: "e30".into(),
                            }
                        } else {
                            ExternalChannelResponseFrame::new(
                                1,
                                lillux::sha256_hex(payload),
                                payload,
                            )
                            .unwrap()
                        };
                        let response = ExternalChannelExchangeResponse {
                            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
                            incoming_new: true,
                            incoming_sequence: 1,
                            incoming_frame_digest: lillux::sha256_hex(
                                &exchange.decode_frame().unwrap(),
                            ),
                            acknowledgement_frame_digest: None,
                            outbound_frames: vec![frame],
                            urgent_revocation_frame: None,
                        };
                        http_response("200 OK", &canonical_request_bytes(&response).unwrap(), "")
                    }),
                ],
            );
            let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
            let (_, mut channel) = attach_external_execution_channel(
                &bootstrap,
                &supervisor_key,
                &test_network_inputs(&bootstrap),
            )
            .unwrap();
            let result = channel.exchange_until(
                br#"{"supervisor":"frame"}"#,
                lillux::time::MonotonicDeadline::after(Duration::from_secs(2)),
            );
            if malformed {
                assert!(matches!(
                    result,
                    Err(ExternalTransportStepFailure::Fatal(_))
                ));
            } else {
                let result = result.unwrap();
                assert_eq!(result.outbound_frames.len(), 1);
                assert_eq!(
                    result.outbound_frames[0].canonical_wire,
                    br#"{"owner":"frame"}"#
                );
            }
            assert_eq!(server.join().unwrap(), 2);
        }
    }

    #[test]
    fn exchange_classifies_protocol_refusal_separately_from_transport_ambiguity() {
        for response in [
            http_response("503 Service Unavailable", b"{}", ""),
            http_response("200 OK", b"{", ""),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\nConnection: close\r\n\r\n".to_vec(),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let bootstrap = bootstrap(
                format!("https://localhost:{port}"),
                vec![TEST_CA_DER.to_owned()],
            );
            let server_bootstrap = serde_json::to_value(&bootstrap).unwrap();
            let server = serve_tls(
                listener,
                TEST_SERVER_DER,
                vec![
                    Box::new(move |request| {
                        attach_success_response(server_bootstrap.clone(), request)
                    }),
                    Box::new(move |_| response.clone()),
                ],
            );
            let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
            let (_, mut channel) = attach_external_execution_channel(
                &bootstrap,
                &supervisor_key,
                &test_network_inputs(&bootstrap),
            )
            .unwrap();
            assert!(matches!(
                channel.exchange_until(
                    br#"{"supervisor":"frame"}"#,
                    lillux::time::MonotonicDeadline::after(Duration::from_secs(2)),
                ),
                Err(ExternalTransportStepFailure::Fatal(_))
            ));
            assert_eq!(server.join().unwrap(), 2);
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bootstrap = bootstrap(
            format!("https://localhost:{port}"),
            vec![TEST_CA_DER.to_owned()],
        );
        let server_bootstrap = serde_json::to_value(&bootstrap).unwrap();
        let server = serve_tls(
            listener,
            TEST_SERVER_DER,
            vec![
                Box::new(move |request| attach_success_response(server_bootstrap.clone(), request)),
                Box::new(|_| Vec::new()),
            ],
        );
        let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
        let (_, mut channel) = attach_external_execution_channel(
            &bootstrap,
            &supervisor_key,
            &test_network_inputs(&bootstrap),
        )
        .unwrap();
        assert!(matches!(
            channel.exchange_until(
                br#"{"supervisor":"frame"}"#,
                lillux::time::MonotonicDeadline::after(Duration::from_secs(2)),
            ),
            Err(ExternalTransportStepFailure::AmbiguousTransport(_))
        ));
        assert_eq!(server.join().unwrap(), 2);
    }

    #[test]
    fn ambient_proxy_environment_is_ignored() {
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let origin = format!("https://localhost:{port}");
        let bootstrap = bootstrap(origin.clone(), vec![TEST_CA_DER.to_owned()]);
        let server_bootstrap = serde_json::to_value(&bootstrap).unwrap();
        let server = serve_tls(
            listener,
            TEST_SERVER_DER,
            vec![Box::new(move |request| {
                attach_success_response(server_bootstrap.clone(), request)
            })],
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--ignored")
            .arg("--exact")
            .arg("tests::proxy_environment_child")
            .arg("--test-threads=1")
            .env_clear()
            .env("RYEOS_TEST_CONTROLLER_ORIGIN", origin)
            .env("HTTPS_PROXY", &proxy_url)
            .env("https_proxy", &proxy_url)
            .env("ALL_PROXY", &proxy_url)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "proxy child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(server.join().unwrap(), 1);
        proxy.set_nonblocking(true).unwrap();
        assert_eq!(
            proxy.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    #[ignore = "subprocess-only proxy isolation probe"]
    fn proxy_environment_child() {
        let origin = std::env::var("RYEOS_TEST_CONTROLLER_ORIGIN").unwrap();
        let bootstrap = bootstrap(origin, vec![TEST_CA_DER.to_owned()]);
        let supervisor_key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
        attach_external_execution_channel(
            &bootstrap,
            &supervisor_key,
            &test_network_inputs(&bootstrap),
        )
        .unwrap();
    }
}
