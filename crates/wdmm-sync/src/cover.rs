//! Cover file path helpers and backup section build/parse for COVERS / MANGA_COVERS.
//!
//! # Music cover stems
//!
//! [`music_cover_stem`] matches Android `MusicIdHelper.musicIdForRemote`:
//! ```text
//! sha1_hex( utf8( normalize_source_name(source) + '\0' + normalize_remote_path_for_cover(remote) ) )[0..16] + ".jpg"
//! ```
//! **Important:** [`normalize_remote_path_for_cover`] keeps the original case and the leading
//! `/`, unlike `wdmm_core::identity::normalize_remote_path` which lowercases and strips the
//! leading slash.  Do **not** substitute one for the other when computing cover stems.
//!
//! # Manga cover stems
//!
//! [`manga_cover_stem`] matches Android `CacheService.mangaCoverFile`:
//! `manga_id[0..min(16, len)] + ".jpg"`.
//!
//! # Section layout
//!
//! Both COVERS (SEC=3) and MANGA_COVERS (SEC=15) use codec Raw and carry a sequence of TLV
//! entries:
//! ```text
//! [u8  kind ]
//! [u32 le len]
//! [bytes × len]
//! ```
//! Defined kinds: `0` = none, `1` = WebP, `2` = JPEG, `3` = PNG — matching Android
//! `WmpImageKind`.  Entries are indexed 0..N-1; the `coverIndex` integer tag (TAG 26) in a
//! track record and the `cover_index` field in a manga JSON row are 0-based indices into that
//! sequence.

use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

// ── Public types ──────────────────────────────────────────────────────────────

/// Root directories for the music and manga cover file caches.
#[derive(Debug, Clone)]
pub struct CoverRoots {
    /// Directory holding `<stem>.jpg` music covers (`<app_docs>/covers/`).
    pub music_covers_dir: PathBuf,
    /// Directory holding `<stem>.jpg` manga covers (`<app_support>/manga_cache/covers/`).
    pub manga_covers_dir: PathBuf,
}

// ── Kind constants ────────────────────────────────────────────────────────────
// Matches Android WmpImageKind: none=0, webp=1, jpeg=2, png=3.

/// TLV kind indicating no / unknown image type (Android `WmpImageKind.none`).
pub const COVER_KIND_NONE: u8 = 0;
/// TLV kind for a WebP cover blob (Android `WmpImageKind.webp`).
pub const COVER_KIND_WEBP: u8 = 1;
/// TLV kind for a JPEG cover blob (Android `WmpImageKind.jpeg`).
pub const COVER_KIND_JPEG: u8 = 2;
/// TLV kind for a PNG cover blob (Android `WmpImageKind.png`).
pub const COVER_KIND_PNG: u8 = 3;

// ── Image kind detection ──────────────────────────────────────────────────────

/// Detect the TLV kind from leading magic bytes, matching Android `WmpImageKind.detect(bytes)`.
///
/// Checks (in order): JPEG (`FF D8 FF`), WebP (`RIFF….WEBP`), PNG (`89 PNG CR LF 1A LF`).
/// Returns `COVER_KIND_NONE` when no magic matches.
pub fn detect_image_kind(bytes: &[u8]) -> u8 {
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return COVER_KIND_JPEG;
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return COVER_KIND_WEBP;
    }
    if bytes.len() >= 8 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        return COVER_KIND_PNG;
    }
    COVER_KIND_NONE
}

// ── Normalizers ───────────────────────────────────────────────────────────────

/// Normalize source name per Android `MusicIdHelper.normalizeSourceName`:
/// trim leading/trailing whitespace, collapse internal whitespace runs to a single space.
pub fn normalize_source_name(s: &str) -> String {
    let trimmed = s.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut in_ws = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

/// Normalize remote path per Android `MusicIdHelper.normalizeRemotePath`:
/// trim, replace `\` with `/`, ensure exactly one leading `/`, collapse consecutive `/` runs.
///
/// **Does NOT lowercase** — Android keeps the original case for music-ID derivation.
pub fn normalize_remote_path_for_cover(s: &str) -> String {
    let raw: String = s.trim().chars().map(|c| if c == '\\' { '/' } else { c }).collect();
    let mut out = String::with_capacity(raw.len() + 1);
    let mut prev_slash = false;
    for ch in raw.chars() {
        if ch == '/' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            out.push(ch);
            prev_slash = false;
        }
    }
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    out
}

