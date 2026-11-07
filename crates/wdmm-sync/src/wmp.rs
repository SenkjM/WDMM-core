//! Shared WMP (Webdav Media Manager Protocol) container helpers.
//!
//! Supports WDMM binary container format used by backup (WDMMBK01, WDMMCV01, WDMMPL01)
//! and library shards (WDMMLB01, WDMMLS01, WDMMLT01).
//!
//! Container structure: magic(8) + count(2) + flags(2) + section_table + payloads.
//! Each section: id(1) + codec(1) + offset(4) + storedLen(4) + rawLen(4) + crc32(4).

use crate::{SyncError, SyncResult};
use std::collections::BTreeMap;

/// Section IDs (Android WmpSections).
pub const SEC_META: u8 = 1;
pub const SEC_TRACKS: u8 = 2;
pub const SEC_COVERS: u8 = 3;
pub const SEC_TOMBS: u8 = 4;
pub const SEC_CREDENTIALS: u8 = 5;
pub const SEC_PLAYLISTS: u8 = 6;
pub const SEC_SETTINGS: u8 = 7;
pub const SEC_CUEALBUMS: u8 = 8;
pub const SEC_PASS_CHECK: u8 = 11;
pub const SEC_MANGAS: u8 = 12;
pub const SEC_MANGA_LIBRARY: u8 = 13;
pub const SEC_USER_FOLDERS: u8 = 14;
pub const SEC_MANGA_COVERS: u8 = 15;
pub const SEC_DMR_PREFS: u8 = 16;
/// Account deletion tombstones carried inside a vault document.
pub const SEC_ACCOUNT_DELETIONS: u8 = 17;

/// Container magic prefixes.
pub const MAGIC_BK: &[u8; 8] = b"WDMMBK01";  // backup
pub const MAGIC_CV: &[u8; 8] = b"WDMMCV01";  // credential vault
pub const MAGIC_PL: &[u8; 8] = b"WDMMPL01";  // playlist
pub const MAGIC_LB: &[u8; 8] = b"WDMMLB01";  // library base shard
pub const MAGIC_LS: &[u8; 8] = b"WDMMLS01";  // library segment shard
pub const MAGIC_LT: &[u8; 8] = b"WDMMLT01";  // library tombstone shard

/// Codec for a section: 0=raw, 1=deflate.
#[derive(Debug, Clone, Copy)]
pub enum Codec {
    Raw = 0,
    Deflate = 1,
}

/// Record value: int (varint u64), string (UTF-8), or bytes.
#[derive(Debug, Clone)]
pub enum RecordValue {
    Int(u64),
    Str(String),
    Bytes(Vec<u8>),
}

impl RecordValue {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            RecordValue::Int(v) => Some(*v as i64),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            RecordValue::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            RecordValue::Bytes(b) => Some(b.as_slice()),
            _ => None,
        }
    }
}

pub fn crc32_ieee(data: &[u8]) -> u32 {
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

/// Encode varint (u64).
pub fn encode_varint(mut val: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while val >= 0x80 {
        out.push((val as u8 & 0x7F) | 0x80);
        val >>= 7;
    }
    out.push(val as u8);
    out
}

/// Decode varint, returning (value, next_offset).
pub fn decode_varint(data: &[u8], mut at: usize) -> SyncResult<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        if at >= data.len() {
            return Err(SyncError::Other("truncated varint".into()));
        }
        let byte = data[at];
        at += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if (byte & 0x80) == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err(SyncError::Other("varint overflow".into()));
        }
    }
    Ok((result, at))
}

/// Decode one tag/value record from Android binary format.
/// Returns map of tag → value (int or String), and next offset.
pub fn decode_record(
    data: &[u8],
    mut at: usize,
    int_tags: &[u8],
) -> SyncResult<(BTreeMap<u8, RecordValue>, usize)> {
    let mut rec = BTreeMap::new();
    while at < data.len() {
        let tag = data[at];
        at += 1;
        if tag == 0 {
            // end of record
            break;
        }
        if int_tags.contains(&tag) {
            let (val, next) = decode_varint(data, at)?;
            rec.insert(tag, RecordValue::Int(val));
            at = next;
        } else {
            // length-prefixed UTF-8 string or bytes
            let (len, next) = decode_varint(data, at)?;
            at = next;
            let end = at + len as usize;
            if end > data.len() {
                return Err(SyncError::Other("record value out of bounds".into()));
            }
            let slice = &data[at..end];
            // Try UTF-8 decode; if it fails treat as bytes
            if let Ok(s) = std::str::from_utf8(slice) {
                rec.insert(tag, RecordValue::Str(s.to_string()));
            } else {
                rec.insert(tag, RecordValue::Bytes(slice.to_vec()));
            }
            at = end;
        }
    }
    Ok((rec, at))
}

/// Encode records (tag/value pairs) into Android binary format.
pub fn encode_records(records: &[BTreeMap<u8, RecordValue>], int_tags: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for rec in records {
        for (&tag, val) in rec {
            out.push(tag);
            if int_tags.contains(&tag) {
                if let RecordValue::Int(v) = val {
                    out.extend_from_slice(&encode_varint(*v));
                }
            } else {
                match val {
                    RecordValue::Str(s) => {
                        out.extend_from_slice(&encode_varint(s.len() as u64));
                        out.extend_from_slice(s.as_bytes());
                    }
                    RecordValue::Bytes(b) => {
                        out.extend_from_slice(&encode_varint(b.len() as u64));
                        out.extend_from_slice(b);
                    }
                    RecordValue::Int(_) => {
                        // Tag not in int_tags but value is int — encode as varint-prefixed empty
                        out.extend_from_slice(&encode_varint(0));
                    }
                }
            }
        }
        out.push(0); // end marker
    }
    out
}

/// Decode all records from a section.
pub fn decode_records(data: &[u8], int_tags: &[u8]) -> SyncResult<Vec<BTreeMap<u8, RecordValue>>> {
    let mut records = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let (rec, next) = decode_record(data, at, int_tags)?;
        if rec.is_empty() {
            break;
        }
        records.push(rec);
        at = next;
    }
    Ok(records)
}

/// Build a WMP container from section id → (payload, codec).
pub fn build_wmp_container(sections: &[(u8, Vec<u8>, Codec)], magic: &[u8; 8]) -> Vec<u8> {
    use flate2::write::DeflateEncoder;
    use std::io::Write;

    let count = sections.len() as u16;
    let header_len = 12usize;
    let table_len = sections.len() * 18;
    let payload_start = header_len + table_len;
    let mut payload = Vec::new();
    let mut table = Vec::with_capacity(table_len);

    for &(id, ref raw, codec) in sections {
        let offset = (payload_start + payload.len()) as u32;
        let raw_len = raw.len() as u32;
        let crc = crc32_ieee(raw);

        let (stored, codec_byte) = match codec {
            Codec::Raw => (raw.clone(), 0u8),
            Codec::Deflate => {
                let mut encoder = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(raw).expect("deflate write");
                let compressed = encoder.finish().expect("deflate finish");
                (compressed, 1u8)
            }
        };

        table.push(id);
        table.push(codec_byte);
        table.extend_from_slice(&offset.to_le_bytes());
        table.extend_from_slice(&(stored.len() as u32).to_le_bytes()); // storedLen
        table.extend_from_slice(&raw_len.to_le_bytes()); // rawLen
        table.extend_from_slice(&crc.to_le_bytes());
        payload.extend_from_slice(&stored);
    }

    let mut out = Vec::with_capacity(payload_start + payload.len());
    out.extend_from_slice(magic);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&table);
    out.extend_from_slice(&payload);
    out
}

