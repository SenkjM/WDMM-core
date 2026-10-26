//! Music library shard codec (Android `LibraryShardCodec` + WMP containers).
//!
//! Magics: `WDMMLB01` (base), `WDMMLS01` (seg), `WDMMLT01` (tomb).
//! Covers are omitted on encode and ignored on decode (Core v1).

use crate::wmp::{
    self, Codec, RecordValue, MAGIC_LB, MAGIC_LS, MAGIC_LT, SEC_META, SEC_TOMBS, SEC_TRACKS,
};
use crate::{SyncError, SyncResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use wdmm_core::models::LibraryTrack;

pub const WMP_KIND_BASE: i64 = 0;
pub const WMP_KIND_SEG: i64 = 1;
pub const WMP_KIND_TOMB: i64 = 2;

pub const TAG_META_KIND: u8 = 1;
pub const TAG_META_SEQ_FROM: u8 = 2;
pub const TAG_META_SEQ_TO: u8 = 3;
pub const TAG_META_COUNT: u8 = 4;
pub const TAG_META_DEVICE_ID: u8 = 5;
pub const TAG_META_CREATED_AT: u8 = 6;

pub const TAG_TRACK_SOURCE: u8 = 1;
pub const TAG_TRACK_REMOTE_PATH: u8 = 2;
pub const TAG_TRACK_FILENAME: u8 = 3;
pub const TAG_TRACK_REV: u8 = 4;
pub const TAG_TRACK_TITLE: u8 = 5;
pub const TAG_TRACK_ARTIST: u8 = 6;
pub const TAG_TRACK_ALBUM_ARTIST: u8 = 7;
pub const TAG_TRACK_ALBUM: u8 = 8;
pub const TAG_TRACK_DURATION_MS: u8 = 9;
pub const TAG_TRACK_TRACK_NUM: u8 = 10;
pub const TAG_TRACK_DISC_NUM: u8 = 12;
pub const TAG_TRACK_YEAR: u8 = 14;
pub const TAG_TRACK_GENRE: u8 = 15;

pub const TAG_TOMB_SOURCE: u8 = 1;
pub const TAG_TOMB_REMOTE_PATH: u8 = 2;
pub const TAG_TOMB_REV: u8 = 3;
pub const TAG_TOMB_DELETED_AT: u8 = 4;
/// Android `WmpTomb.syncKey` — manga uses this; music omits it (None → tag absent).
pub const TAG_TOMB_SYNC_KEY: u8 = 5;

/// Android `kMetaIntTags`.
pub const META_INT_TAGS: &[u8] = &[1, 2, 3, 4];
/// Android `kTrackIntTags` (subset we emit; full set for decode).
pub const TRACK_INT_TAGS: &[u8] = &[4, 9, 10, 11, 12, 13, 14, 16, 17, 20, 23, 24, 26];
/// Android `kTombIntTags`.
pub const TOMB_INT_TAGS: &[u8] = &[3];

#[derive(Debug, Clone, Default)]
pub struct ShardMeta {
    pub kind: i64,
    pub seq_from: i64,
    pub seq_to: i64,
    pub count: i64,
    pub device_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TombRecord {
    pub source_name: String,
    pub remote_path: String,
    pub rev: i64,
    pub deleted_at: String,
    /// Android `WmpTomb.syncKey`.  `None` for music (tag 5 omitted on encode,
    /// absent on decode → None).  Manga sets this to the Android sync key
    /// string (manga id for `images` format, else `source\0path`).
    #[serde(default)]
    pub sync_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DecodedLibraryShard {
    pub meta: ShardMeta,
    pub tracks: Vec<LibraryTrack>,
    pub tombstones: Vec<TombRecord>,
}

pub(crate) fn magic_for_kind(kind: i64) -> SyncResult<&'static [u8; 8]> {
    match kind {
        WMP_KIND_BASE => Ok(MAGIC_LB),
        WMP_KIND_SEG => Ok(MAGIC_LS),
        WMP_KIND_TOMB => Ok(MAGIC_LT),
        _ => Err(SyncError::Other(format!("unknown shard kind {kind}"))),
    }
}

pub(crate) fn expected_kind_bytes(kind: i64) -> SyncResult<&'static [u8; 2]> {
    match kind {
        WMP_KIND_BASE => Ok(b"LB"),
        WMP_KIND_SEG => Ok(b"LS"),
        WMP_KIND_TOMB => Ok(b"LT"),
        _ => Err(SyncError::Other(format!("unknown shard kind {kind}"))),
    }
}

pub(crate) fn meta_kind_of_container(bytes: &[u8]) -> i64 {
    if bytes.len() < 6 {
        return -1;
    }
    match &bytes[4..6] {
        b"LB" => WMP_KIND_BASE,
        b"LS" => WMP_KIND_SEG,
        b"LT" => WMP_KIND_TOMB,
        _ => -1,
    }
}

pub(crate) fn encode_meta(meta: &ShardMeta) -> Vec<u8> {
    let mut rec = BTreeMap::new();
    rec.insert(TAG_META_KIND, RecordValue::Int(meta.kind as u64));
    rec.insert(TAG_META_SEQ_FROM, RecordValue::Int(meta.seq_from as u64));
    rec.insert(TAG_META_SEQ_TO, RecordValue::Int(meta.seq_to as u64));
    rec.insert(TAG_META_COUNT, RecordValue::Int(meta.count as u64));
    rec.insert(TAG_META_DEVICE_ID, RecordValue::Str(meta.device_id.clone()));
    rec.insert(TAG_META_CREATED_AT, RecordValue::Str(meta.created_at.clone()));
    wmp::encode_records(&[rec], META_INT_TAGS)
}

pub(crate) fn decode_meta(raw: &[u8]) -> SyncResult<ShardMeta> {
    let records = wmp::decode_records(raw, META_INT_TAGS)?;
    let rec = records
        .first()
        .ok_or_else(|| SyncError::Other("empty shard META".into()))?;
    Ok(ShardMeta {
        kind: rec.get(&TAG_META_KIND).and_then(|v| v.as_int()).unwrap_or(WMP_KIND_SEG),
        seq_from: rec.get(&TAG_META_SEQ_FROM).and_then(|v| v.as_int()).unwrap_or(0),
        seq_to: rec.get(&TAG_META_SEQ_TO).and_then(|v| v.as_int()).unwrap_or(0),
        count: rec.get(&TAG_META_COUNT).and_then(|v| v.as_int()).unwrap_or(0),
        device_id: rec
            .get(&TAG_META_DEVICE_ID)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        created_at: rec
            .get(&TAG_META_CREATED_AT)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn track_to_record(t: &LibraryTrack) -> BTreeMap<u8, RecordValue> {
    let mut rec = BTreeMap::new();
    rec.insert(TAG_TRACK_SOURCE, RecordValue::Str(t.account_id.clone()));
    rec.insert(TAG_TRACK_REMOTE_PATH, RecordValue::Str(t.remote_path.clone()));
    let file_name = t.remote_path.rsplit('/').next().unwrap_or(&t.remote_path);
    rec.insert(TAG_TRACK_FILENAME, RecordValue::Str(file_name.to_string()));
    rec.insert(TAG_TRACK_REV, RecordValue::Int(t.rev as u64));
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
    if t.duration_ms != 0 {
        rec.insert(TAG_TRACK_DURATION_MS, RecordValue::Int(t.duration_ms as u64));
    }
    if let Some(n) = t.track_number {
        rec.insert(TAG_TRACK_TRACK_NUM, RecordValue::Int(n as u64));
    }
    if let Some(n) = t.disc_number {
        rec.insert(TAG_TRACK_DISC_NUM, RecordValue::Int(n as u64));
    }
    if let Some(n) = t.year {
        rec.insert(TAG_TRACK_YEAR, RecordValue::Int(n as u64));
    }
    if let Some(ref s) = t.genre {
        rec.insert(TAG_TRACK_GENRE, RecordValue::Str(s.clone()));
    }
    rec
}

fn record_to_track(rec: &BTreeMap<u8, RecordValue>) -> LibraryTrack {
    let remote_path = rec
        .get(&TAG_TRACK_REMOTE_PATH)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let account_id = rec
        .get(&TAG_TRACK_SOURCE)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let title = rec
        .get(&TAG_TRACK_TITLE)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            rec.get(&TAG_TRACK_FILENAME)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| {
            remote_path
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string()
        });
    LibraryTrack {
        id: wdmm_core::identity::track_identity(&remote_path),
        title,
        artist: rec.get(&TAG_TRACK_ARTIST).and_then(|v| v.as_str()).map(|s| s.to_string()),
        remote_path,
        account_id,
        size: 0,
        modified: None,
        album: rec.get(&TAG_TRACK_ALBUM).and_then(|v| v.as_str()).map(|s| s.to_string()),
        album_artist: rec
            .get(&TAG_TRACK_ALBUM_ARTIST)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        duration_ms: rec.get(&TAG_TRACK_DURATION_MS).and_then(|v| v.as_int()).unwrap_or(0),
        genre: rec.get(&TAG_TRACK_GENRE).and_then(|v| v.as_str()).map(|s| s.to_string()),
        track_number: rec
            .get(&TAG_TRACK_TRACK_NUM)
            .and_then(|v| v.as_int())
            .map(|n| n as i32),
        disc_number: rec
            .get(&TAG_TRACK_DISC_NUM)
            .and_then(|v| v.as_int())
            .map(|n| n as i32),
        year: rec.get(&TAG_TRACK_YEAR).and_then(|v| v.as_int()).map(|n| n as i32),
        mime_type: None,
        album_id: None,
        artist_id: None,
        rev: rec.get(&TAG_TRACK_REV).and_then(|v| v.as_int()).unwrap_or(0),
    }
}

fn tomb_to_record(t: &TombRecord) -> BTreeMap<u8, RecordValue> {
    let mut rec = BTreeMap::new();
    rec.insert(TAG_TOMB_SOURCE, RecordValue::Str(t.source_name.clone()));
    rec.insert(TAG_TOMB_REMOTE_PATH, RecordValue::Str(t.remote_path.clone()));
    rec.insert(TAG_TOMB_REV, RecordValue::Int(t.rev as u64));
    rec.insert(TAG_TOMB_DELETED_AT, RecordValue::Str(t.deleted_at.clone()));
    // TAG_TOMB_SYNC_KEY (5) is written only when present; music leaves it None.
    if let Some(ref sk) = t.sync_key {
        rec.insert(TAG_TOMB_SYNC_KEY, RecordValue::Str(sk.clone()));
    }
    rec
}

fn record_to_tomb(rec: &BTreeMap<u8, RecordValue>) -> TombRecord {
    TombRecord {
        source_name: rec
            .get(&TAG_TOMB_SOURCE)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        remote_path: rec
            .get(&TAG_TOMB_REMOTE_PATH)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        rev: rec.get(&TAG_TOMB_REV).and_then(|v| v.as_int()).unwrap_or(0),
        deleted_at: rec
            .get(&TAG_TOMB_DELETED_AT)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        sync_key: rec
            .get(&TAG_TOMB_SYNC_KEY)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

/// Encode a base or segment track shard (no covers).
pub fn encode_track_shard(
    kind: i64,
    device_id: &str,
    rev_from: i64,
    rev_to: i64,
    tracks: &[LibraryTrack],
) -> SyncResult<Vec<u8>> {
    if kind != WMP_KIND_BASE && kind != WMP_KIND_SEG {
        return Err(SyncError::Other("encode_track_shard: kind must be base/seg".into()));
    }
    let meta = ShardMeta {
        kind,
        seq_from: rev_from,
        seq_to: rev_to,
        count: tracks.len() as i64,
        device_id: device_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let meta_bytes = encode_meta(&meta);
    let track_recs: Vec<_> = tracks.iter().map(track_to_record).collect();
    let tracks_bytes = wmp::encode_records(&track_recs, TRACK_INT_TAGS);
    let sections = [
        (SEC_META, meta_bytes, Codec::Raw),
        (SEC_TRACKS, tracks_bytes, Codec::Raw),
    ];
    Ok(wmp::build_wmp_container(&sections, magic_for_kind(kind)?))
}

/// Encode a tombstone shard.
pub fn encode_tomb_shard(
    device_id: &str,
    rev_from: i64,
    rev_to: i64,
    tombs: &[TombRecord],
) -> SyncResult<Vec<u8>> {
    let meta = ShardMeta {
        kind: WMP_KIND_TOMB,
        seq_from: rev_from,
        seq_to: rev_to,
        count: tombs.len() as i64,
        device_id: device_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let meta_bytes = encode_meta(&meta);
    let tomb_recs: Vec<_> = tombs.iter().map(tomb_to_record).collect();
    let tombs_bytes = wmp::encode_records(&tomb_recs, TOMB_INT_TAGS);
    let sections = [
        (SEC_META, meta_bytes, Codec::Raw),
        (SEC_TOMBS, tombs_bytes, Codec::Raw),
    ];
    Ok(wmp::build_wmp_container(&sections, MAGIC_LT))
}

/// Decode any music library shard. Ignores COVERS.
pub fn decode_library_shard(bytes: &[u8]) -> SyncResult<DecodedLibraryShard> {
    let sections = wmp::parse_wmp_container(bytes, None)?;
    let meta_raw = sections
        .get(&SEC_META)
        .ok_or_else(|| SyncError::Other("shard missing META".into()))?;
    let meta = decode_meta(meta_raw)?;
    let magic_kind = meta_kind_of_container(bytes);
    if magic_kind >= 0 && magic_kind != meta.kind {
        return Err(SyncError::Other(format!(
            "err.shardKindMismatch|{magic_kind}|{}",
            meta.kind
        )));
    }
    // Soft-check expected kind bytes when magic maps.
    let _ = expected_kind_bytes(meta.kind);

    if meta.kind == WMP_KIND_TOMB {
        let raw = sections.get(&SEC_TOMBS).map(|v| v.as_slice()).unwrap_or(&[]);
        let records = if raw.is_empty() {
            vec![]
        } else {
            wmp::decode_records(raw, TOMB_INT_TAGS)?
        };
        return Ok(DecodedLibraryShard {
            meta,
            tracks: vec![],
            tombstones: records.iter().map(record_to_tomb).collect(),
        });
    }

    let raw = sections.get(&SEC_TRACKS).map(|v| v.as_slice()).unwrap_or(&[]);
    let records = if raw.is_empty() {
        vec![]
    } else {
        wmp::decode_records(raw, TRACK_INT_TAGS)?
    };
    Ok(DecodedLibraryShard {
        meta,
        tracks: records.iter().map(record_to_track).collect(),
        tombstones: vec![],
    })
}

/// Identity key: account_id|remote_path (Android sourceName+remotePath).
pub fn track_identity_key(account_id: &str, remote_path: &str) -> String {
    format!("{account_id}|{remote_path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track(rev: i64) -> LibraryTrack {
        LibraryTrack {
            id: wdmm_core::identity::track_identity("/music/a.mp3"),
            title: "A".into(),
            artist: Some("Art".into()),
            remote_path: "/music/a.mp3".into(),
            account_id: "nas".into(),
            size: 1,
            modified: None,
            rev,
            duration_ms: 1000,
            album: Some("Alb".into()),
            ..Default::default()
        }
    }

    #[test]
    fn track_shard_roundtrip_seg() {
        let tracks = vec![sample_track(42)];
        let bytes = encode_track_shard(WMP_KIND_SEG, "dev1", 42, 42, &tracks).unwrap();
        assert!(bytes.starts_with(b"WDMMLS01"));
        let decoded = decode_library_shard(&bytes).unwrap();
        assert_eq!(decoded.meta.kind, WMP_KIND_SEG);
        assert_eq!(decoded.tracks.len(), 1);
        assert_eq!(decoded.tracks[0].title, "A");
        assert_eq!(decoded.tracks[0].account_id, "nas");
        assert_eq!(decoded.tracks[0].rev, 42);
        assert_eq!(decoded.tracks[0].artist.as_deref(), Some("Art"));
    }

    #[test]
    fn track_shard_roundtrip_base() {
        let tracks = vec![sample_track(1), sample_track(2)];
        let bytes = encode_track_shard(WMP_KIND_BASE, "dev", 0, 0, &tracks).unwrap();
        assert!(bytes.starts_with(b"WDMMLB01"));
        let decoded = decode_library_shard(&bytes).unwrap();
        assert_eq!(decoded.meta.kind, WMP_KIND_BASE);
        assert_eq!(decoded.tracks.len(), 2);
    }

    #[test]
    fn tomb_shard_roundtrip() {
        let tombs = vec![TombRecord {
            source_name: "nas".into(),
            remote_path: "/music/gone.mp3".into(),
            rev: 99,
            deleted_at: "2026-10-26T00:00:00Z".into(),
            sync_key: None, // music leaves sync_key absent
        }];
        let bytes = encode_tomb_shard("dev", 99, 99, &tombs).unwrap();
        assert!(bytes.starts_with(b"WDMMLT01"));
        let decoded = decode_library_shard(&bytes).unwrap();
        assert_eq!(decoded.meta.kind, WMP_KIND_TOMB);
        assert_eq!(decoded.tombstones.len(), 1);
        assert_eq!(decoded.tombstones[0].rev, 99);
        assert_eq!(decoded.tombstones[0].remote_path, "/music/gone.mp3");
        // music tombs must round-trip with sync_key == None
        assert_eq!(decoded.tombstones[0].sync_key, None);
    }

    #[test]
    fn tomb_shard_roundtrip_with_sync_key() {
        let tombs = vec![TombRecord {
            source_name: "nas".into(),
            remote_path: "/manga/a.cbz".into(),
            rev: 7,
            deleted_at: "2026-10-26T00:00:00Z".into(),
            sync_key: Some("nas\x00/manga/a.cbz".into()),
        }];
        let bytes = encode_tomb_shard("dev", 7, 7, &tombs).unwrap();
        let decoded = decode_library_shard(&bytes).unwrap();
        assert_eq!(decoded.tombstones.len(), 1);
        assert_eq!(decoded.tombstones[0].sync_key.as_deref(), Some("nas\x00/manga/a.cbz"));
    }

    #[test]
    fn kind_mismatch_errors() {
        let tracks = vec![sample_track(1)];
        let mut bytes = encode_track_shard(WMP_KIND_SEG, "d", 1, 1, &tracks).unwrap();
        // Corrupt META kind to base while magic stays LS
        let sections = wmp::parse_wmp_container(&bytes, None).unwrap();
        let mut meta = decode_meta(sections.get(&SEC_META).unwrap()).unwrap();
        meta.kind = WMP_KIND_BASE;
        let meta_bytes = encode_meta(&meta);
        let tracks_bytes = sections.get(&SEC_TRACKS).unwrap().clone();
        bytes = wmp::build_wmp_container(
            &[(SEC_META, meta_bytes, Codec::Raw), (SEC_TRACKS, tracks_bytes, Codec::Raw)],
            MAGIC_LS,
        );
        let err = decode_library_shard(&bytes).unwrap_err();
        assert!(err.to_string().contains("shardKindMismatch"));
    }
}
