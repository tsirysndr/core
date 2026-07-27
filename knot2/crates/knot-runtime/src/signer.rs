use k256::ecdsa::signature::{Signer as _, Verifier as _};
use k256::ecdsa::{Signature as K256Signature, SigningKey, VerifyingKey};

use crate::Entropy;

pub const MAX_SCALAR_ATTEMPTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature(Vec<u8>);

impl Signature {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKeyBytes(Vec<u8>);

impl PublicKeyBytes {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SignerError {
    #[error("invalid signing key bytes")]
    InvalidKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureScheme {
    Secp256k1,
    P256,
}

pub trait Signer: Send + Sync + 'static {
    fn sign(&self, message: &[u8]) -> Signature;
    fn public_key(&self) -> PublicKeyBytes;
    fn scheme(&self) -> SignatureScheme;
}

pub struct K256Signer {
    key: SigningKey,
}

impl K256Signer {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, SignerError> {
        SigningKey::from_slice(bytes)
            .map(|key| Self { key })
            .map_err(|_| SignerError::InvalidKey)
    }

    // peak dice rolling
    pub fn generate(entropy: &dyn Entropy) -> Self {
        std::iter::repeat_with(|| {
            let mut bytes = [0u8; 32];
            entropy.fill(&mut bytes);
            SigningKey::from_slice(&bytes).ok()
        })
        .take(MAX_SCALAR_ATTEMPTS)
        .flatten()
        .next()
        .map(|key| Self { key })
        .unwrap_or_else(|| {
            panic!(
                "entropy failed to yield valid secp256k1 scalar in {MAX_SCALAR_ATTEMPTS} attempts"
            )
        })
    }
}

impl Signer for K256Signer {
    fn sign(&self, message: &[u8]) -> Signature {
        let signature: K256Signature = self.key.sign(message);
        Signature(signature.to_bytes().to_vec())
    }

    fn public_key(&self) -> PublicKeyBytes {
        let point = self.key.verifying_key().to_encoded_point(true);
        PublicKeyBytes(point.as_bytes().to_vec())
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::Secp256k1
    }
}

pub fn verify(public_key: &PublicKeyBytes, message: &[u8], signature: &Signature) -> bool {
    let Ok(verifying_key) = VerifyingKey::from_sec1_bytes(public_key.as_bytes()) else {
        return false;
    };
    let Ok(parsed) = K256Signature::from_slice(signature.as_bytes()) else {
        return false;
    };
    verifying_key.verify(message, &parsed).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SeededEntropy;

    #[test]
    fn generate_is_deterministic_from_seed() {
        let one = K256Signer::generate(&SeededEntropy::new(99));
        let two = K256Signer::generate(&SeededEntropy::new(99));
        assert_eq!(one.public_key(), two.public_key());
    }

    struct BrokenEntropy;

    impl crate::Entropy for BrokenEntropy {
        fn next_u64(&self) -> u64 {
            0
        }

        fn fill(&self, buffer: &mut [u8]) {
            buffer.fill(0);
        }

        fn derive(&self, _label: u64) -> Box<dyn crate::Entropy> {
            Box::new(BrokenEntropy)
        }
    }

    #[test]
    #[should_panic(expected = "entropy failed to yield valid secp256k1 scalar")]
    fn broken_entropy_fails_stop_instead_of_spinning() {
        let _ = K256Signer::generate(&BrokenEntropy);
    }
}
