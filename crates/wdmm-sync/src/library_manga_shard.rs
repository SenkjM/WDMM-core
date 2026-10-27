//! Manga library shard codec (mirrors music `library_shard.rs`).
//!
//! Uses the same WMP container magics and META section as music.  The data
//! section is `SEC_MANGAS` (12) instead of `SEC_TRACKS` (2).
//!
//! Base/seg shards: `seg-*` shards never carry covers.  Full-rebuild base
//! shards (`lib-*`) may embed `SEC_COVERS` (3) + `TAG_MANGA_COVER_INDEX` (19)
//! when `cover_roots` is `Some`.  `SEC_MANGA_COVERS` (15) is never used for
//! shards.  Tomb shards never carry covers.  When no [`CoverRoots`] is
//! supplied, or the kind is not `WMP_KIND_BASE`, the wire format is unchanged.

use crate::cover::{build_cover_section, manga_cover_stem, CoverRoots};
use crate::library_shard::{
    decode_meta, encode_meta, magic_for_kind, meta_kind_of_container,
    ShardMeta, TombRecord, WMP_KIND_BASE, WMP_KIND_SEG, WMP_KIND_TOMB,
    META_INT_TAGS, TOMB_INT_TAGS,
    TAG_TOMB_SOURCE, TAG_TOMB_REMOTE_PATH, TAG_TOMB_REV,
    TAG_TOMB_DELETED_AT, TAG_TOMB_SYNC_KEY,
};
use crate::wmp::{self, Codec, RecordValue, MAGIC_LT, SEC_COVERS, SEC_META, SEC_MANGAS, SEC_TOMBS};
use crate::{SyncError, SyncResult};
use std::collections::BTreeMap;
use wdmm_storage::{manga_id_for_remote, normalize_manga_remote_path, Manga};

// ── WmpManga tag IDs ──────────────────────────────────────────────────────────

pub const TAG_MANGA_SOURCE_NAME: u8 = 1;
pub const TAG_MANGA_REMOTE_PATH: u8 = 2;
pub const TAG_MANGA_REV: u8 = 3;
pub const TAG_MANGA_TITLE: u8 = 4;
pub const TAG_MANGA_AUTHOR: u8 = 5;
pub const TAG_MANGA_SERIES: u8 = 7;
pub const TAG_MANGA_VOLUME: u8 = 10;
pub const TAG_MANGA_CHAPTER_NUMBER: u8 = 11;
pub const TAG_MANGA_PAGE_COUNT: u8 = 12;
pub const TAG_MANGA_COVER_RELPATH: u8 = 13;
pub const TAG_MANGA_FORMAT: u8 = 14;
pub const TAG_MANGA_REMOTE_MISSING: u8 = 15;
pub const TAG_MANGA_PAGE_TURN: u8 = 16;
pub const TAG_MANGA_UPDATED_AT: u8 = 17;
pub const TAG_MANGA_PAGES: u8 = 18; // JSON array; only when format=="images"
pub const TAG_MANGA_COVER_INDEX: u8 = 19; // emitted when blob present (parity with Android)
pub const TAG_MANGA_ID: u8 = 20;

/// Integer tags for WmpManga (Android `kMangaIntTags`).
pub const MANGA_INT_TAGS: &[u8] = &[3, 12, 15, 19];

// ── Public types ──────────────────────────────────────────────────────────────

/// A manga row bundled with its page list.
///
/// Pages are only meaningful when `manga.format == "images"`; for container
/// formats (cbz/pdf/…) the list is always empty.
#[derive(Debug, Clone)]
pub struct MangaSyncRow {
    pub manga: Manga,
    pub pages: Vec<String>,
}

/// Result of decoding a manga shard.
#[derive(Debug, Clone)]
pub struct DecodedMangaShard {
    pub meta: ShardMeta,
    pub rows: Vec<MangaSyncRow>,
    pub tombstones: Vec<TombRecord>,
    /// Covers parsed from SEC_COVERS (3): `(kind, blob_bytes)` indexed by cover index.
    pub covers: Vec<(u8, Vec<u8>)>,
    /// Cover index per row (parallel to `rows`): `None` when the row has no cover.
    pub cover_indices: Vec<Option<u32>>,
}

