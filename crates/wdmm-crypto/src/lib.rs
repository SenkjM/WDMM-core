//! `wdmm-crypto` — AES-256-GCM credential vault + backup envelope.
//!
//! Aligns with `openlist_crypt` format for interoperability.

#![forbid(unsafe_code)]
#![allow(dead_code)]

pub mod rclone;
pub mod vault;

pub use vault::CredentialVault;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("decryption failed: {0}")]
    DecryptionFailed(String),

    #[error("encryption failed: {0}")]
    EncryptionFailed(String),

    #[error("invalid passphrase")]
    InvalidPassphrase,

    #[error("invalid format")]
    InvalidFormat,
}

pub type Result<T> = std::result::Result<T, CryptoError>;

// ── CredentialEncoder ─────────────────────────────────────────────────────────

/// Encode/decode a `HashMap<String, String>` credential map through the vault.
///
/// This is the bridge between the application's `CloudAccount.credentials_encrypted`
/// byte field and the driver-specific key/value config stored inside it.
/// Typical use:
/// ```ignore
/// let encrypted = CredentialEncoder::encode(&cfg, passphrase)?;
/// account.credentials_encrypted = encrypted;
/// // …later…
/// let cfg = CredentialEncoder::decode(&account.credentials_encrypted, passphrase)?;
/// ```
pub struct CredentialEncoder;

impl CredentialEncoder {
    /// Serialize `data` as JSON then encrypt with the vault.
    pub fn encode(
        data: &std::collections::HashMap<String, String>,
        passphrase: &str,
    ) -> Result<Vec<u8>> {
        let json =
            serde_json::to_vec(data).map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;
        CredentialVault::encrypt(&json, passphrase)
    }

    /// Decrypt with the vault then deserialize JSON back to a map.
    pub fn decode(
        encrypted: &[u8],
        passphrase: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        let plaintext = CredentialVault::decrypt(encrypted, passphrase)?;
        serde_json::from_slice(&plaintext).map_err(|e| CryptoError::DecryptionFailed(e.to_string()))
    }
}

#[cfg(test)]
mod encoder_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn roundtrip_credential_map() {
        let mut cfg = HashMap::new();
        cfg.insert("refresh_token".into(), "rt_abc123".into());
        cfg.insert("client_id".into(), "my_client".into());
        cfg.insert("access_token".into(), "at_xyz789".into());

        let passphrase = "hunter2";
        let enc = CredentialEncoder::encode(&cfg, passphrase).unwrap();
        assert!(!enc.is_empty());

        let decoded = CredentialEncoder::decode(&enc, passphrase).unwrap();
        assert_eq!(decoded, cfg);
    }

    #[test]
    fn wrong_passphrase_returns_error() {
        let mut cfg = HashMap::new();
        cfg.insert("token".into(), "secret".into());

        let enc = CredentialEncoder::encode(&cfg, "correct").unwrap();
        assert!(CredentialEncoder::decode(&enc, "wrong").is_err());
    }

    #[test]
    fn credentials_encrypted_field_roundtrip() {
        // Simulate what CloudAccount.credentials_encrypted stores.
        let mut driver_cfg = HashMap::new();
        driver_cfg.insert("session".into(), "ct-session-token".into());
        driver_cfg.insert("cloud_type".into(), "public".into());

        let passphrase = "vault-master-key";
        let ciphertext = CredentialEncoder::encode(&driver_cfg, passphrase).unwrap();

        // Round-trip as if stored in DB then reloaded.
        let recovered = CredentialEncoder::decode(&ciphertext, passphrase).unwrap();
        assert_eq!(recovered["session"], "ct-session-token");
        assert_eq!(recovered["cloud_type"], "public");
    }
}