/// Parse a WMP container, returning section_id → raw_payload map.
/// Validates magic prefix, checks CRC, inflates compressed sections.
pub fn parse_wmp_container(
    bytes: &[u8],
    expected_kind: Option<&[u8; 2]>,
) -> SyncResult<BTreeMap<u8, Vec<u8>>> {
    if bytes.len() < 12 || &bytes[0..4] != b"WDMM" {
        return Err(SyncError::Other("not a WDMM container".into()));
    }
    let kind = &bytes[4..6];
    if let Some(exp) = expected_kind {
        if kind != exp {
            return Err(SyncError::Other(format!(
                "container kind mismatch: got {}, want {}",
                String::from_utf8_lossy(kind),
                String::from_utf8_lossy(exp)
            )));
        }
    }
    let count = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let table_end = 12 + count * 18;
    if bytes.len() < table_end {
        return Err(SyncError::Other("truncated WMP section table".into()));
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

        if offset + stored_len > bytes.len() {
            return Err(SyncError::Other(format!("WMP section {id} bounds")));
        }

        let stored = &bytes[offset..offset + stored_len];
        let raw = match codec {
            0 => stored.to_vec(),
            1 => {
                use flate2::Decompress;
                let mut decomp = Decompress::new(false);
                let mut out = Vec::with_capacity(raw_len);
                decomp
                    .decompress_vec(stored, &mut out, flate2::FlushDecompress::Finish)
                    .map_err(|e| SyncError::Other(format!("WMP section {id} inflate: {e}")))?;
                if out.len() != raw_len {
                    return Err(SyncError::Other(format!(
                        "WMP section {id} length mismatch: got {}, want {raw_len}",
                        out.len()
                    )));
                }
                out
            }
            _ => {
                return Err(SyncError::Other(format!("unsupported WMP codec {codec}")));
            }
        };

        if crc32_ieee(&raw) != crc {
            return Err(SyncError::Other(format!("WMP section {id} CRC mismatch")));
        }
        map.insert(id, raw);
    }
    Ok(map)
}

/// Container file kind (magic bytes 4..6), e.g. `b"CV"`, `b"PL"`, `b"PD"`.
pub fn container_file_kind(bytes: &[u8]) -> Option<[u8; 2]> {
    if bytes.len() >= 6 && &bytes[0..4] == b"WDMM" {
        Some([bytes[4], bytes[5]])
    } else {
        None
    }
}

// ── WMP documents: credential vault / playlist / playlist deletion ───────────
//
// One implementation of the three documents the app syncs:
//
// * credential vault (`WDMMCV01`): user accounts with plaintext address fields
//   and either a plaintext or `AESGCMv1:` password. Dart only calls the FFI.
// * playlist (`WDMMPL01`): one playlist plus per-entry covers.
// * playlist deletion pack (`WDMMPD01`): deletes that must outlive the deleted
//   playlist document.
//
// The `wdmm-sync` credential engine ALSO speaks `WDMMCV01`, but it is a
// different document: `{sync_root}/accounts.wdmcv` is a **device account
// snapshot** whose `password` tag carries `CloudAccount.credentials_encrypted`
// as base64. It shares the container format (and only the format) with the
// user-facing vault `{sync_root}/credentials.wdmcv`; the two files must never be
// swapped — the vault's password is an `AESGCMv1:` blob the engine cannot read,
// and the engine's snapshot has no username at all.

/// Section id for playlist entries (`WmpSections.entries`).
pub const SEC_ENTRIES: u8 = 9;
/// Section id for playlist deletion packs (`WmpSections.playlistDeletions`).
pub const SEC_PLAYLIST_DELETIONS: u8 = 10;

/// Document kind of a playlist document (`WmpKind.playlist`).
pub const KIND_PLAYLIST: u64 = 6;
/// Document kind of a credential vault (`WmpKind.vault`).
pub const KIND_VAULT: u64 = 7;
/// Document kind of a playlist deletion pack (`WmpKind.playlistDeletion`).
pub const KIND_PLAYLIST_DELETION: u64 = 8;

/// Container file kind of a credential vault document.
pub const FILE_KIND_VAULT: &[u8; 2] = b"CV";
/// Container file kind of a playlist document.
pub const FILE_KIND_PLAYLIST: &[u8; 2] = b"PL";
/// Container file kind of a playlist deletion pack.
pub const FILE_KIND_PLAYLIST_DELETION: &[u8; 2] = b"PD";
/// Magic of a playlist deletion pack (`WDMMPD01`).
pub const MAGIC_PD: &[u8; 8] = b"WDMMPD01";

/// Remote file name of the user-facing credential vault.
pub const VAULT_FILE_NAME: &str = "credentials.wdmcv";
/// Remote file name of the sync engine's account snapshot.
///
/// Not the vault: see the module comment above. Different document, different
/// payload, never interchangeable.
pub const ENGINE_ACCOUNTS_FILE_NAME: &str = "accounts.wdmcv";
/// Extension of playlist documents and deletion packs.
pub const PLAYLIST_FILE_EXTENSION: &str = "wdmp";
/// File-name prefix of a playlist deletion pack.
pub const PLAYLIST_DELETION_PREFIX: &str = "pdel_";

// META tags shared by every document (`WmpMeta`).
pub const META_KIND: u8 = 1;
pub const META_SEQ_FROM: u8 = 2;
pub const META_SEQ_TO: u8 = 3;
pub const META_COUNT: u8 = 4;
pub const META_DEVICE_ID: u8 = 5;
pub const META_CREATED_AT: u8 = 6;
pub const META_PLAYLIST_ID: u8 = 9;
pub const META_PLAYLIST_NAME: u8 = 10;
pub const META_PLAYLIST_UPDATED_AT: u8 = 11;
pub const META_FORMAT_VERSION: u8 = 12;

const META_INT_TAGS: &[u8] = &[META_KIND, META_SEQ_FROM, META_SEQ_TO, META_COUNT];
const VAULT_META_INT_TAGS: &[u8] = &[
    META_KIND,
    META_SEQ_FROM,
    META_SEQ_TO,
    META_COUNT,
    META_FORMAT_VERSION,
];