// ── Internal encode helpers ───────────────────────────────────────────────────

fn manga_to_record(row: &MangaSyncRow) -> BTreeMap<u8, RecordValue> {
    let m = &row.manga;
    let mut rec = BTreeMap::new();
    rec.insert(TAG_MANGA_SOURCE_NAME, RecordValue::Str(m.source_name.clone()));
    rec.insert(
        TAG_MANGA_REMOTE_PATH,
        RecordValue::Str(normalize_manga_remote_path(&m.remote_path)),
    );
    rec.insert(TAG_MANGA_REV, RecordValue::Int(m.rev as u64));
    rec.insert(TAG_MANGA_TITLE, RecordValue::Str(m.title.clone()));
    if let Some(ref s) = m.author {
        rec.insert(TAG_MANGA_AUTHOR, RecordValue::Str(s.clone()));
    }
    if let Some(ref s) = m.series {
        rec.insert(TAG_MANGA_SERIES, RecordValue::Str(s.clone()));
    }
    if let Some(ref s) = m.volume {
        rec.insert(TAG_MANGA_VOLUME, RecordValue::Str(s.clone()));
    }
    if let Some(ref s) = m.chapter_number {
        rec.insert(TAG_MANGA_CHAPTER_NUMBER, RecordValue::Str(s.clone()));
    }
    rec.insert(TAG_MANGA_PAGE_COUNT, RecordValue::Int(m.page_count as u64));
    if let Some(ref s) = m.cover_relpath {
        rec.insert(TAG_MANGA_COVER_RELPATH, RecordValue::Str(s.clone()));
    }
    rec.insert(TAG_MANGA_FORMAT, RecordValue::Str(m.format.clone()));
    rec.insert(TAG_MANGA_REMOTE_MISSING, RecordValue::Int(m.remote_missing as u64));
    if let Some(ref s) = m.page_turn {
        rec.insert(TAG_MANGA_PAGE_TURN, RecordValue::Str(s.clone()));
    }
    rec.insert(TAG_MANGA_UPDATED_AT, RecordValue::Str(m.updated_at.clone()));
    // Pages JSON: only for images format with a non-empty list
    if m.format == "images" && !row.pages.is_empty() {
        let json = serde_json::to_string(&row.pages).unwrap_or_else(|_| "[]".to_string());
        rec.insert(TAG_MANGA_PAGES, RecordValue::Str(json));
    }
    // TAG_MANGA_COVER_INDEX (19) is injected by `encode_manga_shard` when a
    // cover blob is present (see below); `manga_to_record` never emits it.
    rec.insert(TAG_MANGA_ID, RecordValue::Str(m.id.clone()));
    rec
}

fn tomb_record_to_map(t: &TombRecord) -> BTreeMap<u8, RecordValue> {
    let mut rec = BTreeMap::new();
    rec.insert(TAG_TOMB_SOURCE, RecordValue::Str(t.source_name.clone()));
    rec.insert(TAG_TOMB_REMOTE_PATH, RecordValue::Str(t.remote_path.clone()));
    rec.insert(TAG_TOMB_REV, RecordValue::Int(t.rev as u64));
    rec.insert(TAG_TOMB_DELETED_AT, RecordValue::Str(t.deleted_at.clone()));
    if let Some(ref sk) = t.sync_key {
        rec.insert(TAG_TOMB_SYNC_KEY, RecordValue::Str(sk.clone()));
    }
    rec
}

// ── Internal decode helpers ───────────────────────────────────────────────────

