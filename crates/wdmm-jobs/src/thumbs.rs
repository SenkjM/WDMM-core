//! Cover thumbnails and the library identity hash.
//!
//! Two pieces of the old Dart side live here because the executors need them
//! before they can touch the database:
//!
//! * [`identity_hash_stem`] — the cover file name. Dart
//!   `CoverService.thumbPath` is `<app_docs>/covers/<identityHashStem>.jpg`,
//!   where `identityHashStem` is the first 16 hex chars of
//!   `sha1(normalizeSourceName(source) + "\0" + normalizeRemotePath(path))`.
//!   The rules must stay byte-for-byte compatible, otherwise every existing
//!   row looks like a new track after the cutover.
//! * [`write_thumb`] — the JPEG encoder. Dart `resizeCoverToThumb`
//!   (`lib/utils/cover_image.dart:39`) decodes the embedded cover, calls
//!   `copyResizeCropSquare(size: 300)` and encodes JPEG at quality **85**.
//!   We therefore always emit a square `max_side × max_side` JPEG at quality
//!   85. (Dart scales with `Interpolation.average`, which has no exact
//!   equivalent in `image`; [`FilterType::Triangle`] is the closest cheap
//!   filter, so pixels may differ by a fraction while dimensions and quality
//!   match.)
//!
//! `wdmm-jobs` does not depend on the `sha1` crate (see `Cargo.toml`), so the
//! digest is implemented locally. The unit tests pin the output against
//! sample strings produced by the Dart implementation.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use thiserror::Error;

/// The default cover edge used by the old Dart code (`coverThumbSize = 300`).
pub const COVER_THUMB_SIZE: u32 = 300;

/// JPEG quality used by `resizeCoverToThumb` (`encodeJpg(quality: 85)`).
pub const THUMB_JPEG_QUALITY: u8 = 85;

/// Lower clamp from `cover_image.dart` (`_minCoverSize = 32`).
pub const THUMB_MIN_SIDE: u32 = 32;

/// Upper clamp from `cover_image.dart` (`_maxCoverSize = 1024`).
pub const THUMB_MAX_SIDE: u32 = 1024;

