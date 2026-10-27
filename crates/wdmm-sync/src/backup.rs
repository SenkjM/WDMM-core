//! WDMMBK01 backup archive (+ optional WDMMEN01 passphrase envelope).
//!
//! Core **create** emits Android-compatible binary sections (codec 1 deflate for most,
//! codec 0 raw for covers/credentials/playlists). Nested WDMMCV01/WDMMPL01 containers.
//! Core **restore** dual-reads: Android binary sections OR legacy Core JSON sections.
//! Container header matches Android `WmpContainer` (docs/10): magic + section table.
//! Envelope matches Android `BackupCrypto` (WDMMEN01, PBKDF2 120_000).

use crate::wmp::*;
use crate::cover::{self, CoverRoots};
use crate::SyncError;
use crate::SyncResult;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use wdmm_drivers::{CloudDriver, CloudFileItem};
use wdmm_storage::{DmrPrefRow, DmrPrefsDatabase, LibraryDatabase, Playlist, PlaylistEntry};

const PBKDF2_ITERS_ENVELOPE: u32 = 120_000;

// Legacy section alias for restore dual-read
const SEC_LIBRARY_JSON: u8 = SEC_TRACKS;

const MAGIC_EN: &[u8; 8] = b"WDMMEN01";

/// Build a raw (unencrypted) WDMMBK01 container from section id → payload bytes (codec 0 raw).
/// Legacy API for tests; new code should use build_wmp_container with explicit codec.
pub fn build_bk_container(sections: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let sections_with_codec: Vec<(u8, Vec<u8>, Codec)> = sections
        .iter()
        .map(|&(id, ref payload)| (id, payload.clone(), Codec::Raw))
        .collect();
    build_wmp_container(&sections_with_codec, MAGIC_BK)
}

/// Parse WDMMBK01 → map section id → raw bytes. Supports codec 0 (raw) and codec 1 (deflate).
pub fn parse_bk_container(bytes: &[u8]) -> SyncResult<BTreeMap<u8, Vec<u8>>> {
    if bytes.len() < 12 || &bytes[0..8] != MAGIC_BK {
        return Err(SyncError::Other("error:notBackupArchive|not WDMMBK01".into()));
    }
    parse_wmp_container(bytes, Some(b"BK"))
}

// Tag constants (backup-specific); encode/decode live in crate::wmp.

const TAG_META_KIND: u8 = 1;
const TAG_META_COUNT: u8 = 2;
const TAG_META_DEVICE_ID: u8 = 5;
const TAG_META_CREATED_AT: u8 = 6;
const TAG_META_NOTE: u8 = 9;

const TAG_VAULT_ID: u8 = 1;
const TAG_VAULT_NAME: u8 = 2;
const TAG_VAULT_PROVIDER: u8 = 3;
const TAG_VAULT_URL: u8 = 4;
const TAG_VAULT_USERNAME: u8 = 5;
const TAG_VAULT_PASSWORD: u8 = 6;
const TAG_VAULT_PASS_ENCRYPTED: u8 = 7;
const TAG_VAULT_DRIVER_CONFIG: u8 = 9;

const TAG_TRACK_SOURCE: u8 = 1;
const TAG_TRACK_REMOTE_PATH: u8 = 2;
const TAG_TRACK_FILENAME: u8 = 3;
const TAG_TRACK_REV: u8 = 4;
const TAG_TRACK_TITLE: u8 = 5;
const TAG_TRACK_ARTIST: u8 = 6;
const TAG_TRACK_ALBUM_ARTIST: u8 = 7;
const TAG_TRACK_ALBUM: u8 = 8;
const TAG_TRACK_DURATION_MS: u8 = 9;
const TAG_TRACK_TRACK_NUM: u8 = 10;
const TAG_TRACK_TRACK_TOTAL: u8 = 11;
const TAG_TRACK_DISC_NUM: u8 = 12;
const TAG_TRACK_DISC_TOTAL: u8 = 13;
const TAG_TRACK_YEAR: u8 = 14;
const TAG_TRACK_GENRE: u8 = 15;
const TAG_TRACK_BITRATE: u8 = 16;
const TAG_TRACK_SAMPLE_RATE: u8 = 17;
const TAG_TRACK_CUE_ID: u8 = 18;
const TAG_TRACK_CUE_REMOTE_PATH: u8 = 19;
const TAG_TRACK_CUE_TRACK_INDEX: u8 = 20;
const TAG_TRACK_AUDIO_MUSIC_ID: u8 = 21;
const TAG_TRACK_AUDIO_REMOTE_PATH: u8 = 22;
const TAG_TRACK_CLIP_START_MS: u8 = 23;
const TAG_TRACK_CLIP_END_MS: u8 = 24;
const TAG_TRACK_CACHE_GROUP_ID: u8 = 25;
const TAG_TRACK_COVER_INDEX: u8 = 26;
const TAG_TRACK_LAST_DOWNLOADED_AT: u8 = 28;
const TAG_TRACK_LAST_TAG_READ_AT: u8 = 29;

// Android kTrackIntTags: 4,9,10,11,12,13,14,16,17,20,23,24,26
const TRACK_INT_TAGS: &[u8] = &[4, 9, 10, 11, 12, 13, 14, 16, 17, 20, 23, 24, 26];
const VAULT_ACCOUNT_INT_TAGS: &[u8] = &[7]; // passwordEncrypted
const META_INT_TAGS: &[u8] = &[1, 2]; // kind, count

/// Check if bytes look like UTF-8 JSON (starts with `{` or `[`).
fn looks_like_json(raw: &[u8]) -> bool {
    if raw.is_empty() {
        return false;
    }
    let s = match std::str::from_utf8(raw) {
        Ok(s) => s.trim_start(),
        Err(_) => return false,
    };
    s.starts_with('{') || s.starts_with('[')
}

/// AESGCMv1 field decrypt helper (matching Android CredentialVaultCrypto).
fn try_decrypt_aesgcmv1(encoded: &str, passphrase: &str) -> Option<String> {
    const PREFIX: &str = "AESGCMv1:";
    if !encoded.starts_with(PREFIX) {
        return Some(encoded.to_string());
    }
    if passphrase.is_empty() {
        return None;
    }
    let b64 = &encoded[PREFIX.len()..];
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()?;
    if raw.len() < 16 + 12 + 16 {
        return None;
    }
    let salt = &raw[0..16];
    let nonce_bytes = &raw[16..28];
    let rest = &raw[28..];
    let mac = &rest[rest.len() - 16..];
    let ct = &rest[..rest.len() - 16];

    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use pbkdf2::pbkdf2_hmac;
    use sha2::Sha256;

    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, 120_000, &mut key);
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let mut ciphertext_with_mac = ct.to_vec();
    ciphertext_with_mac.extend_from_slice(mac);
    let plaintext = cipher.decrypt(nonce, ciphertext_with_mac.as_slice()).ok()?;
    String::from_utf8(plaintext).ok()
}

/// AESGCMv1 field encrypt helper (matching Android CredentialVaultCrypto).
fn encrypt_aesgcmv1(plaintext: &str, passphrase: &str) -> SyncResult<String> {
    const PREFIX: &str = "AESGCMv1:";
    if passphrase.is_empty() || plaintext.is_empty() {
        return Ok(plaintext.to_string());
    }

    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use pbkdf2::pbkdf2_hmac;
    use rand::RngCore;
    use sha2::Sha256;

    let mut salt = [0u8; 16];
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), &salt, 120_000, &mut key);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| SyncError::Other(format!("aesgcm init: {e}")))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| SyncError::Other(format!("aesgcm encrypt: {e}")))?;

    // Wire: salt16|nonce12|ciphertext|mac16
    let mut raw = Vec::with_capacity(16 + 12 + ciphertext.len());
    raw.extend_from_slice(&salt);
    raw.extend_from_slice(&nonce_bytes);
    raw.extend_from_slice(&ciphertext);

    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &raw);
    Ok(format!("{PREFIX}{b64}"))
}

/// Envelope encrypt (unchanged).
pub fn envelope_encrypt(plaintext: &[u8], passphrase: &str) -> SyncResult<Vec<u8>> {
    if passphrase.is_empty() {
        return Ok(plaintext.to_vec());
    }
    // Reuse CredentialVault wire (salt|nonce|ct) but prefix WDMMEN01 and use 120k iters.
    // Implement locally to match Android iteration count (vault uses 600k).
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use pbkdf2::pbkdf2_hmac;
    use rand::RngCore;
    use sha2::Sha256;

    let mut salt = [0u8; 16];
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), &salt, PBKDF2_ITERS_ENVELOPE, &mut key);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| SyncError::Other(format!("encrypt: {e}")))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| SyncError::Other(format!("encrypt: {e}")))?;
    let mut out = Vec::with_capacity(8 + 16 + 12 + ct.len());
    out.extend_from_slice(MAGIC_EN);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn envelope_decrypt(data: &[u8], passphrase: &str) -> SyncResult<Vec<u8>> {
    if data.len() >= 8 && &data[0..8] == MAGIC_BK {
        return Ok(data.to_vec());
    }
    if data.len() < 8 + 16 + 12 + 16 || &data[0..8] != MAGIC_EN {
        return Err(SyncError::Other("not encrypted backup / bad magic".into()));
    }
    if passphrase.is_empty() {
        return Err(SyncError::Other("err.backupEncryptedNeedPassphrase".into()));
    }
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use pbkdf2::pbkdf2_hmac;
    use sha2::Sha256;

    let salt = &data[8..24];
    let nonce_bytes = &data[24..36];
    let ciphertext = &data[36..];
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, PBKDF2_ITERS_ENVELOPE, &mut key);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| SyncError::Other(format!("decrypt: {e}")))?;
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| SyncError::Other("backup decrypt failed (bad passphrase?)".into()))
}

fn json_bytes<T: serde::Serialize>(v: &T) -> SyncResult<Vec<u8>> {
    serde_json::to_vec(v).map_err(|e| SyncError::Serde(e.to_string()))
}

