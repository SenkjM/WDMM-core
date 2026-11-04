//! Host integration tests for the `wdmm_vault_crypto_*` C ABI (slice 9).
//!
//! The exports are declared `extern "C"` here on purpose: that is the exact ABI
//! `dart:ffi` binds to, so a drifted symbol name or signature fails loudly.

use std::ffi::{c_char, CStr, CString};

use serde_json::Value;
use wdmm_ffi::FfiString;

extern "C" {
    fn wdmm_vault_crypto_encrypt(plaintext: *const c_char, passphrase: *const c_char) -> FfiString;
    fn wdmm_vault_crypto_decrypt(value: *const c_char, passphrase: *const c_char) -> FfiString;
    fn wdmm_vault_crypto_is_encrypted(value: *const c_char) -> FfiString;
    fn wdmm_ffi_free_string(value: FfiString);
}

/// Read one raw reply string and free the backing string.
fn raw(value: FfiString) -> String {
    let text = unsafe { CStr::from_ptr(value.ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { wdmm_ffi_free_string(value) };
    text
}

/// Read one JSON reply and free the backing string.
fn reply(value: FfiString) -> Value {
    let text = raw(value);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("reply must be JSON ({e}): {text}"))
}

fn c(value: &str) -> CString {
    CString::new(value).expect("no interior nul")
}

fn encrypt(plaintext: &str, passphrase: &str) -> Value {
    let (plaintext, passphrase) = (c(plaintext), c(passphrase));
    reply(unsafe { wdmm_vault_crypto_encrypt(plaintext.as_ptr(), passphrase.as_ptr()) })
}

fn decrypt(value: &str, passphrase: &str) -> Value {
    let (value, passphrase) = (c(value), c(passphrase));
    reply(unsafe { wdmm_vault_crypto_decrypt(value.as_ptr(), passphrase.as_ptr()) })
}

fn is_encrypted(value: &str) -> bool {
    let value = c(value);
    let reply = reply(unsafe { wdmm_vault_crypto_is_encrypted(value.as_ptr()) });
    assert_eq!(reply["ok"], true, "is_encrypted must succeed: {reply}");
    reply["encrypted"].as_bool().expect("encrypted is a bool")
}

#[test]
fn encrypt_then_decrypt_round_trips() {
    let encrypted = encrypt("hunter2", "pw");
    assert_eq!(encrypted["ok"], true);
    let ciphertext = encrypted["value"].as_str().expect("value string");
    assert!(ciphertext.starts_with("AESGCMv1:"));

    let plaintext = decrypt(ciphertext, "pw");
    assert_eq!(plaintext["ok"], true);
    assert_eq!(plaintext["plaintext"], "hunter2");
}

#[test]
fn encrypt_always_encrypts_even_with_an_empty_passphrase() {
    let encrypted = encrypt("secret", "");
    let ciphertext = encrypted["value"].as_str().expect("value string");
    assert!(ciphertext.starts_with("AESGCMv1:"));
    let plaintext = decrypt(ciphertext, "");
    assert_eq!(plaintext["plaintext"], "secret");
}

#[test]
fn wrong_passphrase_reports_the_reason() {
    let encrypted = encrypt("secret", "pw");
    let ciphertext = encrypted["value"].as_str().expect("value string");
    let plaintext = decrypt(ciphertext, "other");
    assert_eq!(plaintext["ok"], false);
    assert_eq!(plaintext["reason"], "wrong_passphrase");
}

#[test]
fn damaged_payload_reports_the_reason() {
    let plaintext = decrypt("AESGCMv1:not-base64!!", "pw");
    assert_eq!(plaintext["ok"], false);
    assert_eq!(plaintext["reason"], "damaged");
}

#[test]
fn unprefixed_values_pass_through() {
    let plaintext = decrypt("plain text", "pw");
    assert_eq!(plaintext["ok"], true);
    assert_eq!(plaintext["plaintext"], "plain text");
}

#[test]
fn is_encrypted_follows_the_prefix() {
    assert!(!is_encrypted("hunter2"));
    let encrypted = encrypt("hunter2", "pw");
    assert!(is_encrypted(encrypted["value"].as_str().unwrap()));
}

#[test]
fn missing_arguments_report_invalid_input() {
    let value = c("hunter2");
    let text = raw(unsafe { wdmm_vault_crypto_encrypt(std::ptr::null(), value.as_ptr()) });
    assert_eq!(text, "error:invalid plaintext");
}
