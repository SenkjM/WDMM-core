//! CryptDriver — rclone crypt wrapper.
//!
//! Wraps another WdmmVfs, encrypting/decrypting filenames and content using
//! the rclone crypt format. All `get()` calls return `LocalStream` with plaintext
//! size; the `get_content` / `get_content_range` implementations decrypt blocks
//! from the source driver on demand.
//!
//! Supports both:
//! - Registry path with nested `source` config for testing
//! - `CryptDriver::with_source(Arc<dyn WdmmVfs>, CryptConfig)` for runtime

use crate::driver::{WdmmVfs, CloudFileItem, ContentDeliveryKind, DriverError, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wdmm_crypto::rclone::{NameEncoding, NameMode, RcloneCipher, RcloneError};

/// How long the ciphertext size and file nonce are reused for a path. Every
/// range read needs both; caching them turns a 3-request header+metadata dance
/// per 1 MiB chunk into a single range request.
const META_CACHE_TTL: Duration = Duration::from_secs(60);

/// Cached ciphertext metadata for one plaintext path.
struct CachedMeta {
    inner_size: u64,
    nonce: [u8; 24],
    expires: Instant,
}

/// Crypt driver configuration aligned with Dart CryptDriver.
#[derive(Debug, Clone)]
pub struct CryptConfig {
    pub password: String,
    pub salt: String,
    pub filename_encryption: NameMode,
    pub filename_encoding: NameEncoding,
    pub encrypted_suffix: String,
    pub directory_name_encryption: bool,
    pub source_dir: String,
}

impl CryptConfig {
    pub fn new(password: impl Into<String>) -> Self {
        Self {
            password: password.into(),
            salt: String::new(),
            filename_encryption: NameMode::Off,
            filename_encoding: NameEncoding::Base64,
            encrypted_suffix: ".bin".to_string(),
            directory_name_encryption: false,
            source_dir: "/".to_string(),
        }
    }
}

/// CryptDriver wraps a source driver and performs rclone crypt encryption/decryption.
pub struct CryptDriver {
    cipher: RcloneCipher,
    source: Arc<dyn WdmmVfs + Send + Sync>,
    config: CryptConfig,
    /// inner ciphertext path → size + 24-byte file nonce (the header is not
    /// part of the block data, so it would otherwise be re-read per range).
    meta_cache: Mutex<HashMap<String, CachedMeta>>,
}

impl CryptDriver {
    /// Create a CryptDriver wrapping the given source driver.
    pub fn with_source(source: Arc<dyn WdmmVfs + Send + Sync>, config: CryptConfig) -> Result<Self> {
        let cipher = RcloneCipher::new(
            &config.password,
            &config.salt,
            config.filename_encryption,
            config.directory_name_encryption,
            config.filename_encoding,
            &config.encrypted_suffix,
        )
        .map_err(|e| DriverError::Config(format!("crypt cipher init: {}", e)))?;

        Ok(Self {
            cipher,
            source,
            config,
            meta_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Return the cached ciphertext size and file nonce for `inner_path`.
    fn cached_meta(&self, inner_path: &str) -> Option<(u64, [u8; 24])> {
        let cache = self.meta_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(inner_path).and_then(|meta| {
            (meta.expires > Instant::now()).then_some((meta.inner_size, meta.nonce))
        })
    }

    /// Remember the ciphertext size and file nonce for `inner_path`.
    fn store_meta(&self, inner_path: &str, inner_size: u64, nonce: [u8; 24]) {
        let mut cache = self.meta_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(
            inner_path.to_string(),
            CachedMeta {
                inner_size,
                nonce,
                expires: Instant::now() + META_CACHE_TTL,
            },
        );
    }

    /// Map plaintext path → ciphertext path.
    ///
    /// For paths like `/dir1/dir2/file.txt`, each segment is encrypted
    /// (using `decrypt_dir_name` for directories, `decrypt_file_name` for the final segment).
    ///
    /// Empty and `.` segments are dropped before encryption: the UI names
    /// directories with a trailing slash (`/dir/`), and encrypting that empty
    /// segment produced a bogus ciphertext name, so the source driver was asked
    /// for a path that does not exist (Baidu answers `errno -9`).
    fn map_to_inner(&self, path: &str, last_is_file: bool) -> Result<String> {
        let parts: Vec<&str> = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();

        if parts.is_empty() {
            return Ok(self.config.source_dir.clone());
        }

        let mut inner_parts = Vec::new();

        for (i, part) in parts.iter().enumerate() {
            let is_last = i == parts.len() - 1;
            let encrypted = if is_last && last_is_file {
                self.cipher.encrypt_file_name(part)
            } else {
                self.cipher.encrypt_dir_name(part)
            }
            .map_err(|e| DriverError::Other(format!("encrypt name '{}': {}", part, e)))?;
            inner_parts.push(encrypted);
        }

        let base = self.config.source_dir.trim_end_matches('/');
        let inner = format!("{}/{}", base, inner_parts.join("/"));
        Ok(inner)
    }

    /// Map ciphertext path ← plaintext path (decrypt names).
    fn map_from_inner(&self, inner_path: &str, is_dir: bool) -> Result<String> {
        let base = self.config.source_dir.trim_end_matches('/');
        let relative = inner_path.strip_prefix(base).unwrap_or(inner_path);
        let relative = relative.trim_start_matches('/');

        if relative.is_empty() {
            return Ok("/".to_string());
        }

        let parts: Vec<&str> = relative.split('/').collect();
        let mut plain_parts = Vec::new();

        for (i, part) in parts.iter().enumerate() {
            let is_last = i == parts.len() - 1;
            let decrypted = if is_last && !is_dir {
                self.cipher.decrypt_file_name(part)
            } else {
                self.cipher.decrypt_dir_name(part)
            }
            .map_err(|e| DriverError::Other(format!("decrypt name '{}': {}", part, e)))?;
            plain_parts.push(decrypted);
        }

        Ok(format!("/{}", plain_parts.join("/")))
    }
}

// ── File structure constants ──
// From openlist_crypt/lib/src/rclone_cipher.dart
const FILE_HEADER_SIZE: u64 = 32; // 8 bytes magic + 24 bytes nonce
const BLOCK_HEADER_SIZE: u64 = 16; // poly1305 auth tag
const BLOCK_DATA_SIZE: u64 = 65536; // 64 KiB
const BLOCK_SIZE: u64 = BLOCK_HEADER_SIZE + BLOCK_DATA_SIZE; // 65552 bytes

#[async_trait]
impl WdmmVfs for CryptDriver {
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>> {
        let inner_path = self.map_to_inner(path, false)?;
        let inner_items = self.source.list(&inner_path).await?;

        let mut result = Vec::new();
        for item in inner_items {
            // Decrypt name
            let plain_name = if item.is_dir {
                self.cipher.decrypt_dir_name(&item.name)
            } else {
                self.cipher.decrypt_file_name(&item.name)
            };

            // Skip items that can't be decrypted (mixed dirs)
            let plain_name = match plain_name {
                Ok(name) => name,
                Err(RcloneError::NameEncoding) | Err(RcloneError::InvalidContent) => continue,
                Err(e) => return Err(DriverError::Other(format!("decrypt name: {}", e))),
            };

            // For files, compute plaintext size
            let size = if item.is_dir {
                0
            } else {
                RcloneCipher::decrypted_size(item.size)
                    .map_err(|e| DriverError::Other(format!("decrypt size: {}", e)))?
            };

            result.push(CloudFileItem {
                name: plain_name,
                is_dir: item.is_dir,
                size,
                modified: item.modified,
                raw_url: None,
                raw_headers: vec![],
                delivery: ContentDeliveryKind::LocalStream,
                local_path: None,
            });
        }

        Ok(result)
    }

    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        let inner_path = self.map_to_inner(path, true)?;
        let inner_item = self.source.get(&inner_path).await?;

        if inner_item.is_dir {
            return Err(DriverError::NotFound(path.to_string()));
        }

        let size = RcloneCipher::decrypted_size(inner_item.size)
            .map_err(|e| DriverError::Other(format!("decrypt size: {}", e)))?;

        let plain_name = self
            .cipher
            .decrypt_file_name(&inner_item.name)
            .map_err(|e| DriverError::Other(format!("decrypt name: {}", e)))?;

        Ok(CloudFileItem {
            name: plain_name,
            is_dir: false,
            size,
            modified: inner_item.modified,
            raw_url: None, // MustProxy — content must go through get_content/range
            raw_headers: vec![],
            delivery: ContentDeliveryKind::LocalStream,
            local_path: None,
        })
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let inner_path = self.map_to_inner(path, false)?;
        self.source.mkdir(&inner_path).await
    }

    async fn rename(&self, old: &str, new: &str) -> Result<()> {
        // Try file first, then directory if that fails
        let inner_old = self.map_to_inner(old, true)?;
        let inner_new = self.map_to_inner(new, true)?;

        match self.source.rename(&inner_old, &inner_new).await {
            Ok(()) => Ok(()),
            Err(_) => {
                // Retry as directory
                let inner_old = self.map_to_inner(old, false)?;
                let inner_new = self.map_to_inner(new, false)?;
                self.source.rename(&inner_old, &inner_new).await
            }
        }
    }

    async fn remove(&self, path: &str) -> Result<()> {
        // Try file first, then directory if that fails
        let inner_path = self.map_to_inner(path, true)?;

        match self.source.remove(&inner_path).await {
            Ok(()) => Ok(()),
            Err(_) => {
                // Retry as directory
                let inner_path = self.map_to_inner(path, false)?;
                self.source.remove(&inner_path).await
            }
        }
    }

    /// Read full decrypted content.
    async fn get_content(&self, path: &str) -> Result<Vec<u8>> {
        let inner_path = self.map_to_inner(path, true)?;

        // Fetch full ciphertext from source
        let ciphertext = self.source.get_content(&inner_path).await?;

        // Decrypt entire content
        self.cipher
            .decrypt(&ciphertext)
            .map_err(|e| DriverError::Other(format!("decrypt: {}", e)))
    }

    /// Read a byte range of decrypted content.
    ///
    /// Calculates which ciphertext blocks contain the requested plaintext range,
    /// fetches them from source, and decrypts the relevant portion.
    async fn get_content_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Vec<u8>> {
        let inner_path = self.map_to_inner(path, true)?;

        // Ciphertext size + file nonce. Both are stable for a file; caching
        // them removes a `get()` (list + filemetas + HEAD upstream) and a
        // 32-byte header read from every playback chunk request.
        let (inner_size, nonce) = match self.cached_meta(&inner_path) {
            Some(hit) => hit,
            None => {
                let inner_item = self.source.get(&inner_path).await?;
                let header = self
                    .source
                    .get_content_range(&inner_path, 0, Some(FILE_HEADER_SIZE - 1))
                    .await?;
                if header.len() < FILE_HEADER_SIZE as usize {
                    return Err(DriverError::Other("short file header".into()));
                }
                let mut nonce = [0u8; 24];
                nonce.copy_from_slice(&header[8..32]); // skip 8-byte magic
                self.store_meta(&inner_path, inner_item.size, nonce);
                (inner_item.size, nonce)
            }
        };

        let plaintext_size = RcloneCipher::decrypted_size(inner_size)
            .map_err(|e| DriverError::Other(format!("decrypt size: {}", e)))?;

        if start >= plaintext_size {
            return Ok(Vec::new());
        }

        let end = end.map(|e| e.min(plaintext_size - 1)).unwrap_or(plaintext_size - 1);

        // Calculate which plaintext blocks we need
        let first_block = start / BLOCK_DATA_SIZE;
        let last_block = end / BLOCK_DATA_SIZE;

        // Calculate ciphertext range to fetch
        // Each block: FILE_HEADER (once) + BLOCK_SIZE per block
        let cipher_start = FILE_HEADER_SIZE + first_block * BLOCK_SIZE;
        let mut cipher_end = FILE_HEADER_SIZE + (last_block + 1) * BLOCK_SIZE - 1;
        if cipher_end > inner_size - 1 {
            cipher_end = inner_size - 1;
        }

        // Fetch block data
        let block_data = self.source.get_content_range(&inner_path, cipher_start, Some(cipher_end)).await?;

        // Decrypt blocks
        let mut plaintext = Vec::new();
        let mut offset = 0;

        for block_idx in first_block..=last_block {
            let block_start = offset;
            let block_end = (offset + BLOCK_SIZE as usize).min(block_data.len());

            if block_start >= block_data.len() {
                break;
            }

            let cipher_block = &block_data[block_start..block_end];
            let plain_block = self
                .cipher
                .decrypt_block(&nonce, block_idx, cipher_block)
                .map_err(|e| DriverError::Other(format!("decrypt block {}: {}", block_idx, e)))?;

            // Calculate which portion of this block we need
            let block_plain_start = if block_idx == first_block {
                (start % BLOCK_DATA_SIZE) as usize
            } else {
                0
            };

            let block_plain_end = if block_idx == last_block {
                ((end % BLOCK_DATA_SIZE) as usize + 1).min(plain_block.len())
            } else {
                plain_block.len()
            };

            plaintext.extend_from_slice(&plain_block[block_plain_start..block_plain_end]);

            offset += BLOCK_SIZE as usize;
        }

        Ok(plaintext)
    }
}

// ────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// MockSource: in-memory driver for testing
    struct MockSource {
        files: HashMap<String, Vec<u8>>,
        get_calls: AtomicUsize,
        range_calls: AtomicUsize,
    }

    impl MockSource {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
                get_calls: AtomicUsize::new(0),
                range_calls: AtomicUsize::new(0),
            }
        }

        fn add_file(&mut self, path: impl Into<String>, data: Vec<u8>) {
            self.files.insert(path.into(), data);
        }

        fn get_calls(&self) -> usize {
            self.get_calls.load(Ordering::Relaxed)
        }

        fn range_calls(&self) -> usize {
            self.range_calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl WdmmVfs for MockSource {
        async fn list(&self, _path: &str) -> Result<Vec<CloudFileItem>> {
            Err(DriverError::Other("MockSource::list not implemented".into()))
        }

        async fn get(&self, path: &str) -> Result<CloudFileItem> {
            self.get_calls.fetch_add(1, Ordering::Relaxed);
            let data = self
                .files
                .get(path)
                .ok_or_else(|| DriverError::NotFound(path.to_string()))?;

            Ok(CloudFileItem {
                name: path.split('/').next_back().unwrap_or("").to_string(),
                is_dir: false,
                size: data.len() as u64,
                modified: Some(chrono::Utc::now()),
                raw_url: None,
                raw_headers: vec![],
                delivery: ContentDeliveryKind::NetworkDirect,
                local_path: None,
            })
        }

        async fn mkdir(&self, _path: &str) -> Result<()> {
            Ok(())
        }

        async fn rename(&self, _old: &str, _new: &str) -> Result<()> {
            Ok(())
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            Ok(())
        }

        async fn get_content(&self, path: &str) -> Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| DriverError::NotFound(path.to_string()))
        }

        async fn get_content_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Vec<u8>> {
            self.range_calls.fetch_add(1, Ordering::Relaxed);
            let full = self.get_content(path).await?;
            let s = start as usize;
            if s >= full.len() {
                return Ok(Vec::new());
            }
            let e = end.map(|e| (e as usize + 1).min(full.len())).unwrap_or(full.len());
            Ok(full[s..e].to_vec())
        }
    }

    #[tokio::test]
    async fn test_crypt_name_mapping() {
        let config = CryptConfig {
            password: "test".to_string(),
            salt: String::new(),
            filename_encryption: NameMode::Standard,
            filename_encoding: NameEncoding::Base64,
            encrypted_suffix: ".bin".to_string(),
            directory_name_encryption: true,
            source_dir: "/encrypted".to_string(),
        };

        let source = Arc::new(MockSource::new());
        let crypt = CryptDriver::with_source(source, config).unwrap();

        // Test path mapping
        let inner = crypt.map_to_inner("/dir/file.txt", true).unwrap();
        assert!(inner.starts_with("/encrypted/"));
        assert!(!inner.contains("dir")); // Should be encrypted
        assert!(!inner.contains("file.txt")); // Should be encrypted
    }

    /// The UI names directories with a trailing slash (`/dir/`). Empty and `.`
    /// segments must not be encrypted, otherwise the source driver is asked for
    /// a path that does not exist (Baidu replies `errno -9`).
    #[test]
    fn test_trailing_slash_and_dot_segments_are_ignored() {
        let config = CryptConfig {
            password: "test".to_string(),
            salt: String::new(),
            filename_encryption: NameMode::Standard,
            filename_encoding: NameEncoding::Base64,
            encrypted_suffix: ".bin".to_string(),
            directory_name_encryption: true,
            source_dir: "/encrypted".to_string(),
        };
        let source = Arc::new(MockSource::new());
        let crypt = CryptDriver::with_source(source, config).unwrap();

        let plain = crypt.map_to_inner("/dir", false).unwrap();
        assert_eq!(crypt.map_to_inner("/dir/", false).unwrap(), plain);
        assert_eq!(crypt.map_to_inner("//dir//", false).unwrap(), plain);
        assert_eq!(crypt.map_to_inner("/dir/./", false).unwrap(), plain);
        assert_eq!(crypt.map_to_inner("dir", false).unwrap(), plain);

        assert_eq!(crypt.map_to_inner("/", false).unwrap(), "/encrypted");
        assert_eq!(crypt.map_to_inner("", false).unwrap(), "/encrypted");

        let file = crypt.map_to_inner("/dir/file.txt", true).unwrap();
        assert_eq!(crypt.map_to_inner("/dir/file.txt/", true).unwrap(), file);
    }

    /// Playback asks for one range per stream chunk. The ciphertext size and
    /// file nonce must be cached, otherwise each chunk redoes `get()` (Baidu:
    /// list + filemetas + HEAD) plus a 32-byte header read.
    #[tokio::test]
    async fn test_range_reads_reuse_cached_metadata() {
        let config = CryptConfig {
            password: "test".to_string(),
            salt: String::new(),
            filename_encryption: NameMode::Off,
            filename_encoding: NameEncoding::Base64,
            encrypted_suffix: ".bin".to_string(),
            directory_name_encryption: false,
            source_dir: "/encrypted".to_string(),
        };
        let mut mock = MockSource::new();
        // Invalid ciphertext on purpose: the metadata/header bookkeeping is
        // what this test counts, decryption failure is irrelevant.
        mock.add_file("/encrypted/file.bin", vec![0u8; 32 + 16 + 8]);
        let source = Arc::new(mock);
        let as_dyn: Arc<dyn WdmmVfs + Send + Sync> = source.clone();
        let crypt = CryptDriver::with_source(as_dyn, config).unwrap();

        let _ = crypt.get_content_range("/file", 0, Some(3)).await;
        let _ = crypt.get_content_range("/file", 4, Some(7)).await;

        assert_eq!(source.get_calls(), 1, "metadata must be fetched once");
        assert_eq!(
            source.range_calls(),
            3,
            "header once + one block range per call"
        );
    }
}
