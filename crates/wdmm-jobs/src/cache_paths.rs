//! Cache layout: deterministic paths for cached tracks, manga archives, manga
//! pages and covers. Values must match the Dart `CacheService` layout exactly so
//! files cached by older builds stay usable.
//!
//! ## Layout (Dart `lib/services/cache_service.dart`)
//!
//! ```text
//! <support>/music_cache/<stem>_<basename>        music cache (fileForRemote)
//! <support>/manga_cache/<stem>_<basename>        manga archive (mangaFileForRemote)
//! <support>/manga_cache/covers/<id[..16]>.jpg    manga cover (mangaCoverFile)
//! <cache>/prefetch_cache/manga_pages/<mangaId>/  reader page prefetch (mangaPageDir)
//! <appDocs>/covers/<stem>.jpg                    track cover (CoverService)
//! <path>.part                                    in-flight transfer suffix
//! ```
//!
//! `<stem>` is `identityHashStem(sourceName, remotePath)` — the first 16 hex
//! chars of `sha1(normalizeSourceName(source) ‖ '\0' ‖ normalizeRemotePath(path))`
//! (Dart `lib/utils/track_identity.dart`). Renaming a disk therefore orphans its
//! cached files; that is the documented "改名 = 换盘" semantics.
//!
//! ## Root directories
//!
//! Dart keeps `music_cache` / `manga_cache` under
//! `getApplicationSupportDirectory()`, but `prefetch_cache` under
//! `getApplicationCacheDirectory()` and `covers` under
//! `getApplicationDocumentsDirectory()`. [`CachePaths::new`] takes a single
//! root (the media-cache root the FFI passes as `JobPaths::cache_root`) and
//! derives everything from it; use [`CachePaths::with_cache_root`] when the
//! page-prefetch root is a separate directory, and pass `JobPaths::cover_dir`
//! to [`CachePaths::cover_file`] for covers. The FFI must hand in the app
//! **support** directory (or an equivalent root containing `music_cache`), not
//! the app cache directory, or existing files will not be found.
//!
//! The SHA-1 used for the stem is implemented locally: `wdmm-jobs` does not
//! depend on `sha1`/`hex` and adding a dependency is out of scope. It is
//! exercised against Dart-produced golden vectors in the tests below.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Directory under the support root holding cached audio files.
pub const MUSIC_CACHE_DIR: &str = "music_cache";
/// Directory under the support root holding cached manga archives/covers.
pub const MANGA_CACHE_DIR: &str = "manga_cache";
/// Parent directory (under the app cache root) of the reader's page prefetch.
pub const PREFETCH_CACHE_DIR: &str = "prefetch_cache";
/// Leaf directory (under [`PREFETCH_CACHE_DIR`]) holding per-manga page dirs.
pub const MANGA_PAGES_DIR: &str = "manga_pages";
/// Sub-directory of the manga cache holding manga cover thumbs.
pub const COVERS_DIR: &str = "covers";
/// Suffix appended to the final path while a transfer is in flight.
pub const PART_SUFFIX: &str = ".part";
/// Scratch directory (under the cache root) for public downloads. Contract
/// §6.1 pins `<cache>/queue/<taskId>.part`; Dart used the OS temp dir under
/// `public_dl/`, which the shell cannot address deterministically.
pub const PUBLIC_SCRATCH_DIR: &str = "queue";

/// Deterministic cache paths derived from one media-cache root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePaths {
    support_root: PathBuf,
    cache_root: PathBuf,
}

