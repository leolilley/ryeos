//! TLS policy translation over Lillux clock and cryptographic capabilities.
//! Ring remains the audited cipher/signature implementation. Its default key
//! exchange/random integrations are replaced so host entropy stays in Lillux.

use lillux::crypto::agreement::{AgreementCurve, EphemeralAgreement};
use rustls::crypto::{ActiveKeyExchange, CryptoProvider, SharedSecret, SupportedKxGroup};
use rustls::{Error, NamedGroup, PeerMisbehaved};
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct HostTime;
impl rustls::time_provider::TimeProvider for HostTime {
    fn current_time(&self) -> Option<rustls::pki_types::UnixTime> {
        let milliseconds = u64::try_from(lillux::time::timestamp_millis()).ok()?;
        Some(rustls::pki_types::UnixTime::since_unix_epoch(
            lillux::time::Duration::from_millis(milliseconds),
        ))
    }
}

#[derive(Debug)]
struct HostRandom;
impl rustls::crypto::SecureRandom for HostRandom {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), rustls::crypto::GetRandomFailed> {
        lillux::crypto::fill_random_bytes(bytes).map_err(|_| rustls::crypto::GetRandomFailed)
    }
}

#[derive(Debug)]
struct Group(AgreementCurve, NamedGroup);
impl SupportedKxGroup for Group {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        Ok(Box::new(Exchange(
            self.1,
            EphemeralAgreement::generate(self.0).map_err(|_| Error::FailedToGetRandomBytes)?,
        )))
    }
    fn name(&self) -> NamedGroup {
        self.1
    }
    fn ffdhe_group(&self) -> Option<rustls::ffdhe_groups::FfdheGroup<'static>> {
        None
    }
}

struct Exchange(NamedGroup, EphemeralAgreement);
impl ActiveKeyExchange for Exchange {
    fn pub_key(&self) -> &[u8] {
        self.1.public_key()
    }
    fn group(&self) -> NamedGroup {
        self.0
    }
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, Error> {
        // TLS negotiates only uncompressed SEC1 points for these curves.
        let valid = match self.0 {
            NamedGroup::X25519 => peer.len() == 32,
            NamedGroup::secp256r1 => peer.len() == 65 && peer.first() == Some(&4),
            NamedGroup::secp384r1 => peer.len() == 97 && peer.first() == Some(&4),
            _ => false,
        };
        if !valid {
            return Err(PeerMisbehaved::InvalidKeyShare.into());
        }
        let secret = self
            .1
            .complete(peer)
            .map_err(|_| Error::from(PeerMisbehaved::InvalidKeyShare))?;
        Ok(SharedSecret::from(secret.as_slice()))
    }
    fn ffdhe_group(&self) -> Option<rustls::ffdhe_groups::FfdheGroup<'static>> {
        None
    }
}

pub(crate) fn provider() -> Arc<CryptoProvider> {
    let mut provider = rustls::crypto::ring::default_provider();
    provider.secure_random = &HostRandom;
    provider.kx_groups = vec![
        &Group(AgreementCurve::X25519, NamedGroup::X25519),
        &Group(AgreementCurve::P256, NamedGroup::secp256r1),
        &Group(AgreementCurve::P384, NamedGroup::secp384r1),
    ];
    Arc::new(provider)
}
