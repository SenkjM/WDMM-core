//! `AESGCMv1:` field encryption shared by the credential vault and backups.
//!
//! Wire format (must stay byte-compatible with the Android client):
//!
//! ```text
//! AESGCMv1:<base64(salt16 | nonce12 | ciphertext | tag16)>
//! ```
//!
//! The key comes from PBKDF2-HMAC-SHA256 over the passphrase with 120 000
//! iterations. `backup.rs` and the credential-vault FFI both call this module —
//! it is the only implementation of the wire format in the Rust tree.

use crate::{SyncError, SyncResult};

/// Field ciphertext prefix. An unprefixed value is plaintext.
pub const PREFIX: &str = "AESGCMv1:";

/// PBKDF2 iteration count for field and envelope keys (Android parity).
pub(crate) const KDF_ITERATIONS: u32 = 120_000;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MIN_WIRE_LEN: usize = SALT_LEN + NONCE_LEN + TAG_LEN;

/// Machine token for "the payload is not a well-formed ciphertext".
pub(crate) const REASON_DAMAGED: &str = "damaged";
/// Machine token for "the payload decrypted to nothing: wrong passphrase".
pub(crate) const REASON_WRONG_PASSPHRASE: &str = "wrong_passphrase";

/// Whether `value` carries the vault ciphertext prefix.
pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(PREFIX)
}

/// Encrypt one field value. Always encrypts, even with an empty passphrase.
pub fn encrypt_field(plaintext: &str, passphrase: &str) -> Result<String, String> {
    let wire = seal(plaintext.as_bytes(), passphrase, KDF_ITERATIONS)?;
    Ok(format!(
        "{PREFIX}{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, wire)
    ))
}

/// Decrypt one field value.
///
/// An unprefixed value is not ciphertext at all: it is returned unchanged, so
/// callers never have to test the prefix first. Failures are machine tokens:
/// [`REASON_WRONG_PASSPHRASE`] when the tag does not verify, [`REASON_DAMAGED`]
/// when the payload is not even structurally a ciphertext.
pub fn decrypt_field(encoded: &str, passphrase: &str) -> Result<String, String> {
    if !is_encrypted(encoded) {
        return Ok(encoded.to_string());
    }
    let wire = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &encoded[PREFIX.len()..],
    )
    .map_err(|_| REASON_DAMAGED.to_string())?;
    let plaintext = open(&wire, passphrase, KDF_ITERATIONS)?;
    String::from_utf8(plaintext).map_err(|_| REASON_DAMAGED.to_string())
}

/// Seal `plaintext` into `salt16 | nonce12 | ciphertext | tag16`.
///
/// Shared with the backup envelope, which prefixes the same wire with its own
/// magic instead of [`PREFIX`].
pub(crate) fn seal(
    plaintext: &[u8],
    passphrase: &str,
    iterations: u32,
) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use pbkdf2::pbkdf2_hmac;
    use rand::RngCore;
    use sha2::Sha256;

    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), &salt, iterations, &mut key);
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| format!("cipher init: {e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| format!("encrypt: {e}"))?;

    let mut wire = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    wire.extend_from_slice(&salt);
    wire.extend_from_slice(&nonce_bytes);
    wire.extend_from_slice(&ciphertext);
    Ok(wire)
}

/// Open a `salt16 | nonce12 | ciphertext | tag16` wire produced by [`seal`].
pub(crate) fn open(
    wire: &[u8],
    passphrase: &str,
    iterations: u32,
) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use pbkdf2::pbkdf2_hmac;
    use sha2::Sha256;

    if wire.len() < MIN_WIRE_LEN {
        return Err(REASON_DAMAGED.to_string());
    }
    let salt = &wire[0..SALT_LEN];
    let nonce_bytes = &wire[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ciphertext = &wire[SALT_LEN + NONCE_LEN..];

    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, iterations, &mut key);
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| format!("cipher init: {e}"))?;
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| REASON_WRONG_PASSPHRASE.to_string())
}

/// Map a field error token onto the crate error type.
pub(crate) fn field_error(reason: String) -> SyncError {
    SyncError::Other(reason)
}

/// Convenience wrapper used by `backup.rs` where a `SyncResult` is expected.
pub(crate) fn encrypt_field_sync(plaintext: &str, passphrase: &str) -> SyncResult<String> {
    encrypt_field(plaintext, passphrase).map_err(field_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let encrypted = encrypt_field("secret", "pw").unwrap();
        assert!(encrypted.starts_with(PREFIX));
        assert_eq!(decrypt_field(&encrypted, "pw").unwrap(), "secret");
    }

    #[test]
    fn encrypt_field_always_encrypts() {
        let encrypted = encrypt_field("secret", "").unwrap();
        assert!(is_encrypted(&encrypted));
        assert_eq!(decrypt_field(&encrypted, "").unwrap(), "secret");
    }

    #[test]
    fn wrong_passphrase_is_reported_as_such() {
        let encrypted = encrypt_field("secret", "pw").unwrap();
        assert_eq!(
            decrypt_field(&encrypted, "nope").unwrap_err(),
            REASON_WRONG_PASSPHRASE
        );
    }

    #[test]
    fn damaged_payload_does_not_panic() {
        assert_eq!(
            decrypt_field("AESGCMv1:not-base64!!", "pw").unwrap_err(),
            REASON_DAMAGED
        );
        assert_eq!(
            decrypt_field("AESGCMv1:AAAA", "pw").unwrap_err(),
            REASON_DAMAGED
        );
        let mut truncated = encrypt_field("secret", "pw").unwrap();
        truncated.truncate(truncated.len() - 8);
        assert!(decrypt_field(&truncated, "pw").is_err());
    }

    #[test]
    fn unprefixed_values_pass_through() {
        assert!(!is_encrypted("plain"));
        assert_eq!(decrypt_field("plain", "").unwrap(), "plain");
        assert_eq!(decrypt_field("plain", "pw").unwrap(), "plain");
    }

    #[test]
    fn different_salts_produce_different_ciphertexts() {
        let first = encrypt_field("secret", "pw").unwrap();
        let second = encrypt_field("secret", "pw").unwrap();
        assert_ne!(first, second);
    }
}
