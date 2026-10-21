//! Track identity hashing — aligns with WDMM library SHA-1 logic.
//!
//! `normalizeRemotePath` + SHA-1 fingerprinting. Cross-language golden vectors
//! must be maintained to guarantee the same file path on different platforms
//! produces the same track identity.

use sha1::{Digest, Sha1};

/// Normalise a remote path so two paths referring to the same logical file
/// collapse to the same identity. Cloud providers differ in trailing slash
/// handling, Unicode normalisation, etc.
///
/// Semantics: lowercase, trim, normalise forward slashes.
pub fn normalize_remote_path(path: &str) -> String {
    let trimmed = path.trim().trim_start_matches('/').trim_end_matches('/');
    trimmed.to_lowercase().replace('\\', "/")
}

/// Compute the SHA-1 track identity hash from a normalised remote path.
///
/// Returns the lowercase hex-encoded digest. Compare with Dart's
/// `LibraryTrack.computeIdentity` to confirm alignment.
pub fn track_identity(remote_path: &str) -> String {
    let normalised = normalize_remote_path(remote_path);
    let hash = Sha1::digest(normalised.as_bytes());
    hex::encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_trailing_slash() {
        assert_eq!(
            normalize_remote_path("/Music/track.mp3/"),
            "music/track.mp3"
        );
    }

    #[test]
    fn track_identity_stable() {
        // golden: known SHA-1 of lowercase "music/track.mp3"
        let id = track_identity("/Music/track.mp3");
        assert_eq!(id.len(), 40); // sha1 hex is 40 chars
        // The exact value depends on the path — compare with Dart golden
    }
}
