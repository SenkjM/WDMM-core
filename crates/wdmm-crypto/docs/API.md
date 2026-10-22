# wdmm-crypto — Public API

AES-256-GCM credential vault plus a JSON credential-map encoder.
Format aligns with `openlist_crypt` for interoperability.

---

## Error types

### `enum CryptoError`
`DecryptionFailed(String)`, `EncryptionFailed(String)`,
`InvalidPassphrase`, `InvalidFormat`.

### `type Result<T> = std::result::Result<T, CryptoError>`

---

## Credential vault (`wdmm_crypto::vault`)

### `struct CredentialVault`
Encrypt/decrypt helper. All methods are associated functions (no instance state).

### `fn CredentialVault::encrypt(plaintext: &[u8], passphrase: &str) -> Result<Vec<u8>>`
Encrypt with AES-256-GCM using a PBKDF2-SHA256 key (600_000 iterations).

Wire format: `salt(16) || nonce(12) || ciphertext || tag(16)`.

Errors: `EncryptionFailed` on cipher setup or seal failure.

### `fn CredentialVault::decrypt(encrypted: &[u8], passphrase: &str) -> Result<Vec<u8>>`
Decrypt a vault blob produced by `encrypt`.

Errors:
- `InvalidFormat` if input is shorter than `salt + nonce` (28 bytes).
- `DecryptionFailed` on wrong passphrase, corrupt ciphertext, or tag mismatch.

---

## Credential encoder

### `struct CredentialEncoder`
Bridge between `CloudAccount.credentials_encrypted` (a `Vec<u8>` field) and the
per-driver `HashMap<String, String>` config stored inside it.

### `fn CredentialEncoder::encode(data: &HashMap<String, String>, passphrase: &str) -> Result<Vec<u8>>`
JSON-serialise the map, then vault-encrypt. Returns the ciphertext blob.

### `fn CredentialEncoder::decode(encrypted: &[u8], passphrase: &str) -> Result<HashMap<String, String>>`
Vault-decrypt, then JSON-deserialise back to the map.

Errors: `EncryptionFailed`/`DecryptionFailed` propagate from the vault;
`DecryptionFailed` also covers a non-UTF-8/invalid JSON plaintext.