// Vault account tags (`WmpVaultTags`).
pub const VAULT_TAG_ID: u8 = 1;
pub const VAULT_TAG_NAME: u8 = 2;
pub const VAULT_TAG_PROVIDER_TYPE: u8 = 3;
pub const VAULT_TAG_URL: u8 = 4;
pub const VAULT_TAG_USERNAME: u8 = 5;
pub const VAULT_TAG_PASSWORD: u8 = 6;
pub const VAULT_TAG_PASSWORD_ENCRYPTED: u8 = 7;
pub const VAULT_TAG_REMOTE_PATH: u8 = 8;
pub const VAULT_TAG_DRIVER_CONFIG: u8 = 9;
/// Additive tag: per-account epoch-seconds clock, used by the sync engine only.
/// Dart's vault codec has no such tag and ignores it.
pub const VAULT_TAG_ENGINE_UPDATED_AT: u8 = 10;
/// Additive tag: epoch seconds when the account was deleted.
pub const VAULT_TAG_DELETED_AT: u8 = 11;
const VAULT_RECORD_INT_TAGS: &[u8] =
    &[VAULT_TAG_PASSWORD_ENCRYPTED, VAULT_TAG_ENGINE_UPDATED_AT];
const VAULT_DELETION_INT_TAGS: &[u8] = &[VAULT_TAG_DELETED_AT];

// Playlist entry tags (`WmpPlaylistEntry`).
pub const PLAYLIST_TAG_MUSIC_ID: u8 = 1;
pub const PLAYLIST_TAG_SOURCE_NAME: u8 = 2;
pub const PLAYLIST_TAG_REMOTE_PATH: u8 = 3;
pub const PLAYLIST_TAG_TITLE: u8 = 4;
pub const PLAYLIST_TAG_DURATION_MS: u8 = 5;
pub const PLAYLIST_TAG_CUE_TRACK_INDEX: u8 = 6;
pub const PLAYLIST_TAG_COVER_INDEX: u8 = 7;
pub const PLAYLIST_TAG_REMOTE_MISSING: u8 = 8;
const PLAYLIST_ENTRY_INT_TAGS: &[u8] = &[
    PLAYLIST_TAG_DURATION_MS,
    PLAYLIST_TAG_CUE_TRACK_INDEX,
    PLAYLIST_TAG_COVER_INDEX,
    PLAYLIST_TAG_REMOTE_MISSING,
];

// Playlist deletion tags (`WmpPlaylistDeletion`).
pub const DELETION_TAG_SCOPE: u8 = 1;
pub const DELETION_TAG_MUSIC_ID: u8 = 2;
pub const DELETION_TAG_DELETED_AT: u8 = 3;
pub const DELETION_SCOPE_ENTRY: u64 = 0;
pub const DELETION_SCOPE_PLAYLIST: u64 = 1;
const DELETION_RECORD_INT_TAGS: &[u8] = &[DELETION_TAG_SCOPE];

/// RFC 3339 / ISO-8601 UTC timestamp with millisecond precision.
pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Epoch seconds → ISO-8601 UTC string.
pub fn iso8601_from_epoch(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Parse an ISO-8601 / RFC 3339 string to epoch seconds.
pub fn epoch_from_iso8601(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|at| at.timestamp())
}

fn record_str(rec: &BTreeMap<u8, RecordValue>, tag: u8) -> String {
    rec.get(&tag)
        .and_then(RecordValue::as_str)
        .unwrap_or_default()
        .to_string()
}

fn record_int(rec: &BTreeMap<u8, RecordValue>, tag: u8) -> Option<i64> {
    rec.get(&tag).and_then(RecordValue::as_int)
}

fn string_or(mut value: String, fallback: &str) -> String {
    if value.is_empty() {
        value = fallback.to_string();
    }
    value
}

fn decode_first_record(
    section: Option<&Vec<u8>>,
    int_tags: &[u8],
) -> SyncResult<BTreeMap<u8, RecordValue>> {
    let Some(raw) = section else {
        return Ok(BTreeMap::new());
    };
    Ok(decode_records(raw, int_tags)?.into_iter().next().unwrap_or_default())
}

fn push_str(rec: &mut BTreeMap<u8, RecordValue>, tag: u8, value: &str) {
    rec.insert(tag, RecordValue::Str(value.to_string()));
}

fn push_int(rec: &mut BTreeMap<u8, RecordValue>, tag: u8, value: u64) {
    rec.insert(tag, RecordValue::Int(value));
}

/// One account entry of a credential vault document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultRecordDoc {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub url: String,
    pub username: String,
    /// Plaintext, or an `AESGCMv1:` blob when [`Self::password_encrypted`].
    pub password: String,
    pub password_encrypted: bool,
    pub remote_path: String,
    /// Cloud drivers only: the driver config as a JSON object string.
    pub driver_config: Option<String>,
    /// Engine-only clock (epoch seconds); `0` when the document has none.
    pub engine_updated_at: i64,
}

impl VaultRecordDoc {
    fn to_record(&self) -> BTreeMap<u8, RecordValue> {
        let mut rec = BTreeMap::new();
        push_str(&mut rec, VAULT_TAG_ID, &self.id);
        push_str(&mut rec, VAULT_TAG_NAME, &self.name);
        push_str(&mut rec, VAULT_TAG_PROVIDER_TYPE, &self.provider_type);
        push_str(&mut rec, VAULT_TAG_URL, &self.url);
        push_str(&mut rec, VAULT_TAG_USERNAME, &self.username);
        push_str(&mut rec, VAULT_TAG_PASSWORD, &self.password);
        push_int(
            &mut rec,
            VAULT_TAG_PASSWORD_ENCRYPTED,
            u64::from(self.password_encrypted),
        );
        push_str(&mut rec, VAULT_TAG_REMOTE_PATH, &self.remote_path);
        if let Some(config) = &self.driver_config {
            push_str(&mut rec, VAULT_TAG_DRIVER_CONFIG, config);
        }
        if self.engine_updated_at > 0 {
            push_int(
                &mut rec,
                VAULT_TAG_ENGINE_UPDATED_AT,
                self.engine_updated_at as u64,
            );
        }
        rec
    }

    fn from_record(rec: &BTreeMap<u8, RecordValue>) -> Self {
        let driver_config = record_str(rec, VAULT_TAG_DRIVER_CONFIG);
        Self {
            id: record_str(rec, VAULT_TAG_ID),
            name: record_str(rec, VAULT_TAG_NAME),
            provider_type: string_or(record_str(rec, VAULT_TAG_PROVIDER_TYPE), "webdav"),
            url: record_str(rec, VAULT_TAG_URL),
            username: record_str(rec, VAULT_TAG_USERNAME),
            password: record_str(rec, VAULT_TAG_PASSWORD),
            password_encrypted: record_int(rec, VAULT_TAG_PASSWORD_ENCRYPTED).unwrap_or(0) != 0,
            remote_path: string_or(record_str(rec, VAULT_TAG_REMOTE_PATH), "/"),
            driver_config: if driver_config.is_empty() {
                None
            } else {
                Some(driver_config)
            },
            engine_updated_at: record_int(rec, VAULT_TAG_ENGINE_UPDATED_AT).unwrap_or(0),
        }
    }
}