fn record_to_manga_row(rec: &BTreeMap<u8, RecordValue>) -> MangaSyncRow {
    let source_name = rec
        .get(&TAG_MANGA_SOURCE_NAME)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let remote_path = rec
        .get(&TAG_MANGA_REMOTE_PATH)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = rec
        .get(&TAG_MANGA_ID)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| manga_id_for_remote(&source_name, &remote_path));
    let format = rec
        .get(&TAG_MANGA_FORMAT)
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let pages: Vec<String> = if format == "images" {
        rec.get(&TAG_MANGA_PAGES)
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    } else {
        vec![]
    };
    let manga = Manga {
        id,
        source_name,
        remote_path,
        title: rec
            .get(&TAG_MANGA_TITLE)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        author: rec.get(&TAG_MANGA_AUTHOR).and_then(|v| v.as_str()).map(|s| s.to_string()),
        series: rec.get(&TAG_MANGA_SERIES).and_then(|v| v.as_str()).map(|s| s.to_string()),
        volume: rec.get(&TAG_MANGA_VOLUME).and_then(|v| v.as_str()).map(|s| s.to_string()),
        chapter_number: rec
            .get(&TAG_MANGA_CHAPTER_NUMBER)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        page_count: rec
            .get(&TAG_MANGA_PAGE_COUNT)
            .and_then(|v| v.as_int())
            .unwrap_or(0),
        cover_relpath: rec
            .get(&TAG_MANGA_COVER_RELPATH)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        format,
        remote_missing: rec
            .get(&TAG_MANGA_REMOTE_MISSING)
            .and_then(|v| v.as_int())
            .unwrap_or(0),
        page_turn: rec
            .get(&TAG_MANGA_PAGE_TURN)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        rev: rec.get(&TAG_MANGA_REV).and_then(|v| v.as_int()).unwrap_or(0),
        updated_at: rec
            .get(&TAG_MANGA_UPDATED_AT)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    };
    MangaSyncRow { manga, pages }
}