// ── Stem derivation ───────────────────────────────────────────────────────────

/// Compute the music cover filename stem matching Android `MusicIdHelper.musicIdForRemote`:
/// `sha1_hex( source_norm + '\0' + remote_norm )[0..16] + ".jpg"`.
pub fn music_cover_stem(source: &str, remote: &str) -> String {
    let src = normalize_source_name(source);
    let rem = normalize_remote_path_for_cover(remote);
    let mut h = Sha1::new();
    h.update(src.as_bytes());
    h.update(b"\x00");
    h.update(rem.as_bytes());
    let digest = h.finalize();
    let hex = format!("{digest:x}");
    format!("{}.jpg", &hex[..16])
}

/// Compute the manga cover filename stem matching Android `CacheService.mangaCoverFile`:
/// `manga_id[0..min(16, len)] + ".jpg"`.
pub fn manga_cover_stem(manga_id: &str) -> String {
    let n = manga_id.len().min(16);
    format!("{}.jpg", &manga_id[..n])
}

// ── Section build / parse ─────────────────────────────────────────────────────

/// Build a COVERS or MANGA_COVERS raw section payload from an ordered list of cover stems.
///
/// Reads `<covers_dir>/<stem>` for each unique stem that exists on disk, appending one TLV
/// entry per unique file: `[kind=0][u32 le len][JPEG bytes]`.  Duplicate stems share the same
/// index.
///
/// Returns `(section_bytes, indices)` where `indices[i]` is the 0-based cover index for
/// `stems[i]`, or `None` when the file was absent or unreadable.  Returns `(vec![], …)` when
/// no files could be read.
pub fn build_cover_section(covers_dir: &Path, stems: &[String]) -> (Vec<u8>, Vec<Option<u32>>) {
    let mut stem_to_idx: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let mut indices: Vec<Option<u32>> = Vec::with_capacity(stems.len());

    for stem in stems {
        if let Some(&idx) = stem_to_idx.get(stem) {
            indices.push(Some(idx));
        } else {
            match std::fs::read(covers_dir.join(stem)) {
                Ok(data) => {
                    let idx = blobs.len() as u32;
                    blobs.push(data);
                    stem_to_idx.insert(stem.clone(), idx);
                    indices.push(Some(idx));
                }
                Err(_) => {
                    indices.push(None);
                }
            }
        }
    }

    let mut section: Vec<u8> = Vec::new();
    for blob in &blobs {
        section.push(detect_image_kind(blob));
        section.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        section.extend_from_slice(blob);
    }
    (section, indices)
}