/// One account tombstone inside a credential vault document.
///
/// The vault is a single document, so a deleted account is not simply missing:
/// a peer that did not see the deletion would republish its row. The tombstone
/// is the durable "this id is gone" statement; a live record at a later clock
/// supersedes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultDeletionDoc {
    pub id: String,
    /// Epoch seconds when the owning device deleted the account.
    pub deleted_at: i64,
}

impl VaultDeletionDoc {
    fn to_record(&self) -> BTreeMap<u8, RecordValue> {
        let mut rec = BTreeMap::new();
        push_str(&mut rec, VAULT_TAG_ID, &self.id);
        push_int(
            &mut rec,
            VAULT_TAG_DELETED_AT,
            self.deleted_at.max(0) as u64,
        );
        rec
    }

    fn from_record(rec: &BTreeMap<u8, RecordValue>) -> Self {
        Self {
            id: record_str(rec, VAULT_TAG_ID),
            deleted_at: record_int(rec, VAULT_TAG_DELETED_AT).unwrap_or(0),
        }
    }
}

/// A credential vault document (`WDMMCV01`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultDocument {
    pub format_version: u32,
    pub device_id: String,
    pub created_at: String,
    pub records: Vec<VaultRecordDoc>,
    /// Account tombstones; absent in documents written before v2.
    pub deleted: Vec<VaultDeletionDoc>,
}

impl VaultDocument {
    /// Encode as a `WDMMCV01` container.
    pub fn encode(&self) -> Vec<u8> {
        let mut meta = BTreeMap::new();
        push_int(&mut meta, META_KIND, KIND_VAULT);
        push_int(&mut meta, META_COUNT, self.records.len() as u64);
        push_str(&mut meta, META_DEVICE_ID, &self.device_id);
        let created_at = if self.created_at.is_empty() {
            now_iso8601()
        } else {
            self.created_at.clone()
        };
        push_str(&mut meta, META_CREATED_AT, &created_at);
        push_int(
            &mut meta,
            META_FORMAT_VERSION,
            u64::from(self.format_version.max(1)),
        );
        let meta_raw = encode_records(std::slice::from_ref(&meta), VAULT_META_INT_TAGS);
        let rows: Vec<BTreeMap<u8, RecordValue>> =
            self.records.iter().map(VaultRecordDoc::to_record).collect();
        let credentials_raw = encode_records(&rows, VAULT_RECORD_INT_TAGS);
        let mut sections = vec![
            (SEC_META, meta_raw, Codec::Deflate),
            (SEC_CREDENTIALS, credentials_raw, Codec::Deflate),
        ];
        if !self.deleted.is_empty() {
            let tombstones: Vec<BTreeMap<u8, RecordValue>> = self
                .deleted
                .iter()
                .map(VaultDeletionDoc::to_record)
                .collect();
            sections.push((
                SEC_ACCOUNT_DELETIONS,
                encode_records(&tombstones, VAULT_DELETION_INT_TAGS),
                Codec::Deflate,
            ));
        }
        build_wmp_container(&sections, MAGIC_CV)
    }

    /// Decode a `WDMMCV01` container.
    pub fn decode(bytes: &[u8]) -> SyncResult<Self> {
        let sections = parse_wmp_container(bytes, Some(FILE_KIND_VAULT))?;
        let meta = decode_first_record(sections.get(&SEC_META), VAULT_META_INT_TAGS)?;
        let kind = record_int(&meta, META_KIND).unwrap_or(KIND_VAULT as i64) as u64;
        if kind != KIND_VAULT {
            return Err(SyncError::Other(format!(
                "vault document carries kind {kind}, expected {KIND_VAULT}"
            )));
        }
        let records = match sections.get(&SEC_CREDENTIALS) {
            None => Vec::new(),
            Some(raw) => decode_records(raw, VAULT_RECORD_INT_TAGS)?
                .iter()
                .map(VaultRecordDoc::from_record)
                .collect(),
        };
        let deleted = match sections.get(&SEC_ACCOUNT_DELETIONS) {
            None => Vec::new(),
            Some(raw) => decode_records(raw, VAULT_DELETION_INT_TAGS)?
                .iter()
                .map(VaultDeletionDoc::from_record)
                .collect(),
        };
        Ok(Self {
            format_version: record_int(&meta, META_FORMAT_VERSION).unwrap_or(1).max(1) as u32,
            device_id: record_str(&meta, META_DEVICE_ID),
            created_at: record_str(&meta, META_CREATED_AT),
            records,
            deleted,
        })
    }
}

/// One entry of a playlist document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistEntryDoc {
    pub music_id: String,
    pub source_name: String,
    pub remote_path: String,
    pub title: String,
    pub duration_ms: Option<i64>,
    pub cue_track_index: Option<i64>,
    pub cover_index: Option<i64>,
    pub remote_missing: bool,
}

impl PlaylistEntryDoc {
    fn to_record(&self) -> BTreeMap<u8, RecordValue> {
        let mut rec = BTreeMap::new();
        push_str(&mut rec, PLAYLIST_TAG_MUSIC_ID, &self.music_id);
        push_str(&mut rec, PLAYLIST_TAG_SOURCE_NAME, &self.source_name);
        push_str(&mut rec, PLAYLIST_TAG_REMOTE_PATH, &self.remote_path);
        push_str(&mut rec, PLAYLIST_TAG_TITLE, &self.title);
        if let Some(value) = self.duration_ms {
            push_int(&mut rec, PLAYLIST_TAG_DURATION_MS, value.max(0) as u64);
        }
        if let Some(value) = self.cue_track_index {
            push_int(&mut rec, PLAYLIST_TAG_CUE_TRACK_INDEX, value.max(0) as u64);
        }
        if let Some(value) = self.cover_index {
            push_int(&mut rec, PLAYLIST_TAG_COVER_INDEX, value.max(0) as u64);
        }
        if self.remote_missing {
            push_int(&mut rec, PLAYLIST_TAG_REMOTE_MISSING, 1);
        }
        rec
    }

    fn from_record(rec: &BTreeMap<u8, RecordValue>) -> Self {
        Self {
            music_id: record_str(rec, PLAYLIST_TAG_MUSIC_ID),
            source_name: record_str(rec, PLAYLIST_TAG_SOURCE_NAME),
            remote_path: record_str(rec, PLAYLIST_TAG_REMOTE_PATH),
            title: record_str(rec, PLAYLIST_TAG_TITLE),
            duration_ms: record_int(rec, PLAYLIST_TAG_DURATION_MS),
            cue_track_index: record_int(rec, PLAYLIST_TAG_CUE_TRACK_INDEX),
            cover_index: record_int(rec, PLAYLIST_TAG_COVER_INDEX),
            remote_missing: record_int(rec, PLAYLIST_TAG_REMOTE_MISSING) == Some(1),
        }
    }
}

