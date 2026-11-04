//! Credential-vault field crypto (`AESGCMv1:`) over the C ABI.
//!
//! The wire format lives in `crates/wdmm-sync/src/vault_crypto.rs` (re-exported
//! as [`wdmm_sync::encrypt_field`], [`wdmm_sync::decrypt_field`] and
//! [`wdmm_sync::is_encrypted`]); this module only marshals arguments and reports
//! machine-readable reasons so the Dart layer never has to implement
//! PBKDF2/AES-GCM itself.

use std::os::raw::c_char;

use super::require_arg;
use crate::{ptr_to_str, FfiString};

/// Encrypt one credential field.
///
/// Reply: `{"ok":true,"value":"AESGCMv1:…"}` or `{"ok":false,"reason":…}`.
#[no_mangle]
pub extern "C" fn wdmm_vault_crypto_encrypt(
    plaintext: *const c_char,
    passphrase: *const c_char,
) -> FfiString {
    let plaintext = match require_arg(plaintext, "plaintext") {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    let passphrase = ptr_to_str(passphrase).unwrap_or("");
    let value = match wdmm_sync::encrypt_field(plaintext, passphrase) {
        Ok(value) => serde_json::json!({ "ok": true, "value": value }),
        Err(reason) => serde_json::json!({ "ok": false, "reason": reason }),
    };
    FfiString::from(value.to_string())
}

/// Decrypt one credential field.
///
/// Reply: `{"ok":true,"plaintext":"…"}` (also for unprefixed plaintext input) or
/// `{"ok":false,"reason":"wrong_passphrase"|"damaged"}`.
#[no_mangle]
pub extern "C" fn wdmm_vault_crypto_decrypt(
    value: *const c_char,
    passphrase: *const c_char,
) -> FfiString {
    let value = match require_arg(value, "value") {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    let passphrase = ptr_to_str(passphrase).unwrap_or("");
    let reply = match wdmm_sync::decrypt_field(value, passphrase) {
        Ok(plaintext) => serde_json::json!({ "ok": true, "plaintext": plaintext }),
        Err(reason) => serde_json::json!({ "ok": false, "reason": reason }),
    };
    FfiString::from(reply.to_string())
}

/// Whether a field value carries the `AESGCMv1:` prefix.
///
/// Reply: `{"ok":true,"encrypted":bool}`.
#[no_mangle]
pub extern "C" fn wdmm_vault_crypto_is_encrypted(value: *const c_char) -> FfiString {
    let value = match require_arg(value, "value") {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    let reply = serde_json::json!({
        "ok": true,
        "encrypted": wdmm_sync::is_encrypted(value),
    });
    FfiString::from(reply.to_string())
}