impl CachePaths {
    /// Build the layout from the media-cache root.
    ///
    /// The root is the directory that contains `music_cache` and `manga_cache`
    /// (Dart: `getApplicationSupportDirectory()`), and by default also the
    /// parent of `prefetch_cache`. Use [`Self::with_cache_root`] to point page
    /// prefetch at the separate app cache directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            support_root: root.clone(),
            cache_root: root,
        }
    }

    /// Override the root used for `prefetch_cache/manga_pages`
    /// (Dart: `getApplicationCacheDirectory()`).
    pub fn with_cache_root(mut self, cache_root: impl Into<PathBuf>) -> Self {
        self.cache_root = cache_root.into();
        self
    }

    /// Root containing `music_cache` / `manga_cache`.
    pub fn support_root(&self) -> &Path {
        &self.support_root
    }

    /// Root containing `prefetch_cache` (and the public-download scratch dir).
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// `<support>/music_cache`.
    pub fn music_dir(&self) -> PathBuf {
        self.support_root.join(MUSIC_CACHE_DIR)
    }

    /// `<support>/manga_cache`.
    pub fn manga_dir(&self) -> PathBuf {
        self.support_root.join(MANGA_CACHE_DIR)
    }

    /// `<support>/manga_cache/covers`.
    pub fn manga_covers_dir(&self) -> PathBuf {
        self.manga_dir().join(COVERS_DIR)
    }

    /// `<cache>/prefetch_cache/manga_pages`.
    pub fn manga_pages_root(&self) -> PathBuf {
        self.cache_root
            .join(PREFETCH_CACHE_DIR)
            .join(MANGA_PAGES_DIR)
    }

    /// Cached audio file for a remote object (Dart `fileForRemote`).
    ///
    /// `source_name` is the disk name; `None`/empty reproduces the legacy
    /// Dart-branch that hashes the remote path itself.
    pub fn music_file_for(&self, remote_path: &str, source_name: Option<&str>) -> PathBuf {
        self.music_dir()
            .join(cache_file_name(remote_path, source_name))
    }

    /// Cached manga archive for a remote object (Dart `mangaFileForRemote`).
    pub fn manga_file_for(&self, remote_path: &str, source_name: &str) -> PathBuf {
        self.manga_dir()
            .join(cache_file_name(remote_path, Some(source_name)))
    }

    /// `<manga_cache>/covers/<first 16 chars>.jpg` (Dart `mangaCoverFile`).
    pub fn manga_cover_file(&self, manga_id: &str) -> PathBuf {
        let stem: String = manga_id.chars().take(16).collect();
        self.manga_covers_dir().join(format!("{stem}.jpg"))
    }

    /// `<cache>/prefetch_cache/manga_pages/<manga_id>` (Dart `mangaPageDir`).
    pub fn manga_page_dir(&self, manga_id: &str) -> PathBuf {
        self.manga_pages_root().join(manga_id)
    }

    /// Track cover thumb under `JobPaths::cover_dir`
    /// (Dart `CoverService.thumbPath` = `<appDocs>/covers/<stem>.jpg`).
    pub fn cover_file(
        &self,
        cover_dir: &Path,
        source_name: &str,
        remote_path: &str,
    ) -> PathBuf {
        let stem = identity_hash_stem(source_name, remote_path);
        cover_dir.join(format!("{stem}.jpg"))
    }

    /// Scratch path for a public download that is still in flight.
    ///
    /// Contract §6.1: `<cache>/queue/<taskId>.part`. Failures keep the file so
    /// the next attempt resumes from its length.
    pub fn public_temp_file(&self, task_id: &str) -> PathBuf {
        self.cache_root
            .join(PUBLIC_SCRATCH_DIR)
            .join(format!("{task_id}{PART_SUFFIX}"))
    }
}

/// Append the in-flight suffix (`<path>.part`) — Dart's `'${f.path}.part'`.
pub fn part_path(path: &Path) -> PathBuf {
    let mut os: OsString = path.as_os_str().to_os_string();
    os.push(PART_SUFFIX);
    PathBuf::from(os)
}

/// Dart `cacheFileNameForRemote(remotePath, sourceName: …)`.
///
/// `<stem>_<sanitised basename>`; when the source is missing/empty the legacy
/// branch hashes the remote path with Dart's `String.hashCode` (radix 16).
pub fn cache_file_name(remote_path: &str, source_name: Option<&str>) -> String {
    let base = sanitize_file_name(&dart_basename(remote_path));
    match source_name {
        Some(source) if !source.is_empty() => {
            format!("{}_{base}", identity_hash_stem(source, remote_path))
        }
        _ => format!("{:x}_{base}", dart_string_hash(remote_path)),
    }
}

/// Dart `sanitizeFileName` — replace filesystem-hostile characters with `_`.
pub fn sanitize_file_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect()
}