/// One embedded cover of a playlist document (`COVERS` section entry).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistCoverDoc {
    pub kind: u8,
    pub bytes: Vec<u8>,
}

/// A playlist document (`WDMMPL01`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistDocument {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    pub device_id: String,
    pub entries: Vec<PlaylistEntryDoc>,
    pub covers: Vec<PlaylistCoverDoc>,
}

impl PlaylistDocument {
    /// Encode as a `WDMMPL01` container. Covers are written raw.
    pub fn encode(&self) -> Vec<u8> {
        let mut meta = BTreeMap::new();
        push_int(&mut meta, META_KIND, KIND_PLAYLIST);
        push_int(&mut meta, META_COUNT, self.entries.len() as u64);
        push_str(&mut meta, META_DEVICE_ID, &self.device_id);
        let created_at = if self.created_at.is_empty() {
            now_iso8601()
        } else {
            self.created_at.clone()
        };
        push_str(&mut meta, META_CREATED_AT, &created_at);
        push_str(&mut meta, META_PLAYLIST_ID, &self.id);
        push_str(&mut meta, META_PLAYLIST_NAME, &self.name);
        let updated_at = if self.updated_at.is_empty() {
            now_iso8601()
        } else {
            self.updated_at.clone()
        };
        push_str(&mut meta, META_PLAYLIST_UPDATED_AT, &updated_at);
        let meta_raw = encode_records(std::slice::from_ref(&meta), META_INT_TAGS);
        let rows: Vec<BTreeMap<u8, RecordValue>> =
            self.entries.iter().map(PlaylistEntryDoc::to_record).collect();
        let entries_raw = encode_records(&rows, PLAYLIST_ENTRY_INT_TAGS);

        let mut sections = vec![
            (SEC_META, meta_raw, Codec::Deflate),
            (SEC_ENTRIES, entries_raw, Codec::Deflate),
        ];
        if !self.covers.is_empty() {
            sections.push((SEC_COVERS, build_cover_section_from_blobs(&self.covers), Codec::Raw));
        }
        build_wmp_container(&sections, MAGIC_PL)
    }

    /// Decode a `WDMMPL01` container.
    pub fn decode(bytes: &[u8]) -> SyncResult<Self> {
        let sections = parse_wmp_container(bytes, Some(FILE_KIND_PLAYLIST))?;
        let meta = decode_first_record(sections.get(&SEC_META), META_INT_TAGS)?;
        let kind = record_int(&meta, META_KIND).unwrap_or(-1);
        if kind != KIND_PLAYLIST as i64 {
            return Err(SyncError::Other(format!(
                "playlist document carries kind {kind}, expected {KIND_PLAYLIST}"
            )));
        }
        let id = record_str(&meta, META_PLAYLIST_ID);
        if id.is_empty() {
            return Err(SyncError::Other("playlist document has no playlist id".into()));
        }
        let entries = match sections.get(&SEC_ENTRIES) {
            None => Vec::new(),
            Some(raw) => decode_records(raw, PLAYLIST_ENTRY_INT_TAGS)?
                .iter()
                .map(PlaylistEntryDoc::from_record)
                .collect(),
        };
        let covers = match sections.get(&SEC_COVERS) {
            None => Vec::new(),
            Some(raw) => crate::parse_cover_section(raw)
                .into_iter()
                .map(|(kind, bytes)| PlaylistCoverDoc { kind, bytes })
                .collect(),
        };
        Ok(Self {
            id,
            name: record_str(&meta, META_PLAYLIST_NAME),
            created_at: record_str(&meta, META_CREATED_AT),
            updated_at: record_str(&meta, META_PLAYLIST_UPDATED_AT),
            device_id: record_str(&meta, META_DEVICE_ID),
            entries,
            covers,
        })
    }
}

/// One deletion inside a playlist deletion pack. An empty identity means the
/// whole playlist was deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistDeletionRecordDoc {
    pub entry_identity: String,
    pub deleted_at: String,
}

/// A playlist deletion pack (`WDMMPD01`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistDeletionDocument {
    pub playlist_id: String,
    pub device_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub records: Vec<PlaylistDeletionRecordDoc>,
}

impl PlaylistDeletionDocument {
    /// Encode as a `WDMMPD01` container.
    ///
    /// Records are sorted by entry identity (whole-playlist deletes first, since
    /// their identity is empty), and the document timestamp is the newest
    /// deletion.
    pub fn encode(&self) -> Vec<u8> {
        let mut records = self.records.clone();
        records.sort_by(|a, b| a.entry_identity.cmp(&b.entry_identity));
        let mut meta = BTreeMap::new();
        push_int(&mut meta, META_KIND, KIND_PLAYLIST_DELETION);
        push_int(&mut meta, META_COUNT, records.len() as u64);
        push_str(&mut meta, META_DEVICE_ID, &self.device_id);
        let created_at = if self.created_at.is_empty() {
            now_iso8601()
        } else {
            self.created_at.clone()
        };
        push_str(&mut meta, META_CREATED_AT, &created_at);
        push_str(&mut meta, META_PLAYLIST_ID, &self.playlist_id);
        let newest = records
            .iter()
            .filter_map(|record| epoch_from_iso8601(&record.deleted_at))
            .max();
        let updated_at = if !self.updated_at.is_empty() {
            self.updated_at.clone()
        } else if let Some(seconds) = newest {
            iso8601_from_epoch(seconds)
        } else {
            now_iso8601()
        };
        push_str(&mut meta, META_PLAYLIST_UPDATED_AT, &updated_at);
        let meta_raw = encode_records(std::slice::from_ref(&meta), META_INT_TAGS);
        let rows: Vec<BTreeMap<u8, RecordValue>> = records
            .iter()
            .map(|record| {
                let mut rec = BTreeMap::new();
                let whole = record.entry_identity.is_empty();
                push_int(
                    &mut rec,
                    DELETION_TAG_SCOPE,
                    if whole {
                        DELETION_SCOPE_PLAYLIST
                    } else {
                        DELETION_SCOPE_ENTRY
                    },
                );
                if !whole {
                    push_str(&mut rec, DELETION_TAG_MUSIC_ID, &record.entry_identity);
                }
                push_str(&mut rec, DELETION_TAG_DELETED_AT, &record.deleted_at);
                rec
            })
            .collect();
        let deletions_raw = encode_records(&rows, DELETION_RECORD_INT_TAGS);
        build_wmp_container(
            &[
                (SEC_META, meta_raw, Codec::Deflate),
                (SEC_PLAYLIST_DELETIONS, deletions_raw, Codec::Deflate),
            ],
            MAGIC_PD,
        )
    }

