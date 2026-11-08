//! Track identity — the **single** implementation behind every music id.
//!
//! Identity is `网盘名 + remote_path`: the URL, username and password never
//! participate, so moving the backing disk does not rewrite the library. Dart's
//! `lib/utils/track_identity.dart` is a pure mirror of these functions because
//! the data model and several host tests are Dart-only; the two sides are
//! pinned together by the same golden literals (see the tests below and
//! `test/track_identity_golden_test.dart`).
//!
//! This is the *track* identity. `crate::identity` is a different, legacy
//! storage-row convention (lowercased path, no source name) and must not be
//! used for new library rows.

use sha1::{Digest, Sha1};

/// Normalize a disk name so trailing spaces / inner runs don't split identity.
///
/// Dart: `name.trim().replaceAll(RegExp(r'\s+'), ' ')`.
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

/// Normalize a remote path so `music_id` stays stable across slash variants.
///
/// Dart: trim, `\` → `/`, empty → `/`, ensure a leading `/`, collapse `//`.
/// Case is preserved (cloud paths are case-sensitive).
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

fn sha1_hex(bytes: &[u8]) -> String {
    let digest = Sha1::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Stable offline music_id for a normal (non-CUE-slice) track.
///
/// `sha1(normalizeSourceName + "\0" + normalizeRemotePath)` — not a content
/// hash of the audio bytes.
pub fn music_id_for_remote(source_name: &str, remote_path: &str) -> String {
    let key = format!(
        "{}\u{0}{}",
        normalize_source_name(source_name),
        normalize_remote_path(remote_path)
    );
    sha1_hex(key.as_bytes())
}

/// Stable offline music_id for a CUE virtual slice:
/// `sha1(source + "\0" + cuePath + "\0" + trackIndex)`.
pub fn music_id_for_cue_slice(
    source_name: &str,
    cue_remote_path: &str,
    track_index: u32,
) -> String {
    let key = format!(
        "{}\u{0}{}\u{0}{track_index}",
        normalize_source_name(source_name),
        normalize_remote_path(cue_remote_path)
    );
    sha1_hex(key.as_bytes())
}

/// Cue album identity: `sha1("cue" + "\0" + source + "\0" + cuePath)`.
pub fn cue_id_for(source_name: &str, cue_remote_path: &str) -> String {
    let key = format!(
        "cue\u{0}{}\u{0}{}",
        normalize_source_name(source_name),
        normalize_remote_path(cue_remote_path)
    );
    sha1_hex(key.as_bytes())
}

/// Stable 16-hex-character stem for cache / cover file names.
pub fn identity_hash_stem(source_name: &str, remote_path: &str) -> String {
    music_id_for_remote(source_name, remote_path)
        .chars()
        .take(16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The literals below are the cross-language contract: the identical values
    // live in `test/track_identity_golden_test.dart`. Changing a rule means
    // changing both sides in one commit, or the golden test fails.
    const GOLDEN_PLAIN: &str = "632274b61f5bebcc7215a15621076969e96b19ed";
    const GOLDEN_MESSY: &str = "a14ff280292b3e59249aec8d906503bee71a7e5f";
    const GOLDEN_SLICE: &str = "9cdbcc6783574188a79c1d7bd8e3732ee68c5a07";
    const GOLDEN_CUE_ID: &str = "92db70b74655e4db8ab051f4079e22fa1da1eb75";
    const GOLDEN_EMPTY: &str = "167d40b282fa3ab3a759bebaa971561c07cf03f4";

    #[test]
    fn music_id_matches_the_cross_language_golden() {
        assert_eq!(
            music_id_for_remote("Disk A", "/Music/Artist - Song.mp3"),
            GOLDEN_PLAIN
        );
        assert_eq!(identity_hash_stem("Disk A", "/Music/Artist - Song.mp3"), &GOLDEN_PLAIN[..16]);
    }

    #[test]
    fn music_id_normalizes_source_and_path_before_hashing() {
        // Trailing/inner whitespace in the disk name and backslash/double-slash
        // variants in the path must collapse onto the same id.
        assert_eq!(
            music_id_for_remote("Disk  B  ", "\\Music//Album/../Track.flac"),
            GOLDEN_MESSY
        );
        assert_eq!(
            music_id_for_remote("Disk  B  ", "\\Music//Album/../Track.flac"),
            music_id_for_remote("Disk B", "/Music/Album/../Track.flac")
        );
    }

    #[test]
    fn cue_slice_and_album_ids_match_the_cross_language_goldens() {
        assert_eq!(
            music_id_for_cue_slice("Disk A", "/Music/album.cue", 3),
            GOLDEN_SLICE
        );
        assert_eq!(cue_id_for("Disk A", "/Music/album.cue"), GOLDEN_CUE_ID);
    }

    #[test]
    fn empty_paths_keep_the_dart_contract() {
        assert_eq!(normalize_remote_path(""), "/");
        assert_eq!(normalize_source_name("   "), "");
        assert_eq!(music_id_for_remote("", ""), GOLDEN_EMPTY);
    }
}