/// First 16 hex chars of [`music_id_for_remote`] (Dart `identityHashStem`).
pub fn identity_hash_stem(source_name: &str, remote_path: &str) -> String {
    music_id_for_remote(source_name, remote_path)
        .chars()
        .take(16)
        .collect()
}

/// Stable offline track identity (Dart `musicIdForRemote`), lower-case hex.
pub fn music_id_for_remote(source_name: &str, remote_path: &str) -> String {
    let key = format!(
        "{}\u{0}{}",
        normalize_source_name(source_name),
        normalize_remote_path(remote_path)
    );
    sha1_hex(key.as_bytes())
}

/// Dart `normalizeSourceName`: trim, collapse whitespace runs to one space.
pub fn normalize_source_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Dart `normalizeRemotePath`: trim, `\` → `/`, leading `/`, collapse `//`.
///
/// An empty path becomes `/` (Dart returns `'/'` before adding the prefix).
pub fn normalize_remote_path(path: &str) -> String {
    let mut out = path.trim().replace('\\', "/");
    if out.is_empty() {
        return "/".to_string();
    }
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    while out.contains("//") {
        out = out.replace("//", "/");
    }
    out
}

/// POSIX `p.basename` as used on Android (trailing slashes ignored).
fn dart_basename(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(index) => trimmed[index + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// Dart `String.hashCode` (VM): Jenkins one-at-a-time over UTF-16 code units,
/// masked to 30 bits. Verified against `dart` golden vectors in the tests.
fn dart_string_hash(value: &str) -> u32 {
    let mut hash: u32 = 0;
    for unit in value.encode_utf16() {
        hash = hash.wrapping_add(unit as u32);
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash = hash.wrapping_add(hash << 15);
    hash & 0x3fff_ffff
}

/// Lower-case hex SHA-1 digest.
fn sha1_hex(data: &[u8]) -> String {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
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
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let start = i * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut out = String::with_capacity(40);
    for word in state {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/data/app/files")
    }

    // Golden vectors produced by running the Dart helpers
    // (`lib/utils/audio_extensions.dart`, `lib/utils/track_identity.dart`)
    // under `dart run`.
    #[test]
    fn music_file_matches_dart_golden() {
        let paths = CachePaths::new(root());
        assert_eq!(
            paths.music_file_for("/Music/Album/Track 01.mp3", Some("nas")),
            PathBuf::from("/data/app/files/music_cache/2aebf9dd74238dfa_Track 01.mp3")
        );
    }

    #[test]
    fn music_file_for_flac_and_unicode_name() {
        let paths = CachePaths::new(root());
        assert_eq!(
            paths.music_file_for("/a/b/c.flac", Some("nas")),
            PathBuf::from("/data/app/files/music_cache/79017f43e0eadb4d_c.flac")
        );
        assert_eq!(
            paths.music_file_for("/音乐/专辑/歌 曲.flac", Some("nas")),
            PathBuf::from("/data/app/files/music_cache/95ca2fe9f086b9a8_歌 曲.flac")
        );
    }

    #[test]
    fn music_file_sanitises_illegal_characters() {
        let paths = CachePaths::new(root());
        assert_eq!(
            paths.music_file_for("/Music/que:st*ion?.mp3", Some("nas")),
            PathBuf::from("/data/app/files/music_cache/0462aa304f8128b8_que_st_ion_.mp3")
        );
    }

    #[test]
    fn music_file_without_source_uses_dart_string_hash() {
        let paths = CachePaths::new(root());
        let expected = PathBuf::from("/data/app/files/music_cache/2b18af72_x.mp3");
        assert_eq!(paths.music_file_for("/Music/x.mp3", None), expected);
        assert_eq!(paths.music_file_for("/Music/x.mp3", Some("")), expected);
    }

    #[test]
    fn basename_matches_posix_path_package() {
        assert_eq!(dart_basename("/a/b/c.mp3"), "c.mp3");
        assert_eq!(dart_basename("/a/b/"), "b");
        assert_eq!(dart_basename("/"), "/");
        assert_eq!(dart_basename(""), "");
        assert_eq!(dart_basename("c.mp3"), "c.mp3");
        assert_eq!(dart_basename("/a//b//c.mp3"), "c.mp3");
    }

    #[test]
    fn manga_layout_matches_dart() {
        let paths = CachePaths::new(root());
        assert_eq!(
            paths.manga_file_for("/Manga/Vol 1.cbz", "nas"),
            PathBuf::from("/data/app/files/manga_cache/6eddf1c86868c1f9_Vol 1.cbz")
        );
        assert_eq!(
            paths.manga_cover_file("0123456789abcdef0123456789abcdef"),
            PathBuf::from("/data/app/files/manga_cache/covers/0123456789abcdef.jpg")
        );
        assert_eq!(
            paths.manga_cover_file("short-id"),
            PathBuf::from("/data/app/files/manga_cache/covers/short-id.jpg")
        );
    }

    #[test]
    fn manga_page_dir_uses_prefetch_root() {
        let paths = CachePaths::new(root()).with_cache_root("/data/app/cache");
        assert_eq!(
            paths.manga_page_dir("m1"),
            PathBuf::from("/data/app/cache/prefetch_cache/manga_pages/m1")
        );
        assert_eq!(
            paths.manga_pages_root(),
            PathBuf::from("/data/app/cache/prefetch_cache/manga_pages")
        );
    }

    #[test]
    fn part_suffix_appends_to_full_name() {
        assert_eq!(
            part_path(Path::new("/data/a/b.flac")),
            PathBuf::from("/data/a/b.flac.part")
        );
        // A path that already looks like a part still gains the suffix.
        assert_eq!(
            part_path(Path::new("/data/a/b.flac.part")),
            PathBuf::from("/data/a/b.flac.part.part")
        );
    }

    #[test]
    fn public_temp_lives_under_queue_scratch() {
        let paths = CachePaths::new(root());
        assert_eq!(
            paths.public_temp_file("task-7"),
            PathBuf::from("/data/app/files/queue/task-7.part")
        );
    }

    #[test]
    fn cover_file_uses_job_paths_cover_dir() {
        let paths = CachePaths::new(root());
        assert_eq!(
            paths.cover_file(Path::new("/data/app/docs/covers"), "nas", "/Music/x.mp3"),
            PathBuf::from("/data/app/docs/covers/93fee4452069e81d.jpg")
        );
    }

    #[test]
    fn identity_stem_is_sixteen_lower_hex_chars() {
        let stem = identity_hash_stem("nas", "/Music/Album/Track 01.mp3");
        assert_eq!(stem, "2aebf9dd74238dfa");
        assert!(stem.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        let id = music_id_for_remote("nas", "/Music/Album/Track 01.mp3");
        assert_eq!(id.len(), 40);
        assert!(id.starts_with(&stem));
    }

    #[test]
    fn source_and_path_normalisation_follows_dart() {
        assert_eq!(normalize_source_name("  My   Disk \t"), "My Disk");
        assert_eq!(normalize_remote_path(" /a\\\\b//c.mp3 "), "/a/b/c.mp3");
        assert_eq!(normalize_remote_path(""), "/");
        assert_eq!(normalize_remote_path("a"), "/a");
        // Same logical identity despite slash variants and padding.
        assert_eq!(
            identity_hash_stem("My  Disk", "/a/b/c.mp3"),
            identity_hash_stem(" My Disk ", "//a//b/c.mp3")
        );
        // Dart golden for the "My Disk" fixture (POSIX basename).
        assert_eq!(
            identity_hash_stem("My Disk", "//Music\\\\A.mp3"),
            "21a780471263ccac"
        );
    }

    #[test]
    fn dart_string_hash_matches_vm_vectors() {
        assert_eq!(format!("{:x}", dart_string_hash("a")), "a2e9442");
        assert_eq!(format!("{:x}", dart_string_hash("abc")), "2d131f5b");
        assert_eq!(format!("{:x}", dart_string_hash("/Music/x.mp3")), "2b18af72");
        assert_eq!(
            format!("{:x}", dart_string_hash("/音乐/专辑/歌 曲.flac")),
            "2bd2a131"
        );
        assert_eq!(format!("{:x}", dart_string_hash("hello world")), "3e4a5a57");
    }

    #[test]
    fn sha1_matches_known_digests() {
        assert_eq!(
            sha1_hex(b""),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            sha1_hex(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // 56-byte boundary (extra padding block).
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }
}