/// Snapshot library + optional DMR prefs into Android-compatible WDMMBK01 binary (+ optional envelope).
///
/// When `cover_roots` is `Some`, cover files are read from disk and emitted in COVERS (SEC=3)
/// and MANGA_COVERS (SEC=15) sections with codec Raw.  When `None`, those sections are omitted
/// (Android restore tolerates missing COVERS/MANGA_COVERS).
pub fn build_backup_bytes(
    lib: &LibraryDatabase,
    dmr: Option<&DmrPrefsDatabase>,
    passphrase: &str,
    cover_roots: Option<&CoverRoots>,
) -> SyncResult<Vec<u8>> {
    let mut sections: Vec<(u8, Vec<u8>, Codec)> = Vec::new();

    // Load accounts once for both tracks and credentials sections
    let accounts = lib.list_accounts()?;
    let account_map: std::collections::HashMap<String, String> = accounts
        .iter()
        .map(|a| (a.id.clone(), a.display_name.clone()))
        .collect();

    // 1. META section
    let tracks = lib.list_tracks()?;
    let cue_slices = lib.list_cue_slices()?;
    let total_track_count = tracks.len() + cue_slices.len();

    let device_id = lib.load_kv("device_id").unwrap_or(None).unwrap_or_else(|| "core".to_string());
    let created_at = chrono::Utc::now().to_rfc3339();

    let mut note_json = json!({
        "format": "webdav_media_manager_backup",
        "formatVersion": 7,
        "passwordEncryption": if passphrase.is_empty() { "none" } else { "aes-256-gcm" }
    });
    if !passphrase.is_empty() {
        note_json.as_object_mut().unwrap().insert("hasPassCheck".to_string(), serde_json::Value::Bool(true));
    }

    let mut meta_rec = BTreeMap::new();
    meta_rec.insert(TAG_META_KIND, RecordValue::Int(3)); // WmpKind::backup = 3
    meta_rec.insert(TAG_META_COUNT, RecordValue::Int(total_track_count as u64));
    meta_rec.insert(TAG_META_DEVICE_ID, RecordValue::Str(device_id));
    meta_rec.insert(TAG_META_CREATED_AT, RecordValue::Str(created_at));
    meta_rec.insert(TAG_META_NOTE, RecordValue::Str(serde_json::to_string(&note_json)?));
    sections.push((SEC_META, encode_records(&[meta_rec], META_INT_TAGS), Codec::Deflate));

    // 2. TRACKS section: regular tracks first, then CUE slices (Android order)
    let mut track_records: Vec<BTreeMap<u8, RecordValue>> = Vec::new();

    // Regular tracks
    for t in &tracks {
        let mut rec = BTreeMap::new();
        let source_name = account_map.get(&t.account_id).map(|s| s.as_str()).unwrap_or(&t.account_id);
        rec.insert(TAG_TRACK_SOURCE, RecordValue::Str(source_name.to_string()));
        rec.insert(TAG_TRACK_REMOTE_PATH, RecordValue::Str(t.remote_path.clone()));
        let file_name = t.remote_path.rsplit('/').next().unwrap_or(&t.remote_path);
        rec.insert(TAG_TRACK_FILENAME, RecordValue::Str(file_name.to_string()));
        // Emit rev tag (matches Android which always writes it; tag 4)
        if t.rev != 0 {
            rec.insert(TAG_TRACK_REV, RecordValue::Int(t.rev as u64));
        }
        rec.insert(TAG_TRACK_TITLE, RecordValue::Str(t.title.clone()));
        if let Some(ref s) = t.artist {
            rec.insert(TAG_TRACK_ARTIST, RecordValue::Str(s.clone()));
        }
        if let Some(ref s) = t.album_artist {
            rec.insert(TAG_TRACK_ALBUM_ARTIST, RecordValue::Str(s.clone()));
        }
        if let Some(ref s) = t.album {
            rec.insert(TAG_TRACK_ALBUM, RecordValue::Str(s.clone()));
        }
        if t.duration_ms > 0 {
            rec.insert(TAG_TRACK_DURATION_MS, RecordValue::Int(t.duration_ms as u64));
        }
        if let Some(v) = t.track_number {
            rec.insert(TAG_TRACK_TRACK_NUM, RecordValue::Int(v as u64));
        }
        if let Some(v) = t.disc_number {
            rec.insert(TAG_TRACK_DISC_NUM, RecordValue::Int(v as u64));
        }
        if let Some(v) = t.year {
            rec.insert(TAG_TRACK_YEAR, RecordValue::Int(v as u64));
        }
        if let Some(ref s) = t.genre {
            rec.insert(TAG_TRACK_GENRE, RecordValue::Str(s.clone()));
        }
        track_records.push(rec);
    }

    // Parent album cue_remote_path by cue_id (one query, not per slice)
    let cue_albums_for_tracks = lib.list_cue_albums()?;
    let cue_remote_by_id: std::collections::HashMap<&str, &str> = cue_albums_for_tracks
        .iter()
        .map(|a| (a.cue_id.as_str(), a.cue_remote_path.as_str()))
        .collect();

    // CUE slices (virtual tracks)
    for s in &cue_slices {
        let mut rec = BTreeMap::new();
        rec.insert(TAG_TRACK_SOURCE, RecordValue::Str(s.source_name.clone()));
        rec.insert(TAG_TRACK_REMOTE_PATH, RecordValue::Str(s.remote_path.clone()));
        rec.insert(TAG_TRACK_FILENAME, RecordValue::Str(s.file_name.clone()));
        rec.insert(TAG_TRACK_REV, RecordValue::Int(s.rev as u64));
        if let Some(ref t) = s.title {
            rec.insert(TAG_TRACK_TITLE, RecordValue::Str(t.clone()));
        }
        if let Some(ref a) = s.artist {
            rec.insert(TAG_TRACK_ARTIST, RecordValue::Str(a.clone()));
        }
        if let Some(ref aa) = s.album_artist {
            rec.insert(TAG_TRACK_ALBUM_ARTIST, RecordValue::Str(aa.clone()));
        }
        if let Some(ref al) = s.album {
            rec.insert(TAG_TRACK_ALBUM, RecordValue::Str(al.clone()));
        }
        if let Some(d) = s.duration_ms {
            rec.insert(TAG_TRACK_DURATION_MS, RecordValue::Int(d as u64));
        }
        if let Some(tn) = s.track_number {
            rec.insert(TAG_TRACK_TRACK_NUM, RecordValue::Int(tn as u64));
        }
        if let Some(tt) = s.track_total {
            rec.insert(TAG_TRACK_TRACK_TOTAL, RecordValue::Int(tt as u64));
        }
        if let Some(dn) = s.disc_number {
            rec.insert(TAG_TRACK_DISC_NUM, RecordValue::Int(dn as u64));
        }
        if let Some(dt) = s.disc_total {
            rec.insert(TAG_TRACK_DISC_TOTAL, RecordValue::Int(dt as u64));
        }
        if let Some(y) = s.year {
            rec.insert(TAG_TRACK_YEAR, RecordValue::Int(y as u64));
        }
        if let Some(ref g) = s.genre {
            rec.insert(TAG_TRACK_GENRE, RecordValue::Str(g.clone()));
        }
        if let Some(b) = s.bitrate {
            rec.insert(TAG_TRACK_BITRATE, RecordValue::Int(b as u64));
        }
        if let Some(sr) = s.sample_rate {
            rec.insert(TAG_TRACK_SAMPLE_RATE, RecordValue::Int(sr as u64));
        }
        // CUE-specific tags
        rec.insert(TAG_TRACK_CUE_ID, RecordValue::Str(s.cue_id.clone()));
        if let Some(path) = cue_remote_by_id.get(s.cue_id.as_str()) {
            rec.insert(TAG_TRACK_CUE_REMOTE_PATH, RecordValue::Str((*path).to_string()));
        }
        rec.insert(TAG_TRACK_CUE_TRACK_INDEX, RecordValue::Int(s.track_index as u64));
        rec.insert(TAG_TRACK_AUDIO_MUSIC_ID, RecordValue::Str(s.audio_music_id.clone()));
        if let Some(ref ar) = s.audio_remote_path {
            rec.insert(TAG_TRACK_AUDIO_REMOTE_PATH, RecordValue::Str(ar.clone()));
        }
        if let Some(cs) = s.clip_start_ms {
            rec.insert(TAG_TRACK_CLIP_START_MS, RecordValue::Int(cs as u64));
        }
        if let Some(ce) = s.clip_end_ms {
            rec.insert(TAG_TRACK_CLIP_END_MS, RecordValue::Int(ce as u64));
        }
        if let Some(ref cg) = s.cache_group_id {
            rec.insert(TAG_TRACK_CACHE_GROUP_ID, RecordValue::Str(cg.clone()));
        }
        rec.insert(TAG_TRACK_LAST_DOWNLOADED_AT, RecordValue::Str(s.last_downloaded_at.clone()));
        rec.insert(TAG_TRACK_LAST_TAG_READ_AT, RecordValue::Str(s.last_tag_read_at.clone()));
        track_records.push(rec);
    }

    // 2b. Music cover indices — set TAG_TRACK_COVER_INDEX (tag 26) on all track_records
    //     (regular tracks, then CUE slices, in the same order they appear in track_records)
    //     that have a cover file on disk, then emit COVERS section (SEC=3, codec Raw).
    if let Some(roots) = cover_roots {
        // One stem per track_record entry in order: regular tracks first, then CUE slices.
        let mut all_stems: Vec<String> = tracks
            .iter()
            .map(|t| {
                let source = account_map
                    .get(&t.account_id)
                    .map(|s| s.as_str())
                    .unwrap_or(&t.account_id);
                cover::music_cover_stem(source, &t.remote_path)
            })
            .collect();
        for s in &cue_slices {
            all_stems.push(cover::music_cover_stem(&s.source_name, &s.remote_path));
        }
        let (cover_bytes, cover_indices) =
            cover::build_cover_section(&roots.music_covers_dir, &all_stems);
        // cover_indices is aligned with all_stems which matches track_records order
        for (i, maybe_idx) in cover_indices.iter().enumerate() {
            if let Some(ci) = maybe_idx {
                track_records[i]
                    .insert(TAG_TRACK_COVER_INDEX, RecordValue::Int(*ci as u64));
            }
        }
        sections.push((SEC_TRACKS, encode_records(&track_records, TRACK_INT_TAGS), Codec::Deflate));
        if !cover_bytes.is_empty() {
            sections.push((SEC_COVERS, cover_bytes, Codec::Raw));
        }
    } else {
        sections.push((SEC_TRACKS, encode_records(&track_records, TRACK_INT_TAGS), Codec::Deflate));
    }

    // 3. CREDENTIALS section (nested WDMMCV01 container, raw codec)
    if !accounts.is_empty() {
        let mut vault_records = Vec::new();
        for a in &accounts {
            let mut rec = BTreeMap::new();
            rec.insert(TAG_VAULT_ID, RecordValue::Str(a.id.clone()));
            rec.insert(TAG_VAULT_NAME, RecordValue::Str(a.display_name.clone()));
            rec.insert(TAG_VAULT_PROVIDER, RecordValue::Str(a.provider_type.clone()));
            if let Some(ref url) = a.base_url {
                rec.insert(TAG_VAULT_URL, RecordValue::Str(url.clone()));
            }

            // Parse credentials_encrypted as JSON to extract username/password/driverConfig
            if !a.credentials_encrypted.is_empty() {
                if let Ok(creds) = serde_json::from_slice::<serde_json::Value>(&a.credentials_encrypted) {
                    if a.provider_type == "webdav" {
                        let username = creds.get("username").and_then(|v| v.as_str()).unwrap_or("");
                        let password = creds.get("password").and_then(|v| v.as_str()).unwrap_or("");
                        rec.insert(TAG_VAULT_USERNAME, RecordValue::Str(username.to_string()));
                        if !password.is_empty() && !passphrase.is_empty() {
                            let encrypted = encrypt_aesgcmv1(password, passphrase)?;
                            rec.insert(TAG_VAULT_PASSWORD, RecordValue::Str(encrypted));
                            rec.insert(TAG_VAULT_PASS_ENCRYPTED, RecordValue::Int(1));
                        } else {
                            rec.insert(TAG_VAULT_PASSWORD, RecordValue::Str(password.to_string()));
                            rec.insert(TAG_VAULT_PASS_ENCRYPTED, RecordValue::Int(0));
                        }
                    } else {
                        // Cloud driver: encrypt secret fields in driverConfig
                        let mut config = creds.clone();
                        if let Some(obj) = config.as_object_mut() {
                            for (k, v) in obj.iter_mut() {
                                if let Some(s) = v.as_str() {
                                    // Heuristic: encrypt non-empty string fields with "secret"/"token"/"password"/"key" in name
                                    if !s.is_empty() && !passphrase.is_empty() &&
                                       (k.contains("secret") || k.contains("token") || k.contains("password") || k.contains("key")) {
                                        if let Ok(enc) = encrypt_aesgcmv1(s, passphrase) {
                                            *v = serde_json::Value::String(enc);
                                        }
                                    }
                                }
                            }
                        }
                        rec.insert(TAG_VAULT_DRIVER_CONFIG, RecordValue::Str(serde_json::to_string(&config)?));
                    }
                }
            }
            vault_records.push(rec);
        }

        // Build nested WDMMCV01: section 5 = CREDENTIALS records
        let vault_creds_bytes = encode_records(&vault_records, VAULT_ACCOUNT_INT_TAGS);
        let cv_sections = vec![(5u8, vault_creds_bytes, Codec::Raw)];
        let cv_container = build_wmp_container(&cv_sections, MAGIC_CV);
        sections.push((SEC_CREDENTIALS, cv_container, Codec::Raw));
    }

    // 4. PLAYLISTS section (each playlist as nested WDMMPL01 blob, raw codec)
    let playlists = lib.list_playlists()?;
    if !playlists.is_empty() {
        let mut pl_records = Vec::new();
        for pl in &playlists {
            let entries = lib.list_playlist_entries(&pl.id).unwrap_or_default();

            // Build WDMMPL01: META section + ENTRIES section
            let mut pl_meta = BTreeMap::new();
            pl_meta.insert(9u8, RecordValue::Str(pl.id.clone())); // TAG_PL_ID = 9
            pl_meta.insert(10u8, RecordValue::Str(pl.name.clone())); // TAG_PL_NAME = 10
            pl_meta.insert(11u8, RecordValue::Str(pl.updated_at.to_rfc3339())); // TAG_PL_UPDATED = 11
            let pl_meta_bytes = encode_records(&[pl_meta], &[]);

            let entry_records: Vec<BTreeMap<u8, RecordValue>> = entries
                .iter()
                .map(|e| {
                    let mut rec = BTreeMap::new();
                    rec.insert(1u8, RecordValue::Str(e.music_id.clone())); // TAG_ENTRY_MUSIC_ID
                    rec.insert(2u8, RecordValue::Str(e.source_name.clone())); // TAG_ENTRY_SOURCE
                    rec.insert(3u8, RecordValue::Str(e.remote_path.clone())); // TAG_ENTRY_PATH
                    rec.insert(4u8, RecordValue::Str(e.title.clone())); // TAG_ENTRY_TITLE
                    rec.insert(5u8, RecordValue::Int(e.duration_ms as u64)); // TAG_ENTRY_DURATION
                    rec
                })
                .collect();
            let pl_entries_bytes = encode_records(&entry_records, &[5]); // durationMs is int

            let pl_sections = vec![
                (1u8, pl_meta_bytes, Codec::Raw),    // META
                (9u8, pl_entries_bytes, Codec::Raw), // ENTRIES
            ];
            let pl_blob = build_wmp_container(&pl_sections, MAGIC_PL);

            let mut pl_rec = BTreeMap::new();
            pl_rec.insert(1u8, RecordValue::Bytes(pl_blob)); // TAG_PL_BLOB
            pl_records.push(pl_rec);
        }
        sections.push((SEC_PLAYLISTS, encode_records(&pl_records, &[]), Codec::Raw));
    }

    // 5. SETTINGS section (JSON, deflated)
    let kv: BTreeMap<String, String> = lib.list_kv()?.into_iter().collect();
    sections.push((SEC_SETTINGS, json_bytes(&kv)?, Codec::Deflate));

    // 6. CUEALBUMS section (JSON, deflated) — always emit even if empty (Android does)
    // Reuse albums already loaded for TRACKS cue_remote_path lookup.
    sections.push((SEC_CUEALBUMS, json_bytes(&cue_albums_for_tracks)?, Codec::Deflate));

    // 7. MANGA_LIBRARY section (JSON, deflated) + optional MANGA_COVERS (SEC=15, Raw)
    let mangas = lib.list_mangas().unwrap_or_default();
    let mut manga_pages = Vec::new();
    for m in &mangas {
        if let Ok(pages) = lib.list_pages(&m.id) {
            manga_pages.push(json!({"manga_id": m.id, "pages": pages}));
        }
    }
    let manga_progress = lib.list_progress().unwrap_or_default();

    if let Some(roots) = cover_roots {
        let stems: Vec<String> = mangas
            .iter()
            .map(|m| cover::manga_cover_stem(&m.id))
            .collect();
        let (manga_cover_bytes, manga_cover_indices) =
            cover::build_cover_section(&roots.manga_covers_dir, &stems);
        // Embed cover_index in each manga JSON row (stripped at restore before DB upsert)
        let mangas_json: Vec<serde_json::Value> = mangas
            .iter()
            .zip(&manga_cover_indices)
            .map(|(m, maybe_idx)| {
                let mut v = serde_json::to_value(m).unwrap_or_default();
                if let Some(ci) = maybe_idx {
                    v["cover_index"] = serde_json::Value::Number((*ci).into());
                }
                v
            })
            .collect();
        sections.push((
            SEC_MANGA_LIBRARY,
            json_bytes(&json!({
                "mangas": mangas_json,
                "pages": manga_pages,
                "progress": manga_progress,
            }))?,
            Codec::Deflate,
        ));
        if !manga_cover_bytes.is_empty() {
            sections.push((SEC_MANGA_COVERS, manga_cover_bytes, Codec::Raw));
        }
    } else {
        sections.push((
            SEC_MANGA_LIBRARY,
            json_bytes(&json!({
                "mangas": mangas,
                "pages": manga_pages,
                "progress": manga_progress,
            }))?,
            Codec::Deflate,
        ));
    }

    // 8. USER_FOLDERS section (JSON, deflated)
    let roots = lib.list_user_folder_children(None).unwrap_or_default();
    let mut folders = Vec::new();
    fn walk_folders(
        lib: &LibraryDatabase,
        folders: &mut Vec<serde_json::Value>,
        parent_id: Option<&str>,
    ) {
        let kids = lib.list_user_folder_children(parent_id).unwrap_or_default();
        for f in kids {
            let items = lib.list_user_folder_items(&f.id).unwrap_or_default();
            folders.push(json!({"folder": f, "items": items}));
            walk_folders(lib, folders, Some(&f.id));
        }
    }
    for f in &roots {
        let items = lib.list_user_folder_items(&f.id).unwrap_or_default();
        folders.push(json!({"folder": f, "items": items}));
        walk_folders(lib, &mut folders, Some(&f.id));
    }
    sections.push((SEC_USER_FOLDERS, json_bytes(&folders)?, Codec::Deflate));

    // 9. DMR_PREFS section (JSON, deflated)
    if let Some(d) = dmr {
        let dmr_rows = d.list()?;
        sections.push((SEC_DMR_PREFS, json_bytes(&dmr_rows)?, Codec::Deflate));
    }

    // 10. PASS_CHECK section (UTF-8 AESGCMv1: token, deflated) — only when passphrase non-empty
    if !passphrase.is_empty() {
        const PLAIN_TOKEN: &str = "WDMM-BACKUP-PASS-OK\u{0001}";
        let encrypted_token = encrypt_aesgcmv1(PLAIN_TOKEN, passphrase)?;
        sections.push((SEC_PASS_CHECK, encrypted_token.as_bytes().to_vec(), Codec::Deflate));
    }

    // COVERS (SEC=3) and MANGA_COVERS (SEC=15) are emitted above when cover_roots is Some.
    // When None, those sections are absent; Android restore tolerates missing cover sections.

    let bk = build_wmp_container(&sections, MAGIC_BK);
    envelope_encrypt(&bk, passphrase)
}

