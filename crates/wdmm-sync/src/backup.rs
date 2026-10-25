//! WDMMBK01 backup archive (+ optional WDMMEN01 passphrase envelope).
//!
//! Section payloads are UTF-8 JSON (raw codec) so Core Desktop can round-trip.
//! Container header matches Android `WmpContainer` (docs/10): magic + section table.
//! Envelope matches Android `BackupCrypto` (WDMMEN01, PBKDF2 120_000).

use crate::SyncError;
use crate::SyncResult;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use wdmm_drivers::{CloudDriver, CloudFileItem};
use wdmm_storage::{DmrPrefRow, DmrPrefsDatabase, LibraryDatabase, Playlist, PlaylistEntry};

/// Android WmpSections ids we emit/consume.
const SEC_CREDENTIALS: u8 = 5;
const SEC_PLAYLISTS: u8 = 6;
const SEC_SETTINGS: u8 = 7;
const SEC_MANGA_LIBRARY: u8 = 13;
const SEC_USER_FOLDERS: u8 = 14;
const SEC_LIBRARY_JSON: u8 = 2; // tracks section — we store JSON snapshot
const SEC_DMR_PREFS: u8 = 16; // Core extension; Android ignores unknown ids

const MAGIC_BK: &[u8; 8] = b"WDMMBK01";
const MAGIC_EN: &[u8; 8] = b"WDMMEN01";
const PBKDF2_ITERS_ENVELOPE: u32 = 120_000;

fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (!(crc & 1)).wrapping_add(1); // 0 or 0xFFFFFFFF
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Build a raw (unencrypted) WDMMBK01 container from section id → payload bytes.
pub fn build_bk_container(sections: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let count = sections.len() as u16;
    let header_len = 12usize;
    let table_len = sections.len() * 18;
    let payload_start = header_len + table_len;
    let mut payload = Vec::new();
    let mut table = Vec::with_capacity(table_len);
    for &(id, ref raw) in sections {
        let offset = (payload_start + payload.len()) as u32;
        let raw_len = raw.len() as u32;
        let crc = crc32_ieee(raw);
        table.push(id);
        table.push(0u8); // codec = raw
        table.extend_from_slice(&offset.to_le_bytes());
        table.extend_from_slice(&raw_len.to_le_bytes()); // storedLen
        table.extend_from_slice(&raw_len.to_le_bytes()); // rawLen
        table.extend_from_slice(&crc.to_le_bytes());
        payload.extend_from_slice(raw);
    }
    let mut out = Vec::with_capacity(payload_start + payload.len());
    out.extend_from_slice(MAGIC_BK);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&table);
    out.extend_from_slice(&payload);
    out
}

/// Parse WDMMBK01 → map section id → raw bytes. Unknown codecs rejected.
pub fn parse_bk_container(bytes: &[u8]) -> SyncResult<BTreeMap<u8, Vec<u8>>> {
    if bytes.len() < 12 || &bytes[0..8] != MAGIC_BK {
        return Err(SyncError::Other("error:notBackupArchive|not WDMMBK01".into()));
    }
    let count = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let _flags = u16::from_le_bytes([bytes[10], bytes[11]]);
    let table_end = 12 + count * 18;
    if bytes.len() < table_end {
        return Err(SyncError::Other("truncated section table".into()));
    }
    let mut map = BTreeMap::new();
    for i in 0..count {
        let off = 12 + i * 18;
        let id = bytes[off];
        let codec = bytes[off + 1];
        let offset = u32::from_le_bytes(bytes[off + 2..off + 6].try_into().unwrap()) as usize;
        let stored_len = u32::from_le_bytes(bytes[off + 6..off + 10].try_into().unwrap()) as usize;
        let raw_len = u32::from_le_bytes(bytes[off + 10..off + 14].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[off + 14..off + 18].try_into().unwrap());
        if codec != 0 {
            return Err(SyncError::Other(format!("unsupported section codec {codec}")));
        }
        if offset + stored_len > bytes.len() || stored_len != raw_len {
            return Err(SyncError::Other(format!("section {id} bounds")));
        }
        let raw = bytes[offset..offset + stored_len].to_vec();
        if crc32_ieee(&raw) != crc {
            return Err(SyncError::Other(format!("section {id} CRC mismatch")));
        }
        map.insert(id, raw);
    }
    Ok(map)
}

/// Wrap BK bytes in WDMMEN01 envelope (Android BackupCrypto-compatible params).
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