    /// Decode a `WDMMPD01` container. The playlist id comes from META, not from
    /// the file name.
    pub fn decode(bytes: &[u8]) -> SyncResult<Self> {
        let sections = parse_wmp_container(bytes, Some(FILE_KIND_PLAYLIST_DELETION))?;
        let meta = decode_first_record(sections.get(&SEC_META), META_INT_TAGS)?;
        let kind = record_int(&meta, META_KIND).unwrap_or(-1);
        if kind != KIND_PLAYLIST_DELETION as i64 {
            return Err(SyncError::Other(format!(
                "playlist deletion pack carries kind {kind}, expected {KIND_PLAYLIST_DELETION}"
            )));
        }
        let playlist_id = record_str(&meta, META_PLAYLIST_ID);
        if playlist_id.is_empty() {
            return Err(SyncError::Other(
                "playlist deletion pack has no playlist id".into(),
            ));
        }
        let mut records = Vec::new();
        if let Some(raw) = sections.get(&SEC_PLAYLIST_DELETIONS) {
            for row in decode_records(raw, DELETION_RECORD_INT_TAGS)? {
                let deleted_at = record_str(&row, DELETION_TAG_DELETED_AT);
                if epoch_from_iso8601(&deleted_at).is_none() {
                    continue; // a timestamp we cannot read is not a deletion
                }
                let scope = record_int(&row, DELETION_TAG_SCOPE).unwrap_or(0) as u64;
                let identity = record_str(&row, DELETION_TAG_MUSIC_ID);
                if scope != DELETION_SCOPE_PLAYLIST && identity.is_empty() {
                    continue;
                }
                records.push(PlaylistDeletionRecordDoc {
                    entry_identity: if scope == DELETION_SCOPE_PLAYLIST {
                        String::new()
                    } else {
                        identity
                    },
                    deleted_at,
                });
            }
        }
        Ok(Self {
            playlist_id,
            device_id: record_str(&meta, META_DEVICE_ID),
            created_at: record_str(&meta, META_CREATED_AT),
            updated_at: record_str(&meta, META_PLAYLIST_UPDATED_AT),
            records,
        })
    }
}

/// `COVERS` section: `[u8 kind][u32 LE length][bytes]` per cover.
pub fn build_cover_section_from_blobs(covers: &[PlaylistCoverDoc]) -> Vec<u8> {
    let mut out = Vec::new();
    for cover in covers {
        let kind = if cover.kind == crate::cover::COVER_KIND_NONE {
            crate::cover::detect_image_kind(&cover.bytes)
        } else {
            cover.kind
        };
        out.push(kind);
        out.extend_from_slice(&(cover.bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&cover.bytes);
    }
    out
}

/// Remote file name for a playlist mirror: `name_shortId.wdmp`.
///
/// The name is cosmetic; identity travels inside the document. Sanitising keeps
/// it safe for every WebDAV server we might talk to.
pub fn safe_playlist_file_name(name: &str, id: &str) -> String {
    let mut stem = String::new();
    let mut previous_was_space = false;
    for ch in name.trim().chars() {
        if matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            stem.push('_');
            previous_was_space = false;
        } else if ch.is_whitespace() {
            if !previous_was_space {
                stem.push('_');
                previous_was_space = true;
            }
        } else {
            stem.push(ch);
            previous_was_space = false;
        }
    }
    let stem = if stem.is_empty() {
        "playlist".to_string()
    } else {
        stem
    };
    let short_id: String = id.chars().take(8).collect();
    format!("{stem}_{short_id}.{PLAYLIST_FILE_EXTENSION}")
}

/// File name of one playlist in the sync engine's private mirror.
///
/// Identity-based (`{id}.wdmp`) because the engine never renames a local
/// playlist on the remote: a rename would otherwise leave an orphan copy under
/// the old name. The user-facing Dart mirror keeps human-readable names.
pub fn engine_playlist_file_name(id: &str) -> String {
    format!("{id}.{PLAYLIST_FILE_EXTENSION}")
}

/// File name of a playlist deletion pack (`pdel_{safe-id}.wdmp`).
pub fn playlist_deletion_file_name(playlist_id: &str) -> String {
    let mut safe = String::new();
    for ch in playlist_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            safe.push(ch);
        } else {
            safe.push('_');
        }
    }
    let stem = if safe.is_empty() {
        "playlist".to_string()
    } else {
        safe
    };
    format!("{PLAYLIST_DELETION_PREFIX}{stem}.{PLAYLIST_FILE_EXTENSION}")
}

/// Whether a listing entry looks like a playlist deletion pack.
pub fn looks_like_playlist_deletion(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with(PLAYLIST_DELETION_PREFIX)
        && lower.ends_with(&format!(".{PLAYLIST_FILE_EXTENSION}"))
}

/// Whether a listing entry looks like a playlist document (not a deletion pack).
pub fn looks_like_playlist_document(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(&format!(".{PLAYLIST_FILE_EXTENSION}"))
        && !lower.starts_with(PLAYLIST_DELETION_PREFIX)
}

#[cfg(test)]
mod document_tests {
    use super::*;

