//! Credential vault encryption using AES-256-GCM + PBKDF2-SHA256.
//!
//! Format: `salt(16) || nonce(12) || ciphertext || tag(16)`
//! PBKDF2 iterations: 600_000 (OWASP 2023 recommendation).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use pbkdf2::pbkdf2_hmac;
use rand::{Rng, RngCore};
use sha2::Sha256;
use zeroize::Zeroize;

use crate::{CryptoError, Result};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const PBKDF2_ITERATIONS: u32 = 600_000;

pub struct CredentialVault;

impl CredentialVault {
    /// Encrypt plaintext with passphrase.
    pub fn encrypt(plaintext: &[u8], passphrase: &str) -> Result<Vec<u8>> {
        let mut rng = rand::thread_rng();

        let mut salt = [0u8; SALT_LEN];
        rng.fill_bytes(&mut salt);

        let mut key = [0u8; KEY_LEN];
        pbkdf2_hmac::<Sha256>(
            passphrase.as_bytes(),
            &salt,
            PBKDF2_ITERATIONS,
            &mut key,
        );

        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;
        key.zeroize();

        let mut nonce_bytes = [0u8; NONCE_LEN];
        rng.fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

        let mut output = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
        output.extend_from_slice(&salt);
        output.extend_from_slice(&nonce_bytes);
        output.extend_from_slice(&ciphertext);

        Ok(output)
    }

    /// Decrypt with passphrase.
    pub fn decrypt(encrypted: &[u8], passphrase: &str) -> Result<Vec<u8>> {
        if encrypted.len() < SALT_LEN + NONCE_LEN {
            return Err(CryptoError::InvalidFormat);
        }

        let salt = &encrypted[..SALT_LEN];
        let nonce_bytes = &encrypted[SALT_LEN..SALT_LEN + NONCE_LEN];
        let ciphertext = &encrypted[SALT_LEN + NONCE_LEN..];

        let mut key = [0u8; KEY_LEN];
        pbkdf2_hmac::<Sha256>(
            passphrase.as_bytes(),
            salt,
            PBKDF2_ITERATIONS,
            &mut key,
        );

        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;
        key.zeroize();

        let nonce = Nonce::from_slice(nonce_bytes);
        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data = b"secret credential";
        let pass = "my-passphrase";
        let enc = CredentialVault::encrypt(data, pass).unwrap();
        let dec = CredentialVault::decrypt(&enc, pass).unwrap();
        assert_eq!(&dec, data);
    }

    #[test]
    fn wrong_passphrase() {
        let enc = CredentialVault::encrypt(b"data", "pass1").unwrap();
        assert!(CredentialVault::decrypt(&enc, "pass2").is_err());
    }
}
