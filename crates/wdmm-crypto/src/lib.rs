//! `wdmm-crypto` — AES-256-GCM credential vault + backup envelope.
//!
//! Aligns with `openlist_crypt` format for interoperability.

#![forbid(unsafe_code)]
#![allow(dead_code)]

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
