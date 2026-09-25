//! Ephemeral key agreement including host entropy ownership. Protocol names,
//! negotiation and certificate policy remain outside this capability.

use ring::agreement;

#[derive(Debug, Clone, Copy)]
pub enum AgreementCurve {
    X25519,
    P256,
    P384,
}

impl AgreementCurve {
    fn algorithm(self) -> &'static agreement::Algorithm {
        match self {
            Self::X25519 => &agreement::X25519,
            Self::P256 => &agreement::ECDH_P256,
            Self::P384 => &agreement::ECDH_P384,
        }
    }
}

pub struct EphemeralAgreement {
    curve: AgreementCurve,
    secret: agreement::EphemeralPrivateKey,
    public: agreement::PublicKey,
}

impl EphemeralAgreement {
    pub fn generate(curve: AgreementCurve) -> anyhow::Result<Self> {
        let secret = agreement::EphemeralPrivateKey::generate(
            curve.algorithm(),
            &ring::rand::SystemRandom::new(),
        )
        .map_err(|_| anyhow::anyhow!("ephemeral key generation failed"))?;
        let public = secret
            .compute_public_key()
            .map_err(|_| anyhow::anyhow!("ephemeral public key derivation failed"))?;
        Ok(Self {
            curve,
            secret,
            public,
        })
    }

    pub fn public_key(&self) -> &[u8] {
        self.public.as_ref()
    }

    pub fn complete(self, peer: &[u8]) -> anyhow::Result<zeroize::Zeroizing<Vec<u8>>> {
        let peer = agreement::UnparsedPublicKey::new(self.curve.algorithm(), peer);
        agreement::agree_ephemeral(self.secret, &peer, |bytes| {
            zeroize::Zeroizing::new(bytes.to_vec())
        })
        .map_err(|_| anyhow::anyhow!("invalid key agreement peer"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ephemeral_agreement_matches_and_rejects_invalid_peer() {
        for curve in [
            AgreementCurve::X25519,
            AgreementCurve::P256,
            AgreementCurve::P384,
        ] {
            let a = EphemeralAgreement::generate(curve).unwrap();
            let b = EphemeralAgreement::generate(curve).unwrap();
            let ap = a.public_key().to_vec();
            let bp = b.public_key().to_vec();
            assert_eq!(*a.complete(&bp).unwrap(), *b.complete(&ap).unwrap());
            assert!(
                EphemeralAgreement::generate(curve)
                    .unwrap()
                    .complete(&[0; 32])
                    .is_err()
            );
        }
    }
}