    fn sample_vault() -> VaultDocument {
        VaultDocument {
            format_version: 2,
            device_id: "dev-1".into(),
            created_at: "2026-01-02T03:04:05.000Z".into(),
            deleted: Vec::new(),
            records: vec![
                VaultRecordDoc {
                    id: "acc-1".into(),
                    name: "NAS".into(),
                    provider_type: "webdav".into(),
                    url: "https://nas.local/dav/".into(),
                    username: "alice".into(),
                    password: "AESGCMv1:abc".into(),
                    password_encrypted: true,
                    remote_path: "/".into(),
                    driver_config: None,
                    engine_updated_at: 0,
                },
                VaultRecordDoc {
                    id: "acc-2".into(),
                    name: "Baidu".into(),
                    provider_type: "baidu".into(),
                    url: String::new(),
                    username: String::new(),
                    password: String::new(),
                    password_encrypted: false,
                    remote_path: "/".into(),
                    driver_config: Some(r#"{"cookie":"x"}"#.into()),
                    engine_updated_at: 0,
                },
            ],
        }
    }

    #[test]
    fn vault_document_round_trips() {
        let doc = sample_vault();
        let bytes = doc.encode();
        assert_eq!(&bytes[0..8], MAGIC_CV);
        assert_eq!(container_file_kind(&bytes), Some(*FILE_KIND_VAULT));
        let decoded = VaultDocument::decode(&bytes).unwrap();
        assert_eq!(decoded, doc);
    }

    #[test]
    fn vault_document_keeps_engine_clock_and_empty_optional_fields() {
        let mut doc = sample_vault();
        doc.records[0].engine_updated_at = 1_700_000_000;
        doc.device_id.clear();
        let decoded = VaultDocument::decode(&doc.encode()).unwrap();
        assert_eq!(decoded.records[0].engine_updated_at, 1_700_000_000);
        assert_eq!(decoded.device_id, "");
        // Defaults injected by the decoder, not stored on the wire.
        assert_eq!(decoded.records[1].provider_type, "baidu");
    }

    #[test]
    fn dart_written_vault_without_engine_tags_decodes() {
        // A document with META/CREDENTIALS only, exactly what the old Dart codec
        // produced: no engine clock, passwordEncrypted still an int tag.
        let mut meta = BTreeMap::new();
        push_int(&mut meta, META_KIND, KIND_VAULT);
        push_int(&mut meta, META_COUNT, 1);
        push_str(&mut meta, META_DEVICE_ID, "phone");
        push_str(&mut meta, META_CREATED_AT, "2026-05-06T07:08:09.000Z");
        push_int(&mut meta, META_FORMAT_VERSION, 1);
        let mut rec = BTreeMap::new();
        push_str(&mut rec, VAULT_TAG_ID, "acc-9");
        push_str(&mut rec, VAULT_TAG_PROVIDER_TYPE, "webdav");
        push_str(&mut rec, VAULT_TAG_URL, "https://example.com/dav/");
        push_str(&mut rec, VAULT_TAG_USERNAME, "bob");
        push_str(&mut rec, VAULT_TAG_PASSWORD, "secret");
        push_int(&mut rec, VAULT_TAG_PASSWORD_ENCRYPTED, 0);
        let bytes = build_wmp_container(
            &[
                (
                    SEC_META,
                    encode_records(std::slice::from_ref(&meta), VAULT_META_INT_TAGS),
                    Codec::Deflate,
                ),
                (
                    SEC_CREDENTIALS,
                    encode_records(std::slice::from_ref(&rec), VAULT_RECORD_INT_TAGS),
                    Codec::Deflate,
                ),
            ],
            MAGIC_CV,
        );
        let decoded = VaultDocument::decode(&bytes).unwrap();
        assert_eq!(decoded.format_version, 1);
        assert_eq!(decoded.device_id, "phone");
        assert_eq!(decoded.records.len(), 1);
        assert_eq!(decoded.records[0].id, "acc-9");
        assert_eq!(decoded.records[0].username, "bob");
        assert!(!decoded.records[0].password_encrypted);
        assert_eq!(decoded.records[0].remote_path, "/");
        assert_eq!(decoded.records[0].engine_updated_at, 0);
    }

    #[test]
    fn engine_snapshot_and_vault_are_different_documents() {
        // Same container magic, different files and different payload semantics:
        // the engine snapshot carries the encrypted credential blob as base64 in
        // the password tag, the vault carries a plaintext or AESGCMv1 password.
        assert_eq!(VAULT_FILE_NAME, "credentials.wdmcv");
        assert_eq!(ENGINE_ACCOUNTS_FILE_NAME, "accounts.wdmcv");
        assert_ne!(VAULT_FILE_NAME, ENGINE_ACCOUNTS_FILE_NAME);
        let mut snapshot = sample_vault();
        snapshot.records[0].username.clear(); // the engine snapshot has no username
        snapshot.records[0].password = "AQIDBA==".into(); // opaque b64 blob
        let bytes = snapshot.encode();
        assert_eq!(&bytes[0..8], MAGIC_CV, "shared format");
        // A decoded snapshot must never be mistaken for a vault: the record has
        // no username and its password is not AESGCMv1-prefixed.
        let decoded = VaultDocument::decode(&bytes).unwrap();
        assert!(decoded.records[0].username.is_empty());
        assert!(!decoded.records[0].password.starts_with("AESGCMv1:"));
    }

    fn sample_playlist() -> PlaylistDocument {
        PlaylistDocument {
            id: "pl-1".into(),
            name: "最爱".into(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-02-01T00:00:00.000Z".into(),
            device_id: "phone".into(),
            entries: vec![
                PlaylistEntryDoc {
                    music_id: "id-a".into(),
                    source_name: "NAS".into(),
                    remote_path: "/a.mp3".into(),
                    title: "A".into(),
                    duration_ms: Some(1234),
                    cue_track_index: None,
                    cover_index: Some(0),
                    remote_missing: false,
                },
                PlaylistEntryDoc {
                    music_id: "id-b".into(),
                    source_name: "NAS".into(),
                    remote_path: "/b.flac".into(),
                    title: String::new(),
                    duration_ms: None,
                    cue_track_index: Some(3),
                    cover_index: None,
                    remote_missing: true,
                },
            ],
            covers: vec![PlaylistCoverDoc {
                kind: crate::cover::COVER_KIND_JPEG,
                bytes: vec![0xFF, 0xD8, 0xFF, 0x00],
            }],
        }
    }

    #[test]
    fn playlist_document_round_trips_with_covers() {
        let doc = sample_playlist();
        let bytes = doc.encode();
        assert_eq!(&bytes[0..8], MAGIC_PL);
        assert_eq!(container_file_kind(&bytes), Some(*FILE_KIND_PLAYLIST));
        let decoded = PlaylistDocument::decode(&bytes).unwrap();
        assert_eq!(decoded, doc);
        assert_eq!(decoded.covers[0].kind, crate::cover::COVER_KIND_JPEG);
    }

    #[test]
    fn playlist_document_injects_cover_kind_when_missing() {
        let mut doc = sample_playlist();
        doc.covers[0].kind = crate::cover::COVER_KIND_NONE;
        let decoded = PlaylistDocument::decode(&doc.encode()).unwrap();
        assert_eq!(decoded.covers[0].kind, crate::cover::COVER_KIND_JPEG);
    }

    #[test]
    fn playlist_document_requires_an_id_and_playlist_kind() {
        let mut doc = sample_playlist();
        doc.id.clear();
        assert!(PlaylistDocument::decode(&doc.encode()).is_err());

        // A deletion pack must not decode as a playlist document.
        let pack = PlaylistDeletionDocument {
            playlist_id: "pl-1".into(),
            device_id: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            records: vec![PlaylistDeletionRecordDoc {
                entry_identity: String::new(),
                deleted_at: "2026-03-01T00:00:00.000Z".into(),
            }],
        };
        let pack_bytes = pack.encode();
        assert!(PlaylistDocument::decode(&pack_bytes).is_err());
        assert!(VaultDocument::decode(&pack_bytes).is_err());
    }

    #[test]
    fn playlist_deletion_pack_round_trips_and_sorts_records() {
        let doc = PlaylistDeletionDocument {
            playlist_id: "pl-9".into(),
            device_id: "phone".into(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: String::new(),
            records: vec![
                PlaylistDeletionRecordDoc {
                    entry_identity: "music-b".into(),
                    deleted_at: "2026-02-01T00:00:00.000Z".into(),
                },
                PlaylistDeletionRecordDoc {
                    entry_identity: String::new(),
                    deleted_at: "2026-03-01T00:00:00.000Z".into(),
                },
            ],
        };
        let bytes = doc.encode();
        assert_eq!(&bytes[0..8], MAGIC_PD);
        let decoded = PlaylistDeletionDocument::decode(&bytes).unwrap();
        assert_eq!(decoded.playlist_id, "pl-9");
        assert_eq!(decoded.records.len(), 2);
        // Sorted identity-first: whole-playlist (empty identity) then "music-b".
        assert_eq!(decoded.records[0].entry_identity, "");
        assert_eq!(decoded.records[1].entry_identity, "music-b");
        // updated_at is the newest deletion timestamp.
        assert_eq!(
            epoch_from_iso8601(&decoded.updated_at),
            Some(epoch_from_iso8601("2026-03-01T00:00:00.000Z").unwrap())
        );
    }

    #[test]
    fn playlist_deletion_records_without_timestamps_are_dropped() {
        let mut meta = BTreeMap::new();
        push_int(&mut meta, META_KIND, KIND_PLAYLIST_DELETION);
        push_str(&mut meta, META_PLAYLIST_ID, "pl-1");
        let mut good = BTreeMap::new();
        push_int(&mut good, DELETION_TAG_SCOPE, DELETION_SCOPE_PLAYLIST);
        push_str(&mut good, DELETION_TAG_DELETED_AT, "2026-01-01T00:00:00Z");
        let mut bad = BTreeMap::new();
        push_int(&mut bad, DELETION_TAG_SCOPE, DELETION_SCOPE_ENTRY);
        push_str(&mut bad, DELETION_TAG_MUSIC_ID, "music-x");
        push_str(&mut bad, DELETION_TAG_DELETED_AT, "not-a-date");
        let mut missing_id = BTreeMap::new();
        push_int(&mut missing_id, DELETION_TAG_SCOPE, DELETION_SCOPE_ENTRY);
        push_str(&mut missing_id, DELETION_TAG_DELETED_AT, "2026-01-01T00:00:00Z");
        let bytes = build_wmp_container(
            &[
                (
                    SEC_META,
                    encode_records(std::slice::from_ref(&meta), META_INT_TAGS),
                    Codec::Deflate,
                ),
                (
                    SEC_PLAYLIST_DELETIONS,
                    encode_records(
                        &[good, bad, missing_id],
                        DELETION_RECORD_INT_TAGS,
                    ),
                    Codec::Deflate,
                ),
            ],
            MAGIC_PD,
        );
        let decoded = PlaylistDeletionDocument::decode(&bytes).unwrap();
        assert_eq!(decoded.records.len(), 1);
    }

    #[test]
    fn playlist_file_names_are_sanitized() {
        assert_eq!(
            safe_playlist_file_name("最爱/歌单:2026", "abcdef123456"),
            "最爱_歌单_2026_abcdef12.wdmp"
        );
        assert_eq!(safe_playlist_file_name("  ", "short"), "playlist_short.wdmp");
        assert_eq!(safe_playlist_file_name("a b", "abcdefghij"), "a_b_abcdefgh.wdmp");
        assert_eq!(engine_playlist_file_name("pl-1"), "pl-1.wdmp");
        assert_eq!(playlist_deletion_file_name("pl-1"), "pdel_pl-1.wdmp");
        assert_eq!(playlist_deletion_file_name("a/b c"), "pdel_a_b_c.wdmp");
        assert!(looks_like_playlist_deletion("PDEL_x.wdmp"));
        assert!(!looks_like_playlist_document("pdel_x.wdmp"));
        assert!(looks_like_playlist_document("Mix_ab12.wdmp"));
        assert!(!looks_like_playlist_document("mix.json"));
    }

    #[test]
    fn timestamps_round_trip() {
        let iso = "2026-01-02T03:04:05.678Z";
        let seconds = epoch_from_iso8601(iso).unwrap();
        assert_eq!(epoch_from_iso8601(&iso8601_from_epoch(seconds)), Some(seconds));
        assert_eq!(epoch_from_iso8601("nope"), None);
    }

    /// The backup container test in Dart (`test/backup_container_test.dart`)
    /// embeds these exact bytes instead of building them with a Dart codec.
    /// `flutter test` has no `libwdmm_ffi`, so the fixture is pinned here too:
    /// both sides use the same constant and this test proves Rust reads it as
    /// the expected document.
    ///
    /// Note: only the *decoded* shape is pinned, not the encoder output. The
    /// deflate bytes of a re-encode depend on the flate2 backend selected by
    /// feature unification (miniz_oxide building `wdmm-sync` alone, zlib-rs once
    /// `zip`/`lofty` join the build), so byte equality would be flaky; decoding
    /// is backend-independent.
    const BACKUP_VAULT_FIXTURE: &str = "V0RNTUNWMDECAAAAAQEwAAAAJgAAACgAAABQKdelBQFWAAAATgAAAEkAAAAX3Zg+Y2RnYWRlTUkt0zVkkzAyMDLTNbDUNTIIMTCwAiM9AwODKB4mBgABSQC2/wEFYWNjLTECBjEyM3BhbgMGd2ViZGF2BBdodHRwczovL25hcy5leGFtcGxlLmNvbQUFYWxpY2UGCkFFU0dDTXYxOngHAQgBLwA=";
    const BACKUP_PLAYLIST_FIXTURE: &str = "V0RNTVBMMDECAAAAAQEwAAAASAAAAE4AAADy9FHuCQF4AAAANwAAADIAAAArdlVLdcqxDYAgEAVQSeASdQGXOPO5wkTnsLLWzoKKmpIhWIKlYA0SepJXPkVaGfN+ni1tAjkYJwtu4Op2AM+s3c92oZJCjXkdvqkBATIAzf8BEDEyM3BhbjovbS9hLmZsYWMCBjEyM3BhbgMJL20vYS5mbGFjBAZhLmZsYWMF2I8NAA==";

    #[test]
    fn backup_fixtures_are_readable_documents() {
        use base64::Engine;
        let decode = |text: &str| base64::engine::general_purpose::STANDARD.decode(text).unwrap();

        let vault_bytes = decode(BACKUP_VAULT_FIXTURE);
        assert_eq!(container_file_kind(&vault_bytes), Some(*FILE_KIND_VAULT));
        let vault = VaultDocument::decode(&vault_bytes).unwrap();
        assert_eq!(vault.format_version, 2);
        assert_eq!(vault.device_id, "dev-1");
        assert_eq!(vault.records.len(), 1);
        assert_eq!(vault.records[0].name, "123pan");
        assert_eq!(vault.records[0].password, "AESGCMv1:x");
        assert!(vault.records[0].password_encrypted);
        // Semantic round-trip: encode→decode keeps every field, whatever bytes
        // the deflate backend produces.
        assert_eq!(VaultDocument::decode(&vault.encode()).unwrap(), vault);

        let playlist_bytes = decode(BACKUP_PLAYLIST_FIXTURE);
        assert_eq!(container_file_kind(&playlist_bytes), Some(*FILE_KIND_PLAYLIST));
        let playlist = PlaylistDocument::decode(&playlist_bytes).unwrap();
        assert_eq!(playlist.id, "pl-1");
        assert_eq!(playlist.name, "最爱");
        assert_eq!(playlist.entries.len(), 1);
        assert_eq!(playlist.entries[0].remote_path, "/m/a.flac");
        assert_eq!(playlist.entries[0].duration_ms, Some(215000));
        assert_eq!(PlaylistDocument::decode(&playlist.encode()).unwrap(), playlist);
    }
}
