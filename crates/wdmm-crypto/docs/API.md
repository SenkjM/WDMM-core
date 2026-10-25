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

---

## Rclone crypt (`wdmm_crypto::rclone`)

### `enum RcloneError`
`Crypto(String)`, `NameEncoding`, `InvalidContent`, `UnsupportedMode`, `InvalidConfig(String)`.

### `type Result<T> = std::result::Result<T, RcloneError>`

### `enum FilenameEncryption`
`Off`, `Standard`, `Obfuscate`.

### `enum FilenameEncoding`
`Base32`, `Base64`, `Base32768`.

### `struct RcloneCipher`
Rclone crypt protocol: AES-256-CTR for content, AES-256-SIV for filenames.
Constructed via `RcloneCipher::new(password, salt, filename_encryption, filename_encoding, directory_name_encryption)`.

Key methods:
- `encrypt_name(&self, plaintext: &str) -> Result<String>` — encrypt a file or directory name.
- `decrypt_name(&self, ciphertext: &str, is_dir: bool) -> Result<String>` — decrypt a name; `is_dir` affects suffix handling.
- `encrypt_data(&self, plaintext: &[u8]) -> Result<Vec<u8>>` — encrypt file content (prepends header).
- `decrypt_data(&self, ciphertext: &[u8]) -> Result<Vec<u8>>` — decrypt full content.
- `decrypt_range(&self, ciphertext: &[u8], offset: u64, len: usize) -> Result<Vec<u8>>` — decrypt a plaintext range by reading needed ciphertext blocks.
- `decrypted_size(&self, ciphertext_size: u64) -> u64` — compute plaintext size from ciphertext size.
- `encrypted_size(&self, plaintext_size: u64) -> u64` — compute ciphertext size from plaintext size.

Constants:
- `FILE_HEADER_SIZE`: 8 bytes (magic).
- `BLOCK_SIZE`: 65536 bytes plaintext per block.

See `/workspace/agent-tools/wdmm-crypt-proxy-path.md` for integration with `CryptDriver`.

---

## Netease Music crypto (`wdmm_crypto::netease`)

### `enum NeteaseError`
`Crypto(String)`, `InvalidInput(String)`.

### `type Result<T> = std::result::Result<T, NeteaseError>`

### `fn weapi_encrypt(data: &serde_json::Value) -> Result<String>`
Encrypt a JSON payload for Netease weapi endpoints.
Returns `params=<encrypted>` form body string (UTF-8).
Uses AES-128-CBC with PKCS7 padding (two rounds: random key, then fixed key) + base64.
Golden test vector in `/workspace/WMM-dlna-ui/test/netease_music_crypto_test.dart`.

### `fn linuxapi_encrypt(data: &serde_json::Value) -> Result<String>`
Encrypt a JSON payload for Netease linuxapi endpoints.
Returns `eparams=<hex>` form body string (UTF-8).
Uses AES-128-ECB with PKCS7 padding + RSA (modulus from official client) + hex encoding.
Golden test vector in same Dart test file.

IV for both: ASCII literal `b"0102030405060708"` (16 bytes).
