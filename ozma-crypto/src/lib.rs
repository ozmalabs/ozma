//! ozma-crypto — XChaCha20-Poly1305 AEAD helpers.

use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    XChaCha20Poly1305, XNonce,
};
use thiserror::Error;

/// Errors from encrypt / decrypt operations.
#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("AEAD encryption failed")]
    Encrypt,
    #[error("AEAD decryption failed (bad key or tampered ciphertext)")]
    Decrypt,
    #[error("ciphertext too short to contain nonce")]
    TooShort,
}

/// A thin wrapper around an XChaCha20-Poly1305 key.
pub struct OzmaCipher {
    inner: XChaCha20Poly1305,
}

impl OzmaCipher {
    /// Create a cipher from a 32-byte key.
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            inner: XChaCha20Poly1305::new(key.into()),
        }
    }

    /// Generate a fresh random 32-byte key.
    pub fn generate_key() -> [u8; 32] {
        let key = XChaCha20Poly1305::generate_key(&mut OsRng);
        key.into()
    }

    /// Encrypt `plaintext` with a random nonce.
    ///
    /// Returns `nonce (24 bytes) || ciphertext`.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .inner
            .encrypt(&nonce, plaintext)
            .map_err(|_| CryptoError::Encrypt)?;
        let mut out = Vec::with_capacity(24 + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt a blob produced by [`encrypt`].
    ///
    /// Expects `nonce (24 bytes) || ciphertext`.
    pub fn decrypt(&self, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if blob.len() < 24 {
            return Err(CryptoError::TooShort);
        }
        let (nonce_bytes, ciphertext) = blob.split_at(24);
        let nonce = XNonce::from_slice(nonce_bytes);
        self.inner
            .decrypt(nonce, ciphertext)
            .map_err(|_| CryptoError::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = OzmaCipher::generate_key();
        let cipher = OzmaCipher::new(&key);
        let plaintext = b"hello ozma";
        let blob = cipher.encrypt(plaintext).unwrap();
        let recovered = cipher.decrypt(&blob).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = OzmaCipher::generate_key();
        let cipher = OzmaCipher::new(&key);
        let mut blob = cipher.encrypt(b"secret").unwrap();
        // Flip a byte in the ciphertext portion.
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(cipher.decrypt(&blob).is_err());
    }
}