/// Snapshot library + optional DMR prefs into BK (+ optional envelope) bytes.
pub fn build_backup_bytes(
    lib: &LibraryDatabase,
    dmr: Option<&DmrPrefsDatabase>,
    passphrase: &str,
) -> SyncResult<Vec<u8>> {
    let accounts = lib.list_accounts()?;
    let acc_json: Vec<_> = accounts
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "provider_type": a.provider_type,
                "display_name": a.display_name,
                "base_url": a.base_url,
                "credentials_b64": base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &a.credentials_encrypted
                ),
            })
        })
        .collect();

    let tracks = lib.list_tracks()?;
    let playlists = lib.list_playlists()?;
    let mut pl_dump = Vec::new();
    for pl in &playlists {
        let entries = lib.list_playlist_entries(&pl.id).unwrap_or_default();
        pl_dump.push(json!({
            "playlist": pl,
            "entries": entries,
        }));
    }

    let kv: BTreeMap<String, String> = lib
        .list_kv()?
        .into_iter()
        .collect();

    let mangas = lib.list_mangas().unwrap_or_default();
    let mut manga_pages = Vec::new();
    for m in &mangas {
        if let Ok(pages) = lib.list_pages(&m.id) {
            manga_pages.push(json!({"manga_id": m.id, "pages": pages}));
        }
    }
    let manga_progress = lib.list_progress().unwrap_or_default();

    // user folders: root children + recursive shallow dump
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
    // roots already listed — dump each tree
    for f in &roots {
        let items = lib.list_user_folder_items(&f.id).unwrap_or_default();
        folders.push(json!({"folder": f, "items": items}));
        walk_folders(lib, &mut folders, Some(&f.id));
    }

    let dmr_rows = match dmr {
        Some(d) => d.list()?,
        None => Vec::new(),
    };

    let sections = vec![
        (SEC_CREDENTIALS, json_bytes(&acc_json)?),
        (SEC_LIBRARY_JSON, json_bytes(&tracks)?),
        (SEC_PLAYLISTS, json_bytes(&pl_dump)?),
        (SEC_SETTINGS, json_bytes(&kv)?),
        (
            SEC_MANGA_LIBRARY,
            json_bytes(&json!({
                "mangas": mangas,
                "pages": manga_pages,
                "progress": manga_progress,
            }))?,
        ),
        (SEC_USER_FOLDERS, json_bytes(&folders)?),
        (SEC_DMR_PREFS, json_bytes(&dmr_rows)?),
    ];

    let bk = build_bk_container(&sections);
    envelope_encrypt(&bk, passphrase)
}

pub fn restore_backup_bytes(
    lib: &LibraryDatabase,
    dmr: Option<&DmrPrefsDatabase>,
    data: &[u8],
    passphrase: &str,
) -> SyncResult<String> {
    let plain = envelope_decrypt(data, passphrase)?;
    let sections = parse_bk_container(&plain)?;
    let mut notes = Vec::new();

    if let Some(raw) = sections.get(&SEC_CREDENTIALS) {
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
        notes.push("credentials");
    }

    if let Some(raw) = sections.get(&SEC_LIBRARY_JSON) {
        if let Ok(tracks) = serde_json::from_slice::<Vec<wdmm_core::models::LibraryTrack>>(raw) {
            for t in tracks {
                let _ = lib.upsert_track(&t);
            }
            notes.push("library");
        }
    }

    if let Some(raw) = sections.get(&SEC_PLAYLISTS) {
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
            notes.push("playlists");
        }
    }

    if let Some(raw) = sections.get(&SEC_SETTINGS) {
        if let Ok(map) = serde_json::from_slice::<BTreeMap<String, String>>(raw) {
            for (k, v) in map {
                let _ = lib.save_kv(&k, &v);
            }
            notes.push("settings");
        }
    }

    if let Some(raw) = sections.get(&SEC_MANGA_LIBRARY) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) {
            if let Some(arr) = v.get("mangas").and_then(|x| x.as_array()) {
                for m in arr {
                    if let Ok(manga) = serde_json::from_value::<wdmm_storage::Manga>(m.clone()) {
                        let _ = lib.upsert_manga(&manga);
                    }
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

    Ok(format!("ok:restored:{}", notes.join(",")))
}

pub fn write_backup_file(
    lib: &LibraryDatabase,
    dmr: Option<&DmrPrefsDatabase>,
    passphrase: &str,
    out_path: &Path,
) -> SyncResult<()> {
    let bytes = build_backup_bytes(lib, dmr, passphrase)?;
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
) -> SyncResult<String> {
    let data = std::fs::read(path).map_err(|e| SyncError::Other(e.to_string()))?;
    restore_backup_bytes(lib, dmr, &data, passphrase)
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
) -> SyncResult<String> {
    let bytes = build_backup_bytes(
        lib.as_ref(),
        dmr.as_ref().map(|a| a.as_ref()),
        passphrase,
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
        let bytes = build_backup_bytes(&lib, Some(&dmr), "pw").unwrap();
        let lib2 = LibraryDatabase::open(Path::new(":memory:")).unwrap();
        let dmr2 = DmrPrefsDatabase::open(Path::new(":memory:")).unwrap();
        let msg = restore_backup_bytes(&lib2, Some(&dmr2), &bytes, "pw").unwrap();
        assert!(msg.contains("settings") || msg.contains("dmr"));
        assert_eq!(lib2.load_kv("sync_interval").unwrap().as_deref(), Some("off"));
        assert_eq!(dmr2.get("u1").unwrap().unwrap().override_name.as_deref(), Some("TV"));
    }
}