/// Parse a COVERS or MANGA_COVERS section payload → `Vec<(kind, bytes)>` indexed by cover index.
///
/// Truncated input is silently tolerated (the vec is cut short at the last complete entry),
/// matching Android's behaviour of treating a short / missing section as "no thumbnails".
pub fn parse_cover_section(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut pos = 0;
    while pos + 5 <= data.len() {
        let kind = data[pos];
        let len = u32::from_le_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]])
            as usize;
        pos += 5;
        if pos + len > data.len() {
            break; // truncated — stop gracefully
        }
        entries.push((kind, data[pos..pos + len].to_vec()));
        pos += len;
    }
    entries
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a uniquely-named test directory under std::env::temp_dir() and return its path.
    /// The caller owns the directory for the duration of the test; no automatic cleanup.
    fn test_cover_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wdmm_cover_test_{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn normalize_source_name_collapses_whitespace() {
        assert_eq!(normalize_source_name("  My  Vault "), "My Vault");
        assert_eq!(normalize_source_name("NAS"), "NAS");
        assert_eq!(normalize_source_name(""), "");
    }

    #[test]
    fn normalize_remote_path_for_cover_cases() {
        // keeps case, ensures leading slash, collapses doubles, converts backslash
        assert_eq!(
            normalize_remote_path_for_cover("Music\\Rock\\song.mp3"),
            "/Music/Rock/song.mp3"
        );
        assert_eq!(
            normalize_remote_path_for_cover("  /Music//Rock/song.mp3  "),
            "/Music/Rock/song.mp3"
        );
        assert_eq!(
            normalize_remote_path_for_cover("UPPER/Case/File.MP3"),
            "/UPPER/Case/File.MP3"   // no lowercasing
        );
    }

    #[test]
    fn music_cover_stem_matches_android() {
        // music_cover_stem must equal the first 16 hex chars of
        //   sha1( normalize_source_name(source) + '\0' + normalize_remote_path_for_cover(remote) )
        // followed by ".jpg".  Recompute independently here to guard against regressions.
        use sha1::{Digest, Sha1};
        let src = normalize_source_name("My Source");
        let rem = normalize_remote_path_for_cover("/Music/Track.mp3");
        let mut h = Sha1::new();
        h.update(src.as_bytes());
        h.update(b"\x00");
        h.update(rem.as_bytes());
        let d = h.finalize();
        let hex = format!("{d:x}");
        let expected = format!("{}.jpg", &hex[..16]);
        assert_eq!(music_cover_stem("My Source", "/Music/Track.mp3"), expected);

        // Different inputs must produce different stems (sanity).
        assert_ne!(
            music_cover_stem("My Source", "/Music/Track.mp3"),
            music_cover_stem("My Source", "/Music/Other.mp3")
        );
    }

    #[test]
    fn manga_cover_stem_truncates_at_16() {
        assert_eq!(manga_cover_stem("abc"), "abc.jpg");
        assert_eq!(manga_cover_stem("1234567890123456789"), "1234567890123456.jpg");
        assert_eq!(manga_cover_stem("1234567890123456"), "1234567890123456.jpg");
    }

    #[test]
    fn build_parse_cover_section_roundtrip() {
        let dir = test_cover_dir("build_parse_roundtrip");
        let stem1 = "aaaa0000bbbb1111.jpg".to_string();
        let stem2 = "cccc2222dddd3333.jpg".to_string();
        std::fs::write(dir.join(&stem1), b"\xff\xd8\xff\xe0cover1").unwrap();
        std::fs::write(dir.join(&stem2), b"\xff\xd8\xff\xe0cover2").unwrap();

        let stems = vec![stem1.clone(), stem2.clone(), stem1.clone()]; // stem1 deduplicated
        let (bytes, indices) = build_cover_section(&dir, &stems);
        assert_eq!(indices[0], Some(0));
        assert_eq!(indices[1], Some(1));
        assert_eq!(indices[2], Some(0)); // deduplicated

        let entries = parse_cover_section(&bytes);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, COVER_KIND_JPEG);
        assert_eq!(entries[0].1, b"\xff\xd8\xff\xe0cover1");
        assert_eq!(entries[1].1, b"\xff\xd8\xff\xe0cover2");
    }

    #[test]
    fn build_cover_section_missing_file_returns_none() {
        let dir = test_cover_dir("missing_file");
        let stems = vec!["nonexistent.jpg".to_string()];
        let (bytes, indices) = build_cover_section(&dir, &stems);
        assert!(bytes.is_empty());
        assert_eq!(indices[0], None);
    }

    #[test]
    fn parse_cover_section_tolerates_truncation() {
        // Valid entry followed by truncated data
        let mut data = Vec::new();
        data.push(0u8); // kind
        data.extend_from_slice(&4u32.to_le_bytes()); // len=4
        data.extend_from_slice(b"JPEG"); // 4 bytes
        data.push(0u8); // kind of next entry, truncated (no len/data)
        let entries = parse_cover_section(&data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, b"JPEG");
    }
}