pub fn restore_backup_bytes(
    lib: &LibraryDatabase,
    dmr: Option<&DmrPrefsDatabase>,
    data: &[u8],
    passphrase: &str,
    cover_roots: Option<&CoverRoots>,
) -> SyncResult<String> {
    let plain = envelope_decrypt(data, passphrase)?;
    let sections = parse_bk_container(&plain)?;
    let mut notes = Vec::new();
    let mut used_android_binary = false;
    // Collect (stem, cover_index) pairs during binary TRACKS restore for later file writing
    let mut music_cover_needs: Vec<(String, u32)> = Vec::new();

    // CREDENTIALS: dual-read Android nested WDMMCV01 OR Core JSON
    if let Some(raw) = sections.get(&SEC_CREDENTIALS) {
        if looks_like_json(raw) {
            // Core JSON: array of account objects
            let arr: Vec<serde_json::Value> = serde_json::from_slice(raw)?;
            for v in arr {
                let id = v["id"].as_str().unwrap_or("").to_string();
                if id.is_empty() {
                    continue;
                }
                let creds = if let Some(b64) = v["credentials_b64"].as_str() {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let account = wdmm_core::models::CloudAccount {
                    id: id.clone(),
                    provider_type: v["provider_type"].as_str().unwrap_or("webdav").into(),
                    display_name: v["display_name"].as_str().unwrap_or("").into(),
                    base_url: v["base_url"].as_str().map(|s| s.to_string()),
                    credentials_encrypted: creds,
                };
                let _ = lib.insert_account(&account);
            }
        } else if raw.starts_with(b"WDMMCV01") {
            // Android nested vault: parse WDMMCV01 container
            used_android_binary = true;
            let cv_sections = parse_wmp_container(raw, Some(b"CV"))?;
            if let Some(creds_raw) = cv_sections.get(&5) {
                // section 5 = CREDENTIALS inside vault
                let vault_records = decode_records(creds_raw, VAULT_ACCOUNT_INT_TAGS)?;
                for rec in vault_records {
                    let id = rec.get(&TAG_VAULT_ID).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if id.is_empty() {
                        continue;
                    }
                    let name = rec.get(&TAG_VAULT_NAME).and_then(|v| v.as_str()).unwrap_or("");
                    let provider = rec.get(&TAG_VAULT_PROVIDER).and_then(|v| v.as_str()).unwrap_or("webdav");
                    let url = rec.get(&TAG_VAULT_URL).and_then(|v| v.as_str()).unwrap_or("");
                    let username = rec.get(&TAG_VAULT_USERNAME).and_then(|v| v.as_str()).unwrap_or("");
                    let password_raw = rec.get(&TAG_VAULT_PASSWORD).and_then(|v| v.as_str()).unwrap_or("");
                    let pass_encrypted = rec.get(&TAG_VAULT_PASS_ENCRYPTED).and_then(|v| v.as_int()).unwrap_or(0) != 0;

                    // Decrypt password if encrypted
                    let password = if pass_encrypted && !password_raw.is_empty() {
                        try_decrypt_aesgcmv1(password_raw, passphrase).unwrap_or_default()
                    } else {
                        password_raw.to_string()
                    };

                    // Build credentials JSON for Core
                    let creds_json = if provider == "webdav" {
                        json!({
                            "username": username,
                            "password": password,
                            "remote_dir": "/"
                        })
                    } else {
                        // Cloud driver: parse driverConfig JSON and decrypt fields
                        let config_str = rec.get(&TAG_VAULT_DRIVER_CONFIG).and_then(|v| v.as_str()).unwrap_or("{}");
                        let mut config: serde_json::Map<String, serde_json::Value> =
                            serde_json::from_str(config_str).unwrap_or_default();
                        // Decrypt AESGCMv1: fields in config
                        for (_, v) in config.iter_mut() {
                            if let Some(s) = v.as_str() {
                                if let Some(decrypted) = try_decrypt_aesgcmv1(s, passphrase) {
                                    *v = serde_json::Value::String(decrypted);
                                }
                            }
                        }
                        serde_json::Value::Object(config)
                    };

                    let account = wdmm_core::models::CloudAccount {
                        id: id.clone(),
                        provider_type: provider.to_string(),
                        display_name: name.to_string(),
                        base_url: if url.is_empty() { None } else { Some(url.to_string()) },
                        credentials_encrypted: serde_json::to_vec(&creds_json).unwrap_or_default(),
                    };
                    let _ = lib.insert_account(&account);
                }
            }
        }
        notes.push("credentials");
    }

    // TRACKS: dual-read Android deflated binary records OR Core JSON
    if let Some(raw) = sections.get(&SEC_LIBRARY_JSON) {
        if looks_like_json(raw) {
            // Core JSON
            if let Ok(tracks) = serde_json::from_slice::<Vec<wdmm_core::models::LibraryTrack>>(raw) {
                for t in tracks {
                    let _ = lib.upsert_track(&t);
                }
            }
        } else {
            // Android binary track records
            used_android_binary = true;
            if let Ok(track_records) = decode_records(raw, TRACK_INT_TAGS) {
                // Map account display names for track source_name lookup
                let accounts = lib.list_accounts().unwrap_or_default();
                let account_map: std::collections::HashMap<String, String> = accounts
                    .iter()
                    .map(|a| (a.display_name.clone(), a.id.clone()))
                    .collect();

                for rec in track_records {
                    let source = rec.get(&TAG_TRACK_SOURCE).and_then(|v| v.as_str()).unwrap_or("");
                    let remote_path = rec.get(&TAG_TRACK_REMOTE_PATH).and_then(|v| v.as_str()).unwrap_or("");
                    if remote_path.is_empty() {
                        continue;
                    }

                    // Check if this is a CUE slice (has cueId tag)
                    if let Some(cue_id) = rec.get(&TAG_TRACK_CUE_ID).and_then(|v| v.as_str()) {
                        // This is a CUE slice — restore to cue_slices table
                        use wdmm_storage::CueSlice;
                        let title = rec.get(&TAG_TRACK_TITLE).and_then(|v| v.as_str())
                            .or_else(|| rec.get(&TAG_TRACK_FILENAME).and_then(|v| v.as_str()))
                            .unwrap_or("")
                            .to_string();
                        let file_name = rec.get(&TAG_TRACK_FILENAME).and_then(|v| v.as_str()).unwrap_or("");
                        let music_id = wdmm_core::identity::track_identity(remote_path);

                        let slice = CueSlice {
                            music_id,
                            cue_id: cue_id.to_string(),
                            audio_music_id: rec.get(&TAG_TRACK_AUDIO_MUSIC_ID).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            source_name: source.to_string(),
                            remote_path: remote_path.to_string(),
                            file_name: file_name.to_string(),
                            track_index: rec.get(&TAG_TRACK_CUE_TRACK_INDEX).and_then(|v| v.as_int()).unwrap_or(0),
                            title: if title.is_empty() { None } else { Some(title) },
                            artist: rec.get(&TAG_TRACK_ARTIST).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            album_artist: rec.get(&TAG_TRACK_ALBUM_ARTIST).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            album: rec.get(&TAG_TRACK_ALBUM).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            duration_ms: rec.get(&TAG_TRACK_DURATION_MS).and_then(|v| v.as_int()),
                            track_number: rec.get(&TAG_TRACK_TRACK_NUM).and_then(|v| v.as_int()),
                            track_total: rec.get(&TAG_TRACK_TRACK_TOTAL).and_then(|v| v.as_int()),
                            disc_number: rec.get(&TAG_TRACK_DISC_NUM).and_then(|v| v.as_int()),
                            disc_total: rec.get(&TAG_TRACK_DISC_TOTAL).and_then(|v| v.as_int()),
                            year: rec.get(&TAG_TRACK_YEAR).and_then(|v| v.as_int()),
                            genre: rec.get(&TAG_TRACK_GENRE).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            bitrate: rec.get(&TAG_TRACK_BITRATE).and_then(|v| v.as_int()),
                            sample_rate: rec.get(&TAG_TRACK_SAMPLE_RATE).and_then(|v| v.as_int()),
                            cover_state: 0,
                            audio_remote_path: rec.get(&TAG_TRACK_AUDIO_REMOTE_PATH).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            clip_start_ms: rec.get(&TAG_TRACK_CLIP_START_MS).and_then(|v| v.as_int()),
                            clip_end_ms: rec.get(&TAG_TRACK_CLIP_END_MS).and_then(|v| v.as_int()),
                            cache_group_id: rec.get(&TAG_TRACK_CACHE_GROUP_ID).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            rev: rec.get(&TAG_TRACK_REV).and_then(|v| v.as_int()).unwrap_or(0),
                            last_downloaded_at: rec.get(&TAG_TRACK_LAST_DOWNLOADED_AT).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            last_tag_read_at: rec.get(&TAG_TRACK_LAST_TAG_READ_AT).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            remote_missing: 0,
                        };
                        let _ = lib.upsert_cue_slice(&slice);
                        // Collect cover need for file writing after all tracks are processed
                        if cover_roots.is_some() {
                            if let Some(ci) = rec.get(&TAG_TRACK_COVER_INDEX).and_then(|v| v.as_int()) {
                                let stem = cover::music_cover_stem(source, remote_path);
                                music_cover_needs.push((stem, ci as u32));
                            }
                        }
                    } else {
                        // Regular track
                        let title = rec.get(&TAG_TRACK_TITLE).and_then(|v| v.as_str())
                            .or_else(|| rec.get(&TAG_TRACK_FILENAME).and_then(|v| v.as_str()))
                            .or_else(|| remote_path.rsplit('/').next())
                            .unwrap_or("")
                            .to_string();

                        let account_id = account_map.get(source).cloned().unwrap_or_else(|| source.to_string());
                        let id = wdmm_core::identity::track_identity(remote_path);

                        let track = wdmm_core::models::LibraryTrack {
                            id,
                            account_id,
                            remote_path: remote_path.to_string(),
                            title,
                            artist: rec.get(&TAG_TRACK_ARTIST).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            album_id: None,
                            artist_id: None,
                            album: rec.get(&TAG_TRACK_ALBUM).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            album_artist: rec.get(&TAG_TRACK_ALBUM_ARTIST).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            genre: rec.get(&TAG_TRACK_GENRE).and_then(|v| v.as_str()).map(|s| s.to_string()),
                            duration_ms: rec.get(&TAG_TRACK_DURATION_MS).and_then(|v| v.as_int()).unwrap_or(0),
                            track_number: rec.get(&TAG_TRACK_TRACK_NUM).and_then(|v| v.as_int()).map(|n| n as i32),
                            disc_number: rec.get(&TAG_TRACK_DISC_NUM).and_then(|v| v.as_int()).map(|n| n as i32),
                            year: rec.get(&TAG_TRACK_YEAR).and_then(|v| v.as_int()).map(|n| n as i32),
                            rev: rec.get(&TAG_TRACK_REV).and_then(|v| v.as_int()).unwrap_or(0),
                            size: 0,
                            modified: None,
                            mime_type: None,
                            cover_state: 0,
                        };
                        let _ = lib.upsert_track(&track);
                        // Collect cover need for file writing after all tracks are processed
                        if cover_roots.is_some() {
                            if let Some(ci) = rec.get(&TAG_TRACK_COVER_INDEX).and_then(|v| v.as_int()) {
                                let stem = cover::music_cover_stem(source, remote_path);
                                music_cover_needs.push((stem, ci as u32));
                            }
                        }
                    }
                }
            }
        }
        notes.push("tracks");
    }

    // Write music cover files if COVERS section present and roots configured.
    // coverIndex stored in track records maps to the 0-based index in the COVERS section.
    if let (Some(roots), Some(raw_covers)) = (cover_roots, sections.get(&SEC_COVERS)) {
        let cover_entries = cover::parse_cover_section(raw_covers);
        if !cover_entries.is_empty() && std::fs::create_dir_all(&roots.music_covers_dir).is_ok() {
            let mut wrote_covers = 0usize;
            for (stem, ci) in &music_cover_needs {
                if let Some(&(kind, ref blob)) = cover_entries.get(*ci as usize) {
                    if kind != cover::COVER_KIND_NONE && !blob.is_empty() {
                        let path = roots.music_covers_dir.join(stem);
                        if std::fs::write(&path, blob).is_ok() {
                            wrote_covers += 1;
                        }
                    }
                }
            }
            if wrote_covers > 0 {
                notes.push("covers");
            }
        }
    }

    // PLAYLISTS: dual-read Android WDMMPL01 blobs OR Core JSON
    if let Some(raw) = sections.get(&SEC_PLAYLISTS) {
        if looks_like_json(raw) {
            // Core JSON
            if let Ok(items) = serde_json::from_slice::<Vec<serde_json::Value>>(raw) {
                for it in items {
                    if let Ok(pl) = serde_json::from_value::<Playlist>(it["playlist"].clone()) {
                        let _ = lib.upsert_playlist(&pl);
                        if let Ok(entries) =
                            serde_json::from_value::<Vec<PlaylistEntry>>(it["entries"].clone())
                        {
                            let _ = lib.set_playlist_entries(&pl.id, &entries);
                        }
                    }
                }
            }
        } else {
            // Android: records with tag 1 = WDMMPL01 blob
            used_android_binary = true;
            const TAG_PL_BLOB: u8 = 1;
            if let Ok(pl_records) = decode_records(raw, &[]) {
                for rec in pl_records {
                    if let Some(blob) = rec.get(&TAG_PL_BLOB).and_then(|v| v.as_bytes()) {
                        if let Ok(pl_data) = decode_playlist_wdmmpl01(blob) {
                            let _ = lib.upsert_playlist(&pl_data.0);
                            let _ = lib.set_playlist_entries(&pl_data.0.id, &pl_data.1);
                        }
                    }
                }
            }
        }
        notes.push("playlists");
    }

    if let Some(raw) = sections.get(&SEC_SETTINGS) {
        if let Ok(map) = serde_json::from_slice::<BTreeMap<String, String>>(raw) {
            for (k, v) in map {
                let _ = lib.save_kv(&k, &v);
            }
            notes.push("settings");
        }
    }

    // CUEALBUMS: restore CUE album records
    if let Some(raw) = sections.get(&SEC_CUEALBUMS) {
        if let Ok(albums) = serde_json::from_slice::<Vec<wdmm_storage::CueAlbum>>(raw) {
            for album in albums {
                let _ = lib.upsert_cue_album(&album);
            }
            notes.push("cue_albums");
        }
    }

    // PASS_CHECK: verify passphrase check token if present (informational only during restore)
    if let Some(raw) = sections.get(&SEC_PASS_CHECK) {
        if let Ok(token) = std::str::from_utf8(raw) {
            if !passphrase.is_empty() {
                const PLAIN_TOKEN: &str = "WDMM-BACKUP-PASS-OK\u{0001}";
                if let Some(decrypted) = try_decrypt_aesgcmv1(token, passphrase) {
                    if decrypted == PLAIN_TOKEN {
                        notes.push("pass_check_ok");
                    }
                }
            }
        }
    }

    if let Some(raw) = sections.get(&SEC_MANGA_LIBRARY) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) {
            if let Some(arr) = v.get("mangas").and_then(|x| x.as_array()) {
                // Parse manga covers section once, if available
                let manga_cover_entries = sections
                    .get(&SEC_MANGA_COVERS)
                    .map(|r| cover::parse_cover_section(r))
                    .unwrap_or_default();
                let mut wrote_manga_covers = 0usize;
                for m in arr {
                    // cover_index is present only when the backup was made with cover_roots; serde
                    // ignores it when deserialising into Manga (which has no such field), so the
                    // DB upsert never sees it — no explicit stripping needed.
                    let cover_index = m.get("cover_index").and_then(|v| v.as_u64()).map(|v| v as u32);
                    if let Ok(manga) = serde_json::from_value::<wdmm_storage::Manga>(m.clone()) {
                        let _ = lib.upsert_manga(&manga);
                        // Write manga cover file
                        if let (Some(roots), Some(ci)) = (cover_roots, cover_index) {
                            let stem = cover::manga_cover_stem(&manga.id);
                            if let Some(&(kind, ref blob)) = manga_cover_entries.get(ci as usize) {
                                if kind != cover::COVER_KIND_NONE && !blob.is_empty() {
                                    if std::fs::create_dir_all(&roots.manga_covers_dir).is_ok() {
                                        let path = roots.manga_covers_dir.join(&stem);
                                        if std::fs::write(&path, blob).is_ok() {
                                            wrote_manga_covers += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if wrote_manga_covers > 0 {
                    notes.push("manga_covers");
                }
            }
            if let Some(arr) = v.get("pages").and_then(|x| x.as_array()) {
                for block in arr {
                    let mid = block["manga_id"].as_str().unwrap_or("");
                    if let Ok(pages) =
                        serde_json::from_value::<Vec<wdmm_storage::MangaPage>>(block["pages"].clone())
                    {
                        let _ = lib.replace_pages(mid, &pages);
                    }
                }
            }
            if let Some(arr) = v.get("progress").and_then(|x| x.as_array()) {
                for p in arr {
                    if let (Some(mid), Some(idx)) = (
                        p.get("manga_id").and_then(|x| x.as_str()),
                        p.get("page_index").and_then(|x| x.as_i64()),
                    ) {
                        let _ = lib.set_progress(mid, idx);
                    }
                }
            }
            notes.push("manga");
        }
    }

    if let (Some(raw), Some(dmr_db)) = (sections.get(&SEC_DMR_PREFS), dmr) {
        if let Ok(rows) = serde_json::from_slice::<Vec<DmrPrefRow>>(raw) {
            let _ = dmr_db.replace_all(&rows);
            notes.push("dmr_prefs");
        }
    }

    let suffix = if used_android_binary {
        ",android-binary"
    } else {
        ""
    };
    Ok(format!("ok:restored:{}{}", notes.join(","), suffix))
}

/// Decode a WDMMPL01 playlist blob → (Playlist, entries).
fn decode_playlist_wdmmpl01(bytes: &[u8]) -> SyncResult<(Playlist, Vec<PlaylistEntry>)> {
    let sections = parse_wmp_container(bytes, Some(b"PL"))?;
    let meta_raw = sections.get(&1).ok_or_else(|| SyncError::Other("missing PL META".into()))?;

    // META: intTags = {1,2,3,4} (kind, seqFrom, seqTo, count), strings = playlistId/name/updatedAt
    const META_INT_TAGS: &[u8] = &[1, 2, 3, 4];
    let meta_records = decode_records(meta_raw, META_INT_TAGS)?;
    let meta = meta_records.first().ok_or_else(|| SyncError::Other("empty PL META".into()))?;

    const TAG_PL_ID: u8 = 9;
    const TAG_PL_NAME: u8 = 10;
    const TAG_PL_UPDATED: u8 = 11;

    let id = meta.get(&TAG_PL_ID).and_then(|v| v.as_str()).unwrap_or("").to_string();
    if id.is_empty() {
        return Err(SyncError::Other("playlist missing id".into()));
    }
    let name = meta.get(&TAG_PL_NAME).and_then(|v| v.as_str()).unwrap_or("Unnamed").to_string();
    let updated_str = meta.get(&TAG_PL_UPDATED).and_then(|v| v.as_str()).unwrap_or("");

    let entries_raw = sections.get(&9).ok_or_else(|| SyncError::Other("missing PL ENTRIES".into()))?;
    const ENTRY_INT_TAGS: &[u8] = &[5, 6, 7, 8]; // durationMs, cueTrackIndex, coverIndex, remoteMissing
    let entry_records = decode_records(entries_raw, ENTRY_INT_TAGS)?;

    const TAG_ENTRY_MUSIC_ID: u8 = 1;
    const TAG_ENTRY_SOURCE: u8 = 2;
    const TAG_ENTRY_PATH: u8 = 3;
    const TAG_ENTRY_TITLE: u8 = 4;
    const TAG_ENTRY_DURATION: u8 = 5;
    const TAG_ENTRY_CUE_IDX: u8 = 6;

    let mut entries = Vec::new();
    for (pos, rec) in entry_records.iter().enumerate() {
        let music_id = rec.get(&TAG_ENTRY_MUSIC_ID).and_then(|v| v.as_str()).unwrap_or("");
        let source = rec.get(&TAG_ENTRY_SOURCE).and_then(|v| v.as_str()).unwrap_or("");
        let path = rec.get(&TAG_ENTRY_PATH).and_then(|v| v.as_str()).unwrap_or("");
        let title = rec.get(&TAG_ENTRY_TITLE).and_then(|v| v.as_str()).unwrap_or("");
        let duration_ms = rec.get(&TAG_ENTRY_DURATION).and_then(|v| v.as_int());
        let _cue_idx = rec.get(&TAG_ENTRY_CUE_IDX).and_then(|v| v.as_int());

        entries.push(PlaylistEntry {
            playlist_id: id.clone(),
            position: pos as i64,
            music_id: music_id.to_string(),
            source_name: source.to_string(),
            remote_path: path.to_string(),
            title: title.to_string(),
            duration_ms: duration_ms.unwrap_or(0),
        });
    }

    let updated_dt = chrono::DateTime::parse_from_rfc3339(updated_str)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| chrono::Utc::now());

    let playlist = Playlist {
        id: id.clone(),
        name,
        created_at: chrono::Utc::now(),
        updated_at: updated_dt,
    };

    Ok((playlist, entries))
}

pub fn write_backup_file(
    lib: &LibraryDatabase,
    dmr: Option<&DmrPrefsDatabase>,
    passphrase: &str,
    out_path: &Path,
    cover_roots: Option<&CoverRoots>,
) -> SyncResult<()> {
    let bytes = build_backup_bytes(lib, dmr, passphrase, cover_roots)?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SyncError::Other(e.to_string()))?;
    }
    std::fs::write(out_path, bytes).map_err(|e| SyncError::Other(e.to_string()))?;
    Ok(())
}

pub fn restore_backup_file(
    lib: &LibraryDatabase,
    dmr: Option<&DmrPrefsDatabase>,
    passphrase: &str,
    path: &Path,
    cover_roots: Option<&CoverRoots>,
) -> SyncResult<String> {
    let data = std::fs::read(path).map_err(|e| SyncError::Other(e.to_string()))?;
    restore_backup_bytes(lib, dmr, &data, passphrase, cover_roots)
}

fn join_remote(dir: &str, name: &str) -> String {
    let base = dir.trim_end_matches('/');
    let name = name.trim_start_matches('/');
    if base.is_empty() {
        format!("/{name}")
    } else {
        format!("{base}/{name}")
    }
}

pub async fn backup_upload(
    lib: Arc<LibraryDatabase>,
    dmr: Option<Arc<DmrPrefsDatabase>>,
    driver: Arc<dyn CloudDriver>,
    remote_dir: &str,
    passphrase: &str,
    cover_roots: Option<&CoverRoots>,
) -> SyncResult<String> {
    let bytes = build_backup_bytes(
        lib.as_ref(),
        dmr.as_ref().map(|a| a.as_ref()),
        passphrase,
        cover_roots,
    )?;
    let _ = driver.mkdir(remote_dir).await;
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let name = format!("webdav_media_backup-{stamp}.wdmm");
    let path = join_remote(remote_dir, &name);
    driver
        .put_content(&path, &bytes)
        .await
        .map_err(SyncError::from)?;
    Ok(path)
}

pub async fn backup_list(
    driver: Arc<dyn CloudDriver>,
    remote_dir: &str,
) -> SyncResult<Vec<String>> {
    let items: Vec<CloudFileItem> = driver.list(remote_dir).await.map_err(SyncError::from)?;
    let mut names: Vec<String> = items
        .into_iter()
        .filter(|i| !i.is_dir)
        .map(|i| i.name)
        .filter(|n| n.ends_with(".wdmm") || n.contains("backup"))
        .collect();
    names.sort();
    names.reverse();
    Ok(names)
}

pub async fn backup_download_restore(
    lib: Arc<LibraryDatabase>,
    dmr: Option<Arc<DmrPrefsDatabase>>,
    driver: Arc<dyn CloudDriver>,
    remote_file: &str,
    passphrase: &str,
    cover_roots: Option<&CoverRoots>,
) -> SyncResult<String> {
    let bytes = driver
        .get_content(remote_file)
        .await
        .map_err(SyncError::from)?;
    restore_backup_bytes(
        lib.as_ref(),
        dmr.as_ref().map(|a| a.as_ref()),
        &bytes,
        passphrase,
        cover_roots,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wdmm_storage::DmrPrefsDatabase;

    #[test]
    fn bk_container_roundtrip() {
        let sections = vec![(5u8, b"{\"a\":1}".to_vec()), (16u8, b"[]".to_vec())];
        let bytes = build_bk_container(&sections);
        assert_eq!(&bytes[0..8], MAGIC_BK);
        let map = parse_bk_container(&bytes).unwrap();
        assert_eq!(map.get(&5).unwrap(), b"{\"a\":1}");
        assert_eq!(map.get(&16).unwrap(), b"[]");
    }

    #[test]
    fn envelope_roundtrip() {
        let plain = build_bk_container(&[(7u8, b"{}".to_vec())]);
        let enc = envelope_encrypt(&plain, "secret").unwrap();
        assert_eq!(&enc[0..8], MAGIC_EN);
        let dec = envelope_decrypt(&enc, "secret").unwrap();
        assert_eq!(dec, plain);
    }

    #[test]
    fn backup_lib_roundtrip_memory() {
        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        let dmr = DmrPrefsDatabase::open(Path::new(":memory:")).unwrap();
        dmr.upsert(&DmrPrefRow {
            usn: "u1".into(),
            force_direct: true,
            override_name: Some("TV".into()),
            updated_at: String::new(),
        })
        .unwrap();
        lib.save_kv("sync_interval", "off").unwrap();
        let bytes = build_backup_bytes(&lib, Some(&dmr), "pw", None).unwrap();
        let lib2 = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        let dmr2 = DmrPrefsDatabase::open(Path::new(":memory:")).unwrap();
        let msg = restore_backup_bytes(&lib2, Some(&dmr2), &bytes, "pw", None).unwrap();
        assert!(msg.contains("settings") || msg.contains("dmr"));
        assert_eq!(lib2.load_kv("sync_interval").unwrap().as_deref(), Some("off"));
        assert_eq!(dmr2.get("u1").unwrap().unwrap().override_name.as_deref(), Some("TV"));
    }

    #[test]
    fn android_binary_track_restore() {
        // Build a minimal WDMMBK01 container with deflated track section
        use flate2::write::DeflateEncoder;
        use std::io::Write;

        // Create account first
        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        lib.insert_account(&wdmm_core::models::CloudAccount {
            id: "test_acc".into(),
            provider_type: "webdav".into(),
            display_name: "TestSource".into(),
            base_url: Some("https://test.example".into()),
            credentials_encrypted: vec![],
        }).unwrap();

        // Build binary track records: one track with sourceName=1 remotePath=2 title=5
        let mut track_bytes = Vec::new();
        // Record 1: sourceName="TestSource", remotePath="/music/song.mp3", title="MySong"
        track_bytes.extend_from_slice(&[1]); // tag 1 = sourceName (string)
        let src = b"TestSource";
        track_bytes.extend_from_slice(&encode_varint(src.len() as u64));
        track_bytes.extend_from_slice(src);

        track_bytes.extend_from_slice(&[2]); // tag 2 = remotePath (string)
        let path = b"/music/song.mp3";
        track_bytes.extend_from_slice(&encode_varint(path.len() as u64));
        track_bytes.extend_from_slice(path);

        track_bytes.extend_from_slice(&[5]); // tag 5 = title (string)
        let title = b"MySong";
        track_bytes.extend_from_slice(&encode_varint(title.len() as u64));
        track_bytes.extend_from_slice(title);

        track_bytes.extend_from_slice(&[9]); // tag 9 = durationMs (int)
        track_bytes.extend_from_slice(&encode_varint(180_000));

        track_bytes.extend_from_slice(&[0]); // end marker

        // Deflate track_bytes
        let mut encoder = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&track_bytes).unwrap();
        let deflated = encoder.finish().unwrap();

        // Build WDMMBK01 container: section 2 (TRACKS) = deflated
        let crc = crc32_ieee(&track_bytes);
        let mut container = Vec::new();
        container.extend_from_slice(b"WDMMBK01"); // magic (no space!)
        container.extend_from_slice(&1u16.to_le_bytes()); // 1 section
        container.extend_from_slice(&[0, 0]); // reserved
        // Section table: id=2 codec=1 offset stored_len raw_len crc
        let offset = 12 + 18; // after header + table
        container.extend_from_slice(&[2]); // SEC_LIBRARY_JSON
        container.extend_from_slice(&[1]); // codec deflate
        container.extend_from_slice(&(offset as u32).to_le_bytes());
        container.extend_from_slice(&(deflated.len() as u32).to_le_bytes());
        container.extend_from_slice(&(track_bytes.len() as u32).to_le_bytes());
        container.extend_from_slice(&crc.to_le_bytes());
        container.extend_from_slice(&deflated);

        // Wrap in envelope
        let encrypted = envelope_encrypt(&container, "testpw").unwrap();

        // Restore into fresh DB
        let lib2 = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        lib2.insert_account(&wdmm_core::models::CloudAccount {
            id: "test_acc".into(),
            provider_type: "webdav".into(),
            display_name: "TestSource".into(),
            base_url: Some("https://test.example".into()),
            credentials_encrypted: vec![],
        }).unwrap();

        let msg = restore_backup_bytes(&lib2, None, &encrypted, "testpw", None).unwrap();
        assert!(msg.contains("android-binary"), "Expected android-binary marker, got: {}", msg);
        assert!(msg.contains("tracks"), "Expected tracks marker, got: {}", msg);

        // Verify track was restored
        let tracks = lib2.list_tracks().unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "MySong");
        assert_eq!(tracks[0].remote_path, "/music/song.mp3");
        assert_eq!(tracks[0].duration_ms, 180_000);
    }

    /// Build a WDMMCV01 container with credentials section
    fn build_cv_container(vault_records_raw: Vec<u8>) -> Vec<u8> {
        let sections = vec![(5u8, vault_records_raw)]; // section 5 = CREDENTIALS
        let count = sections.len() as u16;
        let header_len = 12usize;
        let table_len = sections.len() * 18;
        let payload_start = header_len + table_len;
        let mut payload = Vec::new();
        let mut table = Vec::with_capacity(table_len);
        for &(id, ref raw) in &sections {
            let offset = (payload_start + payload.len()) as u32;
            let raw_len = raw.len() as u32;
            let crc = crc32_ieee(raw);
            table.push(id);
            table.push(0u8); // codec = raw
            table.extend_from_slice(&offset.to_le_bytes());
            table.extend_from_slice(&raw_len.to_le_bytes());
            table.extend_from_slice(&raw_len.to_le_bytes());
            table.extend_from_slice(&crc.to_le_bytes());
            payload.extend_from_slice(raw);
        }
        let mut out = Vec::with_capacity(payload_start + payload.len());
        out.extend_from_slice(b"WDMMCV01");
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&table);
        out.extend_from_slice(&payload);
        out
    }

    /// Encode a single record in Android binary format
    fn encode_record(tags: &[(u8, &[u8])], int_tags: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(tag, value) in tags {
            out.push(tag);
            if int_tags.contains(&tag) {
                // int value: value is big-endian u64 bytes
                let val = u64::from_be_bytes(value.try_into().unwrap_or([0u8; 8]));
                out.extend_from_slice(&encode_varint(val));
            } else {
                // string/bytes: length-prefixed
                out.extend_from_slice(&encode_varint(value.len() as u64));
                out.extend_from_slice(value);
            }
        }
        out.push(0); // end marker
        out
    }

    #[test]
    fn android_binary_cv_restore() {
        // Build nested WDMMCV01 with one webdav vault record (plaintext password)
        let vault_id = b"vault_001";
        let vault_name = b"MyWebDAV";
        let provider = b"webdav";
        let url = b"https://dav.example.com";
        let username = b"user1";
        let password = b"pass123";
        let pass_encrypted = 0u64; // plaintext

        let vault_record = encode_record(
            &[
                (TAG_VAULT_ID, vault_id),
                (TAG_VAULT_NAME, vault_name),
                (TAG_VAULT_PROVIDER, provider),
                (TAG_VAULT_URL, url),
                (TAG_VAULT_USERNAME, username),
                (TAG_VAULT_PASSWORD, password),
                (TAG_VAULT_PASS_ENCRYPTED, &pass_encrypted.to_be_bytes()),
            ],
            &[TAG_VAULT_PASS_ENCRYPTED],
        );

        let cv_container = build_cv_container(vault_record);
        let bk_sections = vec![(SEC_CREDENTIALS, cv_container)];
        let bk = build_bk_container(&bk_sections);

        // Restore
        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        let msg = restore_backup_bytes(&lib, None, &bk, "", None).unwrap();
        assert!(msg.contains("credentials"));
        assert!(msg.contains("android-binary"));

        // Verify account
        let accounts = lib.list_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        let acc = &accounts[0];
        assert_eq!(acc.id, "vault_001");
        assert_eq!(acc.display_name, "MyWebDAV");
        assert_eq!(acc.provider_type, "webdav");
        assert_eq!(acc.base_url.as_deref(), Some("https://dav.example.com"));

        // Verify credentials JSON
        let creds: serde_json::Value = serde_json::from_slice(&acc.credentials_encrypted).unwrap();
        assert_eq!(creds["username"].as_str(), Some("user1"));
        assert_eq!(creds["password"].as_str(), Some("pass123"));
        assert_eq!(creds["remote_dir"].as_str(), Some("/"));
    }

    /// Build a WDMMPL01 playlist container
    fn build_pl_container(meta_raw: Vec<u8>, entries_raw: Vec<u8>) -> Vec<u8> {
        let sections = vec![(1u8, meta_raw), (9u8, entries_raw)];
        let count = sections.len() as u16;
        let header_len = 12usize;
        let table_len = sections.len() * 18;
        let payload_start = header_len + table_len;
        let mut payload = Vec::new();
        let mut table = Vec::with_capacity(table_len);
        for &(id, ref raw) in &sections {
            let offset = (payload_start + payload.len()) as u32;
            let raw_len = raw.len() as u32;
            let crc = crc32_ieee(raw);
            table.push(id);
            table.push(0u8); // codec = raw
            table.extend_from_slice(&offset.to_le_bytes());
            table.extend_from_slice(&raw_len.to_le_bytes());
            table.extend_from_slice(&raw_len.to_le_bytes());
            table.extend_from_slice(&crc.to_le_bytes());
            payload.extend_from_slice(raw);
        }
        let mut out = Vec::with_capacity(payload_start + payload.len());
        out.extend_from_slice(b"WDMMPL01");
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&table);
        out.extend_from_slice(&payload);
        out
    }

    #[test]
    fn android_binary_pl_restore() {
        // Build WDMMPL01 with META + one entry
        const TAG_PL_ID: u8 = 9;
        const TAG_PL_NAME: u8 = 10;
        const TAG_PL_UPDATED: u8 = 11;
        const META_INT_TAGS: &[u8] = &[1, 2, 3, 4];

        let pl_id = b"playlist_001";
        let pl_name = b"MyPlaylist";
        let updated = b"2026-10-26T12:00:00Z";

        let meta_record = encode_record(
            &[
                (TAG_PL_ID, pl_id),
                (TAG_PL_NAME, pl_name),
                (TAG_PL_UPDATED, updated),
            ],
            META_INT_TAGS,
        );

        // Build one entry
        const TAG_ENTRY_MUSIC_ID: u8 = 1;
        const TAG_ENTRY_SOURCE: u8 = 2;
        const TAG_ENTRY_PATH: u8 = 3;
        const TAG_ENTRY_TITLE: u8 = 4;
        const TAG_ENTRY_DURATION: u8 = 5;
        const ENTRY_INT_TAGS: &[u8] = &[5, 6, 7, 8];

        let music_id = b"track_001";
        let source = b"source_001";
        let path = b"/music/track.mp3";
        let title = b"Track Title";
        let duration = 240_000u64;

        let entry_record = encode_record(
            &[
                (TAG_ENTRY_MUSIC_ID, music_id),
                (TAG_ENTRY_SOURCE, source),
                (TAG_ENTRY_PATH, path),
                (TAG_ENTRY_TITLE, title),
                (TAG_ENTRY_DURATION, &duration.to_be_bytes()),
            ],
            ENTRY_INT_TAGS,
        );

        let pl_container = build_pl_container(meta_record, entry_record);

        // Wrap WDMMPL01 as BK section 6 (PLAYLISTS) via tag1=blob
        const TAG_PL_BLOB: u8 = 1;
        let mut pl_section = Vec::new();
        pl_section.push(TAG_PL_BLOB);
        pl_section.extend_from_slice(&encode_varint(pl_container.len() as u64));
        pl_section.extend_from_slice(&pl_container);
        pl_section.push(0); // end marker

        let bk_sections = vec![(SEC_PLAYLISTS, pl_section)];
        let bk = build_bk_container(&bk_sections);

        // Restore
        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        let msg = restore_backup_bytes(&lib, None, &bk, "", None).unwrap();
        assert!(msg.contains("playlists"));
        assert!(msg.contains("android-binary"));

        // Verify playlist
        let playlists = lib.list_playlists().unwrap();
        assert_eq!(playlists.len(), 1);
        let pl = &playlists[0];
        assert_eq!(pl.id, "playlist_001");
        assert_eq!(pl.name, "MyPlaylist");

        // Verify entry
        let entries = lib.list_playlist_entries(&pl.id).unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.music_id, "track_001");
        assert_eq!(entry.source_name, "source_001");
        assert_eq!(entry.remote_path, "/music/track.mp3");
        assert_eq!(entry.title, "Track Title");
        assert_eq!(entry.duration_ms, 240_000);
    }

    #[test]
    fn aesgcmv1_roundtrip_field() {
        // Encrypt a field value manually matching Android CredentialVaultCrypto wire
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};
        use pbkdf2::pbkdf2_hmac;
        use sha2::Sha256;

        let plaintext = "secret_value";
        let passphrase = "testpass";

        // Android CredentialVaultCrypto uses 120k iterations for field encryption
        let salt = b"saltsaltsaltsalt"; // 16 bytes
        let nonce_bytes = b"noncenonceno"; // 12 bytes
        let mut key = [0u8; 32];
        pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, 120_000, &mut key);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = Nonce::from_slice(nonce_bytes);
        let ct = cipher.encrypt(nonce, plaintext.as_bytes()).unwrap();

        // Build wire format: salt|nonce|ct (ct includes tag at end)
        let mut wire = Vec::new();
        wire.extend_from_slice(salt);
        wire.extend_from_slice(nonce_bytes);
        wire.extend_from_slice(&ct);

        // Encode as AESGCMv1:base64
        let encoded = format!(
            "AESGCMv1:{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wire)
        );

        // Decrypt via try_decrypt_aesgcmv1
        let decrypted = try_decrypt_aesgcmv1(&encoded, passphrase).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn pass_check_present_with_passphrase() {
        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();

        // Create backup WITH passphrase
        let bk_bytes = build_backup_bytes(&lib, None, "test_pass", None).unwrap();

        // Decrypt envelope to get BK container
        let bk_container = envelope_decrypt(&bk_bytes, "test_pass").unwrap();

        // Parse BK container (already decompresses sections)
        let sections = parse_bk_container(&bk_container).unwrap();

        // Verify PASS_CHECK section (11) exists and contains AESGCMv1:...
        let pass_check_raw = sections.get(&SEC_PASS_CHECK).expect("PASS_CHECK section should exist");
        let token = std::str::from_utf8(pass_check_raw).unwrap();
        assert!(token.starts_with("AESGCMv1:"), "Token should start with AESGCMv1:");

        // Decrypt and verify
        const PLAIN_TOKEN: &str = "WDMM-BACKUP-PASS-OK\u{0001}";
        let decrypted = try_decrypt_aesgcmv1(token, "test_pass").unwrap();
        assert_eq!(decrypted, PLAIN_TOKEN);

        // Verify META note has hasPassCheck
        let meta_raw = sections.get(&SEC_META).unwrap();
        let meta_records = decode_records(meta_raw, META_INT_TAGS).unwrap();
        let meta_rec = &meta_records[0];
        let note_str = meta_rec.get(&TAG_META_NOTE).and_then(|v| v.as_str()).unwrap();
        let note_json: serde_json::Value = serde_json::from_str(note_str).unwrap();
        assert_eq!(note_json["hasPassCheck"].as_bool(), Some(true));

        // Create backup WITHOUT passphrase
        let bk_empty = build_backup_bytes(&lib, None, "", None).unwrap();
        let bk_empty_container = envelope_decrypt(&bk_empty, "").unwrap();
        let sections_empty = parse_bk_container(&bk_empty_container).unwrap();

        // Verify PASS_CHECK section does NOT exist
        assert!(sections_empty.get(&SEC_PASS_CHECK).is_none(), "PASS_CHECK should not exist with empty passphrase");
    }

    #[test]
    fn cue_albums_section() {
        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();

        // Insert a CueAlbum
        use wdmm_storage::CueAlbum;
        let album = CueAlbum {
            cue_id: "cue_001".to_string(),
            source_name: "webdav_test".to_string(),
            cue_remote_path: "/music/album.cue".to_string(),
            title: Some("Test Album".to_string()),
            performer: Some("Test Artist".to_string()),
            cache_group_id: Some("group_1".to_string()),
            created_at: "2024-01-01T00:00:00Z".to_string(),
        };
        lib.upsert_cue_album(&album).unwrap();

        // Create backup
        let bk_bytes = build_backup_bytes(&lib, None, "", None).unwrap();
        let bk_container = envelope_decrypt(&bk_bytes, "").unwrap();
        let sections = parse_bk_container(&bk_container).unwrap();

        // Verify CUEALBUMS section (8) exists (already decompressed)
        let cue_albums_raw = sections.get(&SEC_CUEALBUMS).expect("CUEALBUMS section should exist");

        // Parse as JSON array
        let albums: Vec<serde_json::Value> = serde_json::from_slice(cue_albums_raw).unwrap();
        assert_eq!(albums.len(), 1);

        let album_json = &albums[0];
        assert_eq!(album_json["cue_id"].as_str(), Some("cue_001"));
        assert_eq!(album_json["source_name"].as_str(), Some("webdav_test"));
        assert_eq!(album_json["cue_remote_path"].as_str(), Some("/music/album.cue"));
        assert_eq!(album_json["title"].as_str(), Some("Test Album"));
        assert_eq!(album_json["performer"].as_str(), Some("Test Artist"));
    }

    #[test]
    fn cue_slices_in_tracks() {
        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();

        // Insert a regular track
        let regular_track = wdmm_core::models::LibraryTrack {
            id: "track_regular".to_string(),
            account_id: "webdav_test".to_string(),
            remote_path: "/music/regular.mp3".to_string(),
            title: "Regular Track".to_string(),
            artist: Some("Regular Artist".to_string()),
            album: None,
            album_artist: None,
            genre: None,
            duration_ms: 180000,
            track_number: Some(1),
            disc_number: None,
            year: None,
            album_id: None,
            artist_id: None,
            size: 0,
            modified: None,
            mime_type: None,
            rev: 0,
            cover_state: 0,
        };
        lib.upsert_track(&regular_track).unwrap();

        // Insert a CueAlbum
        use wdmm_storage::{CueAlbum, CueSlice};
        let album = CueAlbum {
            cue_id: "cue_001".to_string(),
            source_name: "webdav_test".to_string(),
            cue_remote_path: "/music/album.cue".to_string(),
            title: Some("CUE Album".to_string()),
            performer: Some("CUE Artist".to_string()),
            cache_group_id: Some("group_1".to_string()),
            created_at: "2024-01-01T00:00:00Z".to_string(),
        };
        lib.upsert_cue_album(&album).unwrap();

        // Insert a CueSlice
        let slice = CueSlice {
            music_id: "slice_001".to_string(),
            cue_id: "cue_001".to_string(),
            audio_music_id: "audio_001".to_string(),
            source_name: "webdav_test".to_string(),
            remote_path: "/music/virtual_track_01.flac".to_string(),
            file_name: "virtual_track_01.flac".to_string(),
            track_index: 1,
            title: Some("CUE Track 1".to_string()),
            artist: Some("CUE Artist".to_string()),
            album_artist: None,
            album: Some("CUE Album".to_string()),
            duration_ms: Some(240000),
            track_number: Some(1),
            track_total: Some(10),
            disc_number: None,
            disc_total: None,
            year: Some(2024),
            genre: None,
            bitrate: Some(1411),
            sample_rate: Some(44100),
            cover_state: 0,
            audio_remote_path: Some("/music/audio.flac".to_string()),
            clip_start_ms: Some(0),
            clip_end_ms: Some(240000),
            cache_group_id: Some("group_1".to_string()),
            rev: 1,
            last_downloaded_at: "2024-01-01T00:00:00Z".to_string(),
            last_tag_read_at: "2024-01-01T00:00:00Z".to_string(),
            remote_missing: 0,
        };
        lib.upsert_cue_slice(&slice).unwrap();

        // Create backup
        let bk_bytes = build_backup_bytes(&lib, None, "", None).unwrap();
        let bk_container = envelope_decrypt(&bk_bytes, "").unwrap();
        let sections = parse_bk_container(&bk_container).unwrap();

        // Verify TRACKS section contains both regular and CUE slice (already decompressed)
        let tracks_raw = sections.get(&SEC_TRACKS).expect("TRACKS section should exist");

        let track_records = decode_records(tracks_raw, TRACK_INT_TAGS).unwrap();
        assert_eq!(track_records.len(), 2, "Should have 2 track records (1 regular + 1 CUE slice)");

        // First record should be regular track (no cueId)
        let first = &track_records[0];
        assert!(first.get(&TAG_TRACK_CUE_ID).is_none(), "First track should not have cueId");
        assert_eq!(first.get(&TAG_TRACK_TITLE).and_then(|v| v.as_str()), Some("Regular Track"));

        // Second record should be CUE slice (has cueId)
        let second = &track_records[1];
        assert_eq!(second.get(&TAG_TRACK_CUE_ID).and_then(|v| v.as_str()), Some("cue_001"), "Second track should have cueId");
        assert_eq!(second.get(&TAG_TRACK_TITLE).and_then(|v| v.as_str()), Some("CUE Track 1"));
        assert_eq!(second.get(&TAG_TRACK_CUE_REMOTE_PATH).and_then(|v| v.as_str()), Some("/music/album.cue"));
        assert_eq!(second.get(&TAG_TRACK_CUE_TRACK_INDEX).and_then(|v| v.as_int()), Some(1));
        assert_eq!(second.get(&TAG_TRACK_AUDIO_MUSIC_ID).and_then(|v| v.as_str()), Some("audio_001"));
    }

    // ── Cover roundtrip tests ────────────────────────────────────────────────

    fn test_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wdmm_backup_test_{tag}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn covers_roundtrip_music() {
        use crate::cover::{music_cover_stem, CoverRoots};

        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        lib.insert_account(&wdmm_core::models::CloudAccount {
            id: "acc1".into(),
            provider_type: "webdav".into(),
            display_name: "MyVault".into(),
            base_url: None,
            credentials_encrypted: vec![],
        }).unwrap();
        lib.upsert_track(&wdmm_core::models::LibraryTrack {
            id: "t1".into(),
            account_id: "acc1".into(),
            remote_path: "/Music/track.mp3".into(),
            title: "Track".into(),
            artist: None, album: None, album_artist: None, genre: None,
            duration_ms: 1000, track_number: None, disc_number: None,
            year: None, album_id: None, artist_id: None, size: 0,
            modified: None, mime_type: None, rev: 0,
            cover_state: 0,
        }).unwrap();

        // Create a fake cover file in the music covers dir
        let music_dir = test_dir("covers_music_create");
        let manga_dir = test_dir("covers_manga_create");
        let stem = music_cover_stem("MyVault", "/Music/track.mp3");
        let fake_jpeg = b"\xff\xd8\xff\xe0fake_cover_music";
        std::fs::write(music_dir.join(&stem), fake_jpeg).unwrap();

        let roots = CoverRoots {
            music_covers_dir: music_dir.clone(),
            manga_covers_dir: manga_dir.clone(),
        };

        // Build backup with cover roots
        let bk = build_backup_bytes(&lib, None, "", Some(&roots)).unwrap();

        // Verify COVERS section present
        let container = envelope_decrypt(&bk, "").unwrap();
        let secs = parse_bk_container(&container).unwrap();
        let covers_raw = secs.get(&SEC_COVERS).expect("COVERS section must be present");
        assert!(!covers_raw.is_empty(), "COVERS section must not be empty");

        // Verify track record has coverIndex=0
        let tracks_raw = secs.get(&SEC_TRACKS).unwrap();
        let recs = decode_records(tracks_raw, TRACK_INT_TAGS).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].get(&TAG_TRACK_COVER_INDEX).and_then(|v| v.as_int()),
            Some(0),
            "track record must carry coverIndex=0"
        );

        // Restore into fresh DB+dir and verify cover file written
        let lib2 = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        let music_dir2 = test_dir("covers_music_restore");
        let manga_dir2 = test_dir("covers_manga_restore");
        lib2.insert_account(&wdmm_core::models::CloudAccount {
            id: "acc1".into(),
            provider_type: "webdav".into(),
            display_name: "MyVault".into(),
            base_url: None,
            credentials_encrypted: vec![],
        }).unwrap();
        let roots2 = CoverRoots {
            music_covers_dir: music_dir2.clone(),
            manga_covers_dir: manga_dir2.clone(),
        };
        let msg = restore_backup_bytes(&lib2, None, &bk, "", Some(&roots2)).unwrap();
        assert!(msg.contains("covers"), "restore note should include 'covers', got: {msg}");
        let restored_cover = std::fs::read(music_dir2.join(&stem)).expect("cover file should be written");
        assert_eq!(&restored_cover, fake_jpeg);
    }

    #[test]
    fn covers_roundtrip_manga() {
        use crate::cover::{manga_cover_stem, CoverRoots};
        use wdmm_storage::Manga;

        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        let manga = Manga {
            id: "manga_abc_12345678".into(),
            source_name: "src".into(),
            remote_path: "/manga/test".into(),
            title: "Test Manga".into(),
            author: None,
            series: None,
            volume: None,
            chapter_number: None,
            page_count: 0,
            cover_state: 0,
            format: "cbz".into(),
            remote_missing: 0,
            page_turn: None,
            rev: 0,
            updated_at: "2024-01-01T00:00:00Z".into(),
        };
        lib.upsert_manga(&manga).unwrap();

        let music_dir = test_dir("manga_covers_music_create");
        let manga_dir = test_dir("manga_covers_create");
        let stem = manga_cover_stem(&manga.id);
        let fake_jpeg = b"\xff\xd8\xff\xe0fake_manga_cover";
        std::fs::write(manga_dir.join(&stem), fake_jpeg).unwrap();

        let roots = CoverRoots {
            music_covers_dir: music_dir.clone(),
            manga_covers_dir: manga_dir.clone(),
        };

        let bk = build_backup_bytes(&lib, None, "", Some(&roots)).unwrap();

        // Verify MANGA_COVERS section present
        let container = envelope_decrypt(&bk, "").unwrap();
        let secs = parse_bk_container(&container).unwrap();
        let mc_raw = secs.get(&SEC_MANGA_COVERS).expect("MANGA_COVERS section must be present");
        assert!(!mc_raw.is_empty());

        // Verify manga JSON contains cover_index
        let ml_raw = secs.get(&SEC_MANGA_LIBRARY).unwrap();
        let ml_val: serde_json::Value = serde_json::from_slice(ml_raw).unwrap();
        let mangas = ml_val["mangas"].as_array().unwrap();
        assert_eq!(mangas.len(), 1);
        assert_eq!(mangas[0]["cover_index"].as_u64(), Some(0));

        // Restore and verify cover file written
        let lib2 = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        let music_dir2 = test_dir("manga_covers_music_restore");
        let manga_dir2 = test_dir("manga_covers_restore");
        let roots2 = CoverRoots {
            music_covers_dir: music_dir2.clone(),
            manga_covers_dir: manga_dir2.clone(),
        };
        let msg = restore_backup_bytes(&lib2, None, &bk, "", Some(&roots2)).unwrap();
        assert!(msg.contains("manga_covers"), "expected manga_covers note, got: {msg}");
        let restored = std::fs::read(manga_dir2.join(&stem)).expect("manga cover must be written");
        assert_eq!(&restored, fake_jpeg);

        // Manga upserted without cover_index in DB (Manga struct has no such field)
        let mangas2 = lib2.list_mangas().unwrap();
        assert_eq!(mangas2.len(), 1);
        assert_eq!(mangas2[0].id, "manga_abc_12345678");
    }

    #[test]
    fn covers_omitted_when_roots_not_set() {
        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        lib.upsert_track(&wdmm_core::models::LibraryTrack {
            id: "t1".into(), account_id: "acc".into(),
            remote_path: "/t.mp3".into(), title: "T".into(),
            artist: None, album: None, album_artist: None, genre: None,
            duration_ms: 0, track_number: None, disc_number: None,
            year: None, album_id: None, artist_id: None, size: 0,
            modified: None, mime_type: None, rev: 0,
            cover_state: 0,
        }).unwrap();

        // No cover_roots → no COVERS/MANGA_COVERS sections
        let bk = build_backup_bytes(&lib, None, "", None).unwrap();
        let container = envelope_decrypt(&bk, "").unwrap();
        let secs = parse_bk_container(&container).unwrap();
        assert!(secs.get(&SEC_COVERS).is_none(), "COVERS must be absent when roots not set");
        assert!(secs.get(&SEC_MANGA_COVERS).is_none(), "MANGA_COVERS must be absent when roots not set");

        // Track record must not carry coverIndex
        let tracks_raw = secs.get(&SEC_TRACKS).unwrap();
        let recs = decode_records(tracks_raw, TRACK_INT_TAGS).unwrap();
        assert_eq!(recs.len(), 1);
        assert!(recs[0].get(&TAG_TRACK_COVER_INDEX).is_none(), "no coverIndex without roots");
    }

    #[test]
    fn covers_missing_files_tolerant() {
        use crate::cover::CoverRoots;

        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        lib.upsert_track(&wdmm_core::models::LibraryTrack {
            id: "t1".into(), account_id: "acc".into(),
            remote_path: "/t.mp3".into(), title: "T".into(),
            artist: None, album: None, album_artist: None, genre: None,
            duration_ms: 0, track_number: None, disc_number: None,
            year: None, album_id: None, artist_id: None, size: 0,
            modified: None, mime_type: None, rev: 0,
            cover_state: 0,
        }).unwrap();

        // Roots configured but cover file missing → no COVERS section, no coverIndex
        let music_dir = test_dir("missing_covers_music");
        let manga_dir = test_dir("missing_covers_manga");
        let roots = CoverRoots {
            music_covers_dir: music_dir,
            manga_covers_dir: manga_dir,
        };
        let bk = build_backup_bytes(&lib, None, "", Some(&roots)).unwrap();
        let container = envelope_decrypt(&bk, "").unwrap();
        let secs = parse_bk_container(&container).unwrap();
        assert!(secs.get(&SEC_COVERS).is_none(), "COVERS absent when files missing");
        let tracks_raw = secs.get(&SEC_TRACKS).unwrap();
        let recs = decode_records(tracks_raw, TRACK_INT_TAGS).unwrap();
        assert!(recs[0].get(&TAG_TRACK_COVER_INDEX).is_none(), "no coverIndex when file missing");
    }

    /// Verify that a COVERS section emitted by Android (kind=2 = WmpImageKind::jpeg) is written
    /// correctly on restore.  Before the WmpImageKind parity fix the restore code filtered on
    /// `kind == COVER_KIND_JPEG` where COVER_KIND_JPEG was 0 — matching Android's "none" — so
    /// every cover from a real Android archive was silently dropped.
    #[test]
    fn android_kind2_jpeg_covers_restore() {
        use crate::cover::CoverRoots;

        let lib = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        lib.insert_account(&wdmm_core::models::CloudAccount {
            id: "acc1".into(),
            provider_type: "webdav".into(),
            display_name: "TestVault".into(),
            base_url: None,
            credentials_encrypted: vec![],
        }).unwrap();

        // Real JPEG magic — Android WmpImageKind::detect returns jpeg(2) for FF D8 FF.
        let jpeg_bytes: &[u8] = b"\xff\xd8\xff\xe0android_kind2_cover";

        // Build a raw COVERS section the way Android would: kind byte = 2 (jpeg).
        let mut covers_section: Vec<u8> = Vec::new();
        covers_section.push(cover::COVER_KIND_JPEG); // == 2
        covers_section.extend_from_slice(&(jpeg_bytes.len() as u32).to_le_bytes());
        covers_section.extend_from_slice(jpeg_bytes);

        // Build a binary TRACKS payload: one track with TAG_TRACK_COVER_INDEX = 0.
        let mut track_bytes: Vec<u8> = Vec::new();
        // TAG_TRACK_SOURCE = 1 (string)
        track_bytes.push(1u8);
        track_bytes.extend_from_slice(&encode_varint(b"TestVault".len() as u64));
        track_bytes.extend_from_slice(b"TestVault");
        // TAG_TRACK_REMOTE_PATH = 2 (string)
        track_bytes.push(2u8);
        track_bytes.extend_from_slice(&encode_varint(b"/Music/kind2test.mp3".len() as u64));
        track_bytes.extend_from_slice(b"/Music/kind2test.mp3");
        // TAG_TRACK_TITLE = 5 (string)
        track_bytes.push(5u8);
        track_bytes.extend_from_slice(&encode_varint(b"Kind2 Test".len() as u64));
        track_bytes.extend_from_slice(b"Kind2 Test");
        // TAG_TRACK_COVER_INDEX = 26 (int tag)
        track_bytes.push(TAG_TRACK_COVER_INDEX);
        track_bytes.extend_from_slice(&encode_varint(0u64));
        track_bytes.push(0u8); // end-of-record marker

        // Wrap in WDMMBK01: TRACKS section deflated, COVERS section raw.
        let bk = build_wmp_container(
            &[
                (SEC_TRACKS, track_bytes, Codec::Deflate),
                (SEC_COVERS, covers_section, Codec::Raw),
            ],
            MAGIC_BK,
        );

        let music_dir = test_dir("kind2_covers_music");
        let manga_dir = test_dir("kind2_covers_manga");
        let roots = CoverRoots {
            music_covers_dir: music_dir.clone(),
            manga_covers_dir: manga_dir,
        };

        let msg = restore_backup_bytes(&lib, None, &bk, "", Some(&roots)).unwrap();
        assert!(msg.contains("covers"), "restore note must include 'covers', got: {msg}");

        // Verify the cover file was written with the correct bytes.
        let stem = cover::music_cover_stem("TestVault", "/Music/kind2test.mp3");
        let written = std::fs::read(music_dir.join(&stem))
            .expect("cover file must be written for Android kind=2 (JPEG) entry");
        assert_eq!(&written, jpeg_bytes);
    }
}