fn map_to_tomb_record(rec: &BTreeMap<u8, RecordValue>) -> TombRecord {
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

// ── Public API ────────────────────────────────────────────────────────────────

/// Encode a base or segment manga shard (META + MANGAS + optional SEC_COVERS).
///
/// When `cover_roots` is `Some` and the manga covers dir exists, each row
/// whose cover file is present on disk gets `TAG_MANGA_COVER_INDEX` (19) and a
/// `SEC_COVERS` (3) section carrying the deduplicated blobs is appended
/// (NOT `SEC_MANGA_COVERS` 15).  Missing cover files omit the tag (indices
/// count only present blobs).  When `cover_roots` is `None` or no files are
/// readable, no section 3 is emitted — parity with the pre-cover wire format,
/// not a product flag.  `TAG_MANGA_COVER_RELPATH` (13) is always kept as today.
pub fn encode_manga_shard(
    kind: i64,
    device_id: &str,
    rev_from: i64,
    rev_to: i64,
    rows: &[MangaSyncRow],
    cover_roots: Option<&CoverRoots>,
) -> SyncResult<Vec<u8>> {
    if kind != WMP_KIND_BASE && kind != WMP_KIND_SEG {
        return Err(SyncError::Other("encode_manga_shard: kind must be base/seg".into()));
    }
    let meta = ShardMeta {
        kind,
        seq_from: rev_from,
        seq_to: rev_to,
        count: rows.len() as i64,
        device_id: device_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let meta_bytes = encode_meta(&meta);

    // Build COVERS section + per-row indices — only for base shards.
    let (cover_bytes, cover_indices) = match cover_roots {
        Some(roots) if kind == WMP_KIND_BASE => {
            let stems: Vec<String> = rows.iter().map(|r| manga_cover_stem(&r.manga.id)).collect();
            build_cover_section(&roots.manga_covers_dir, &stems)
        }
        _ => (vec![], vec![None; rows.len()]),
    };

    let mut manga_recs: Vec<BTreeMap<u8, RecordValue>> =
        rows.iter().map(manga_to_record).collect();
    for (rec, idx_opt) in manga_recs.iter_mut().zip(cover_indices.iter()) {
        if let Some(idx) = idx_opt {
            rec.insert(TAG_MANGA_COVER_INDEX, RecordValue::Int(*idx as u64));
        }
    }
    let mangas_bytes = wmp::encode_records(&manga_recs, MANGA_INT_TAGS);

    let mut sections: Vec<(u8, Vec<u8>, Codec)> =
        vec![(SEC_META, meta_bytes, Codec::Raw), (SEC_MANGAS, mangas_bytes, Codec::Raw)];
    if !cover_bytes.is_empty() {
        sections.push((SEC_COVERS, cover_bytes, Codec::Raw));
    }
    Ok(wmp::build_wmp_container(&sections, magic_for_kind(kind)?))
}

/// Encode a manga tombstone shard.
pub fn encode_manga_tomb_shard(
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
    let tomb_recs: Vec<_> = tombs.iter().map(|t| tomb_record_to_map(t)).collect();
    let tombs_bytes = wmp::encode_records(&tomb_recs, TOMB_INT_TAGS);
    let sections = [
        (SEC_META, meta_bytes, Codec::Raw),
        (SEC_TOMBS, tombs_bytes, Codec::Raw),
    ];
    Ok(wmp::build_wmp_container(&sections, MAGIC_LT))
}

/// Decode any manga shard (base, segment, or tombstone).  Parses
/// `SEC_COVERS` (3) and `TAG_MANGA_COVER_INDEX` (19) so callers can apply
/// cover blobs via [`apply_manga_cover_blobs`].
pub fn decode_manga_shard(bytes: &[u8]) -> SyncResult<DecodedMangaShard> {
    let sections = wmp::parse_wmp_container(bytes, None)?;
    let meta_raw = sections
        .get(&SEC_META)
        .ok_or_else(|| SyncError::Other("manga shard missing META".into()))?;
    let meta = decode_meta(meta_raw)?;
    let magic_kind = meta_kind_of_container(bytes);
    if magic_kind >= 0 && magic_kind != meta.kind {
        return Err(SyncError::Other(format!(
            "err.shardKindMismatch|{magic_kind}|{}",
            meta.kind
        )));
    }

    if meta.kind == WMP_KIND_TOMB {
        let raw = sections.get(&SEC_TOMBS).map(|v| v.as_slice()).unwrap_or(&[]);
        let records = if raw.is_empty() { vec![] } else { wmp::decode_records(raw, TOMB_INT_TAGS)? };
        return Ok(DecodedMangaShard {
            meta,
            rows: vec![],
            tombstones: records.iter().map(map_to_tomb_record).collect(),
            covers: vec![],
            cover_indices: vec![],
        });
    }

    let raw = sections.get(&SEC_MANGAS).map(|v| v.as_slice()).unwrap_or(&[]);
    let records = if raw.is_empty() { vec![] } else { wmp::decode_records(raw, MANGA_INT_TAGS)? };

    // Parse optional COVERS section.
    let covers = sections
        .get(&SEC_COVERS)
        .map(|v| crate::cover::parse_cover_section(v))
        .unwrap_or_default();

    // Extract per-row cover index from TAG_MANGA_COVER_INDEX (19).
    let cover_indices: Vec<Option<u32>> = records
        .iter()
        .map(|rec| {
            rec.get(&TAG_MANGA_COVER_INDEX)
                .and_then(|v| v.as_int())
                .map(|i| i as u32)
        })
        .collect();

    Ok(DecodedMangaShard {
        meta,
        rows: records.iter().map(record_to_manga_row).collect(),
        tombstones: vec![],
        covers,
        cover_indices,
    })
}

/// Write cover blobs for manga rows into `roots.manga_covers_dir`.
///
/// Parity with Android `_storeCover`: for each row whose decoded
/// `cover_indices` entry points at a blob, write
/// `<manga_covers_dir>/<manga_cover_stem(manga.id)>` (the stem already ends
/// with `.jpg`).  A file already present is kept — a missing blob must never
/// wipe the local cache.  Missing cover dir, missing blob, or out-of-range
/// index are silently skipped.
pub fn apply_manga_cover_blobs(
    roots: &CoverRoots,
    rows: &[MangaSyncRow],
    cover_indices: &[Option<u32>],
    covers: &[(u8, Vec<u8>)],
) {
    if rows.len() != cover_indices.len() {
        return;
    }
    let dir = &roots.manga_covers_dir;
    let _ = std::fs::create_dir_all(dir);
    for (row, idx_opt) in rows.iter().zip(cover_indices.iter()) {
        let Some(idx) = *idx_opt else { continue };
        let Some((_, blob)) = covers.get(idx as usize) else { continue };
        if blob.is_empty() {
            continue;
        }
        let stem = manga_cover_stem(&row.manga.id);
        let path = dir.join(&stem);
        if !path.exists() {
            let _ = std::fs::write(&path, blob);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wdmm_storage::Manga;

    fn sample_manga(rev: i64, fmt: &str) -> Manga {
        Manga {
            id: format!("m-{rev}"),
            source_name: "nas".into(),
            remote_path: format!("/manga/vol{rev}.cbz"),
            title: format!("Volume {rev}"),
            author: Some("Auth".into()),
            series: Some("Series A".into()),
            volume: Some(format!("{rev}")),
            chapter_number: Some(format!("{rev}")),
            page_count: 12,
            cover_relpath: None,
            format: fmt.into(),
            remote_missing: 0,
            page_turn: Some("ltr".into()),
            rev,
            updated_at: "2026-10-26T00:00:00Z".into(),
        }
    }

    #[test]
    fn manga_shard_roundtrip_cbz() {
        let rows = vec![MangaSyncRow {
            manga: sample_manga(1, "cbz"),
            pages: vec![],
        }];
        let bytes = encode_manga_shard(WMP_KIND_SEG, "dev", 1, 1, &rows, None).unwrap();
        assert!(bytes.starts_with(b"WDMMLS01"));
        let decoded = decode_manga_shard(&bytes).unwrap();
        assert_eq!(decoded.meta.kind, WMP_KIND_SEG);
        assert_eq!(decoded.rows.len(), 1);
        let m = &decoded.rows[0].manga;
        assert_eq!(m.title, "Volume 1");
        assert_eq!(m.source_name, "nas");
        assert_eq!(m.rev, 1);
        assert_eq!(m.format, "cbz");
        assert_eq!(m.author.as_deref(), Some("Auth"));
        assert_eq!(m.series.as_deref(), Some("Series A"));
        assert_eq!(m.page_count, 12);
        assert_eq!(m.id, "m-1");
        assert!(decoded.rows[0].pages.is_empty());
        assert!(decoded.tombstones.is_empty());
        assert!(decoded.covers.is_empty());
        assert_eq!(decoded.cover_indices, vec![None]);
    }

    #[test]
    fn manga_shard_roundtrip_images_with_pages() {
        let mut manga = sample_manga(2, "images");
        manga.remote_path = "/manga/scans".into();
        let pages = vec!["p001.jpg".to_string(), "p002.jpg".to_string()];
        let rows = vec![MangaSyncRow { manga, pages: pages.clone() }];
        let bytes = encode_manga_shard(WMP_KIND_BASE, "dev", 0, 2, &rows, None).unwrap();
        assert!(bytes.starts_with(b"WDMMLB01"));
        let decoded = decode_manga_shard(&bytes).unwrap();
        assert_eq!(decoded.meta.kind, WMP_KIND_BASE);
        assert_eq!(decoded.rows.len(), 1);
        assert_eq!(decoded.rows[0].manga.format, "images");
        assert_eq!(decoded.rows[0].pages, pages);
    }

    #[test]
    fn manga_tomb_shard_roundtrip_with_sync_key() {
        let tombs = vec![TombRecord {
            source_name: "nas".into(),
            remote_path: "/manga/gone.cbz".into(),
            rev: 5,
            deleted_at: "2026-10-26T00:00:00Z".into(),
            sync_key: Some("nas\x00/manga/gone.cbz".into()),
        }];
        let bytes = encode_manga_tomb_shard("dev", 5, 5, &tombs).unwrap();
        assert!(bytes.starts_with(b"WDMMLT01"));
        let decoded = decode_manga_shard(&bytes).unwrap();
        assert_eq!(decoded.meta.kind, WMP_KIND_TOMB);
        assert!(decoded.rows.is_empty());
        assert_eq!(decoded.tombstones.len(), 1);
        assert_eq!(decoded.tombstones[0].rev, 5);
        assert_eq!(decoded.tombstones[0].sync_key.as_deref(), Some("nas\x00/manga/gone.cbz"));
    }

    // ── Cover tests ──────────────────────────────────────────────────────────

    fn manga_cover_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wdmm_manga_cover_{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cover_jpeg() -> Vec<u8> {
        let mut v = vec![0xFF_u8, 0xD8, 0xFF, 0xE0];
        v.extend_from_slice(b"manga_cover_data");
        v
    }

    #[test]
    fn manga_shard_covers_roundtrip() {
        let cover_dir = manga_cover_dir("roundtrip");
        let mut manga = sample_manga(1, "cbz");
        manga.id = "manga-cover-1".into();
        let rows = vec![MangaSyncRow { manga, pages: vec![] }];

        let stem = manga_cover_stem(&rows[0].manga.id);
        std::fs::write(cover_dir.join(&stem), cover_jpeg()).unwrap();

        let roots = CoverRoots {
            music_covers_dir: std::env::temp_dir(),
            manga_covers_dir: cover_dir.clone(),
        };

        let bytes =
            encode_manga_shard(WMP_KIND_BASE, "dev", 0, 1, &rows, Some(&roots)).unwrap();
        // Section 3 (NOT 15) must be present.
        let sections = wmp::parse_wmp_container(&bytes, None).unwrap();
        assert!(sections.contains_key(&SEC_COVERS), "SEC_COVERS(3) must be present");
        assert!(
            !sections.contains_key(&crate::wmp::SEC_MANGA_COVERS),
            "SEC_MANGA_COVERS(15) must NOT be used for shards"
        );

        let decoded = decode_manga_shard(&bytes).unwrap();
        assert_eq!(decoded.rows.len(), 1);
        assert_eq!(decoded.cover_indices.len(), 1);
        assert_eq!(decoded.cover_indices[0], Some(0), "row must have coverIndex 0");
        assert_eq!(decoded.covers.len(), 1);
        assert_eq!(decoded.covers[0].0, crate::cover::COVER_KIND_JPEG);
        assert_eq!(decoded.covers[0].1, cover_jpeg());

        // Apply into a fresh dir.
        let apply_dir = manga_cover_dir("roundtrip_apply");
        let apply_roots = CoverRoots {
            music_covers_dir: std::env::temp_dir(),
            manga_covers_dir: apply_dir.clone(),
        };
        apply_manga_cover_blobs(&apply_roots, &decoded.rows, &decoded.cover_indices, &decoded.covers);
        let written = std::fs::read(apply_dir.join(&stem)).expect("cover must be written");
        assert_eq!(written, cover_jpeg());
    }

    #[test]
    fn seg_encode_never_has_covers_even_with_roots() {
        let cover_dir = manga_cover_dir("seg");
        let mut manga = sample_manga(1, "cbz");
        manga.id = "manga-seg-cover".into();
        let rows = vec![MangaSyncRow { manga, pages: vec![] }];

        // Pre-write a cover file so encode *could* pick it up if it ignored the kind.
        let stem = manga_cover_stem(&rows[0].manga.id);
        std::fs::write(cover_dir.join(&stem), cover_jpeg()).unwrap();

        let roots = CoverRoots {
            music_covers_dir: std::env::temp_dir(),
            manga_covers_dir: cover_dir,
        };
        let bytes =
            encode_manga_shard(WMP_KIND_SEG, "dev", 1, 1, &rows, Some(&roots)).unwrap();
        let sections = wmp::parse_wmp_container(&bytes, None).unwrap();
        assert!(
            !sections.contains_key(&SEC_COVERS),
            "seg shards must never carry SEC_COVERS"
        );
        let decoded = decode_manga_shard(&bytes).unwrap();
        assert!(decoded.covers.is_empty());
        assert_eq!(decoded.cover_indices, vec![None]);
    }
}