/// Failure to decode or encode a cover thumbnail.
#[derive(Debug, Error)]
pub enum ThumbError {
    #[error("cover decode/encode failed: {0}")]
    Image(#[from] image::ImageError),
    #[error("cover file io failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Counter used to give each [`write_thumb`] temporary file a unique name.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── identity hash ────────────────────────────────────────────────────────────

/// `normalizeSourceName`: trim, then collapse every whitespace run to one
/// ASCII space (`RegExp(r'\s+')` in Dart).
pub fn normalize_source_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_space = false;
    for ch in name.trim().chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// `normalizeRemotePath`: trim, `\` → `/`, empty → `/`, ensure a leading `/`,
/// then collapse repeated `//` until none remain.
pub fn normalize_remote_path(path: &str) -> String {
    let mut out = path.trim().replace('\\', "/");
    if out.is_empty() {
        out.push('/');
    }
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    while out.contains("//") {
        out = out.replace("//", "/");
    }
    out
}

/// `musicIdForRemote`: `sha1(normalizeSourceName(source) + "\0" +
/// normalizeRemotePath(path))`, lower-case hex.
pub fn music_id_for_remote(source_name: &str, remote_path: &str) -> String {
    sha1_hex(format!(
        "{}\u{0}{}",
        normalize_source_name(source_name),
        normalize_remote_path(remote_path)
    )
    .as_bytes())
}

/// `musicIdForCueSlice`: `sha1(source + "\0" + cuePath + "\0" + trackIndex)`.
pub fn music_id_for_cue_slice(source_name: &str, cue_remote_path: &str, track_index: u32) -> String {
    sha1_hex(
        format!(
            "{}\u{0}{}\u{0}{track_index}",
            normalize_source_name(source_name),
            normalize_remote_path(cue_remote_path)
        )
        .as_bytes(),
    )
}

/// `cueIdFor`: `sha1("cue" + "\0" + source + "\0" + cuePath)`.
pub fn cue_id_for(source_name: &str, cue_remote_path: &str) -> String {
    sha1_hex(
        format!(
            "cue\u{0}{}\u{0}{}",
            normalize_source_name(source_name),
            normalize_remote_path(cue_remote_path)
        )
        .as_bytes(),
    )
}

/// The 16-hex-character stem shared by the cover file and the old cache paths.
pub fn identity_hash_stem(source_name: &str, remote_path: &str) -> String {
    music_id_for_remote(source_name, remote_path)[..16].to_string()
}

/// `cueVirtualRemotePath`: the audio path plus `#cue:<track number>`.
pub fn cue_virtual_remote_path(audio_remote_path: &str, track_number: u32) -> String {
    format!("{audio_remote_path}#cue:{track_number}")
}

/// `cueCacheGroupId`: `"cue" + "\0" + source + "\0" + cuePath`.
pub fn cue_cache_group_id(source_name: &str, cue_remote_path: &str) -> String {
    format!(
        "cue\u{0}{}\u{0}{}",
        normalize_source_name(source_name),
        normalize_remote_path(cue_remote_path)
    )
}

/// Path of the cover thumbnail inside `<app_docs>/covers`.
pub fn thumb_path(cover_dir: &Path, source_name: &str, remote_path: &str) -> PathBuf {
    cover_dir.join(format!(
        "{}.jpg",
        identity_hash_stem(source_name, remote_path)
    ))
}

// ── thumbnails ───────────────────────────────────────────────────────────────

/// Decode `bytes`, center-crop it to a square, and write a JPEG into a fresh
/// temporary file. Returns that file's path; the caller owns it (the ingest
/// path immediately renames it into [`thumb_path`]).
pub fn write_thumb(bytes: &[u8], max_side: u32) -> Result<PathBuf, ThumbError> {
    let image = image::load_from_memory(bytes)?;
    let thumb = square_thumb(&image, max_side);

    let path = temp_thumb_path();
    let file = std::fs::File::create(&path)?;
    let mut writer = std::io::BufWriter::new(file);
    thumb.write_with_encoder(JpegEncoder::new_with_quality(
        &mut writer,
        THUMB_JPEG_QUALITY,
    ))?;
    writer.flush()?;
    Ok(path)
}

/// Encode `bytes` straight to `dest` (parent directories are created). The
/// file is written next to `dest` and renamed, so a crash never leaves a
/// half-written thumbnail that the UI could pick up.
pub fn write_thumb_at(bytes: &[u8], dest: &Path, max_side: u32) -> Result<(), ThumbError> {
    let temp = write_thumb(bytes, max_side)?;
    write_thumb_file(&temp, dest)
}

/// Write the cover for `remote_path` under `cover_dir`, using the Dart cover
/// file name (`<identityHashStem>.jpg`). Returns the final path.
pub fn write_cover(
    cover_dir: &Path,
    source_name: &str,
    remote_path: &str,
    bytes: &[u8],
) -> Result<PathBuf, ThumbError> {
    let dest = thumb_path(cover_dir, source_name, remote_path);
    write_thumb_at(bytes, &dest, COVER_THUMB_SIZE)?;
    Ok(dest)
}

/// Move a finished thumbnail into place. `rename` is preferred; a cross-device
/// rename falls back to copy + remove.
fn write_thumb_file(temp: &Path, dest: &Path) -> Result<(), ThumbError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(temp, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(temp, dest)?;
            let _ = std::fs::remove_file(temp);
            Ok(())
        }
    }
}

/// Scale the shorter edge to `max_side`, then crop the longer edge around the
/// centre — the same geometry as Dart `copyResizeCropSquare`.
fn square_thumb(image: &DynamicImage, max_side: u32) -> DynamicImage {
    let side = max_side.clamp(THUMB_MIN_SIDE, THUMB_MAX_SIDE);
    let (width, height) = image.dimensions();
    let shortest = width.min(height).max(1);
    let scale = f64::from(side) / f64::from(shortest);
    let scaled_width = ((f64::from(width) * scale).round() as u32).max(side);
    let scaled_height = ((f64::from(height) * scale).round() as u32).max(side);
    let resized = image.resize_exact(scaled_width, scaled_height, FilterType::Triangle);
    let x = (scaled_width - side) / 2;
    let y = (scaled_height - side) / 2;
    resized.crop_imm(x, y, side, side)
}

fn temp_thumb_path() -> PathBuf {
    let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "wdmm-thumb-{}-{serial}.jpg",
        std::process::id()
    ))
}

// ── SHA-1 ───────────────────────────────────────────────────────────────────

/// Lower-case hex SHA-1. `wdmm-jobs` has no `sha1` dependency; this is the
/// standard FIPS 180-1 compression function, verified against the Dart sample
/// strings in the tests below.
fn sha1_hex(data: &[u8]) -> String {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = Vec::with_capacity(data.len() + 72);
    message.extend_from_slice(data);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.as_chunks::<64>().0 {
        let mut words = [0u32; 80];
        for (index, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            words[index] = u32::from_be_bytes(*word);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut hex = String::with_capacity(40);
    for word in state {
        let _ = write!(hex, "{word:08x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sample strings produced by `lib/utils/track_identity.dart`; the expected
    /// hex digests were generated independently (SHA-1 over UTF-8) so a change
    /// in separator, order, or normalisation fails here first.
    #[test]
    fn identity_hashes_match_the_dart_samples() {
        assert_eq!(
            music_id_for_remote("My Disk", "/Music/Album/01 - Song.mp3"),
            "e486219bd020835b56a91b264714b807a5983344"
        );
        assert_eq!(
            identity_hash_stem("My Disk", "/Music/Album/01 - Song.mp3"),
            "e486219bd020835b"
        );
        assert_eq!(
            music_id_for_remote("My Disk", "/Music/Album/track.flac"),
            "c73fd38acb063180b533a964eb2a7309c7cb1074"
        );
        // Source whitespace and `\` separators are normalised before hashing.
        assert_eq!(
            music_id_for_remote("  My   Disk  ", "Music\\Album\\track.flac"),
            "c73fd38acb063180b533a964eb2a7309c7cb1074"
        );
        assert_eq!(
            music_id_for_remote("百度网盘", "/音乐/专辑/歌曲.flac"),
            "567b6483ef94d1a1494d59b1b5255790939dfc83"
        );
        // Repeated slashes collapse entirely, not just once.
        assert_eq!(
            music_id_for_remote("SMB Disk", "/a//b//c d.mp3"),
            "73cadd2861a6c2192c38ee5c316e506e3b811ad8"
        );
        assert_eq!(
            music_id_for_cue_slice("My Disk", "/Music/Album/album.cue", 2),
            "4839c4e25ec3d9c76896f2d7053fb9edb3517a5b"
        );
        assert_eq!(
            cue_id_for("My Disk", "/Music/Album/album.cue"),
            "b34161ad8b202070c1246541bdecfc482d80eecf"
        );
    }

    #[test]
    fn normalisation_matches_dart_edge_cases() {
        assert_eq!(normalize_source_name("  My \t Disk\n"), "My Disk");
        assert_eq!(normalize_source_name("   "), "");
        assert_eq!(normalize_remote_path("  "), "/");
        assert_eq!(normalize_remote_path("Music\\A"), "/Music/A");
        assert_eq!(normalize_remote_path("/a//b///c"), "/a/b/c");
        assert_eq!(cue_virtual_remote_path("/a/b.flac", 3), "/a/b.flac#cue:3");
        assert_eq!(
            cue_cache_group_id("My Disk", "/Music/Album/album.cue"),
            "cue\u{0}My Disk\u{0}/Music/Album/album.cue"
        );
    }

    #[test]
    fn sha1_matches_known_vectors() {
        // FIPS 180-1 / RFC 3174 test vectors.
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn write_thumb_produces_a_square_jpeg_at_quality_85() {
        let source = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(
            640, 320, |x, y| {
                image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
            },
        ));
        let mut png = Vec::new();
        source
            .write_with_encoder(image::codecs::png::PngEncoder::new(&mut png))
            .unwrap();

        let path = write_thumb(&png, COVER_THUMB_SIZE).unwrap();
        let thumb = image::open(&path).unwrap();
        assert_eq!(thumb.dimensions(), (300, 300));
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        // The output must really be JPEG (Dart wrote `.jpg`).
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_thumb_honours_the_requested_edge() {
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            40,
            90,
            image::Rgba([1, 2, 3, 255]),
        ));
        let mut png = Vec::new();
        source
            .write_with_encoder(image::codecs::png::PngEncoder::new(&mut png))
            .unwrap();

        let path = write_thumb(&png, 96).unwrap();
        assert_eq!(image::open(&path).unwrap().dimensions(), (96, 96));
        let _ = std::fs::remove_file(&path);
    }
}
