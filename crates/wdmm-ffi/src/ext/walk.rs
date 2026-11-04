//! Slice 3: recursive account walk owned by Rust (audit item 5).
//!
//! `wdmm_account_walk(account_id, root, max_depth, max_entries, kinds_json)`
//! replaces the three Dart walks (`WebDavService.collectFilesRecursive`, the
//! `CloudDriveService` copy and `DirectoryWalker` in
//! `lib/utils/directory_walk.dart`). Traversal is iterative (BFS, no
//! recursion), paths are de-duplicated, and the reply carries the metadata Dart
//! used to fetch one directory at a time.
//!
//! Reply contract (`dart-cutover-slices.md` §Slice 3):
//!
//! ```text
//! {"ok":true,"entries":[{"path","name","size","mtime","is_dir"}],"truncated":false,"scanned":N}
//! {"ok":false,"error":"limit","entries":[...partial...],"truncated":true,"scanned":N}
//! ```
//!
//! `kinds_json` defaults to `{"kinds":["any"]}`. Kinds are judged on the file
//! extension: `video`, `audio`, `image`, `manga`, `any`. An optional
//! `"extensions":["mp4",...]` member overrides the built-in tables so the
//! caller can hand over the user-configured extension sets from
//! `FileTypeConfig`. Directories are always part of `entries`, only files are
//! filtered by kind.
//!
//! `max_depth` counts directory levels with the root at level 1 (`0` =
//! unlimited). `max_entries` caps how many directories are listed, root
//! included (`0` = unlimited). Both mirror `WalkOptions` on the Dart side.

use std::collections::{HashSet, VecDeque};
use std::os::raw::c_char;

use wdmm_drivers::WdmmVfs;

use super::{json_reply, parse_u64_arg, require_arg};
use crate::{ptr_to_str, rt, FfiString};

/// Extensions the built-in `video` kind recognises.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "flv", "ts", "m4v", "wmv", "3gp", "mpg", "mpeg",
];

/// Extensions the built-in `audio` kind recognises.
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "wav", "ogg", "opus", "wma",
];

/// Extensions the built-in `image` kind recognises.
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "avif"];

/// Extensions the built-in `manga` kind recognises (comic containers only;
/// image pages are collected per directory by the manga walk).
const MANGA_EXTENSIONS: &[&str] = &["cbz", "cbr", "cb7", "cbt"];

const WALK_KINDS: &[&str] = &["video", "audio", "image", "manga", "any"];

/// One walked entry. `mtime` is Unix milliseconds.
#[derive(Debug, Clone)]
struct WalkEntry {
    path: String,
    name: String,
    size: u64,
    mtime: Option<i64>,
    is_dir: bool,
}

impl WalkEntry {
    fn into_value(self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("path".to_string(), serde_json::Value::from(self.path));
        map.insert("name".to_string(), serde_json::Value::from(self.name));
        map.insert("size".to_string(), serde_json::Value::from(self.size));
        map.insert(
            "mtime".to_string(),
            match self.mtime {
                Some(ms) => serde_json::Value::from(ms),
                None => serde_json::Value::Null,
            },
        );
        map.insert("is_dir".to_string(), serde_json::Value::Bool(self.is_dir));
        serde_json::Value::Object(map)
    }
}

/// The walk reply described by the slice-3 contract.
#[derive(Debug)]
struct WalkReply {
    ok: bool,
    error: Option<&'static str>,
    entries: Vec<WalkEntry>,
    truncated: bool,
    scanned: u64,
}

impl WalkReply {
    /// A reply whose directory fuse tripped: the partial result stays usable
    /// and the caller is told why the walk stopped early.
    fn limited(entries: Vec<WalkEntry>, scanned: u64) -> Self {
        Self {
            ok: false,
            error: Some("limit"),
            entries,
            truncated: true,
            scanned,
        }
    }

    /// Shape the reply exactly as the slice-3 contract describes. `serde` is
    /// not a direct dependency of this crate, so the object is built by hand.
    fn into_value(self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("ok".to_string(), serde_json::Value::Bool(self.ok));
        if let Some(error) = self.error {
            map.insert("error".to_string(), serde_json::Value::from(error));
        }
        map.insert(
            "entries".to_string(),
            serde_json::Value::Array(
                self.entries
                    .into_iter()
                    .map(WalkEntry::into_value)
                    .collect(),
            ),
        );
        map.insert(
            "truncated".to_string(),
            serde_json::Value::Bool(self.truncated),
        );
        map.insert(
            "scanned".to_string(),
            serde_json::Value::from(self.scanned),
        );
        serde_json::Value::Object(map)
    }
}

/// Extension/kind filter parsed from `kinds_json`.
#[derive(Debug)]
struct WalkFilter {
    kinds: Vec<String>,
    extensions: Option<HashSet<String>>,
}

impl WalkFilter {
    fn parse(raw: Option<&str>) -> Result<Self, String> {
        let raw = raw.map(str::trim).unwrap_or_default();
        if raw.is_empty() {
            return Ok(Self {
                kinds: vec!["any".to_string()],
                extensions: None,
            });
        }
        let value: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| format!("invalid kinds_json: {e}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| "invalid kinds_json: expected an object".to_string())?;

        let mut kinds = Vec::new();
        match object.get("kinds") {
            None | Some(serde_json::Value::Null) => kinds.push("any".to_string()),
            Some(serde_json::Value::Array(items)) => {
                for item in items {
                    let kind = item
                        .as_str()
                        .ok_or_else(|| "invalid kinds_json: kind must be a string".to_string())?;
                    let kind = kind.trim().to_ascii_lowercase();
                    if !WALK_KINDS.contains(&kind.as_str()) {
                        return Err(format!("invalid kinds_json: unknown kind: {kind}"));
                    }
                    kinds.push(kind);
                }
            }
            Some(_) => return Err("invalid kinds_json: kinds must be an array".to_string()),
        }
        if kinds.is_empty() {
            kinds.push("any".to_string());
        }

        let extensions = match object.get("extensions") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Array(items)) => {
                let mut set = HashSet::new();
                for item in items {
                    let raw = item.as_str().ok_or_else(|| {
                        "invalid kinds_json: extension must be a string".to_string()
                    })?;
                    let extension = normalize_extension(raw);
                    if !extension.is_empty() {
                        set.insert(extension);
                    }
                }
                Some(set)
            }
            Some(_) => return Err("invalid kinds_json: extensions must be an array".to_string()),
        };

        Ok(Self { kinds, extensions })
    }

    /// Whether a regular file belongs to the requested kinds.
    fn accepts(&self, name: &str) -> bool {
        let extension = extension_of(name);
        if let Some(extensions) = &self.extensions {
            return !extension.is_empty() && extensions.contains(&extension);
        }
        if self.kinds.iter().any(|kind| kind == "any") {
            return true;
        }
        self.kinds.iter().any(|kind| match kind.as_str() {
            "video" => VIDEO_EXTENSIONS.contains(&extension.as_str()),
            "audio" => AUDIO_EXTENSIONS.contains(&extension.as_str()),
            "image" => IMAGE_EXTENSIONS.contains(&extension.as_str()),
            "manga" => MANGA_EXTENSIONS.contains(&extension.as_str()),
            _ => false,
        })
    }
}

/// Trailing-dot/case-insensitive extension of a remote file name.
fn extension_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, extension)) if !extension.is_empty() => extension.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Extension list entries may arrive as `.mp4`, `MP4` or `mp4`.
fn normalize_extension(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('.')
        .to_ascii_lowercase()
}

/// The remaining-directory budget (`max_entries` in the contract).
///
/// The old Dart `WalkOptions.maxEntries` counted listed *directories*, root
/// included; the fuse trips before the listing that would exceed it.
struct DirBudget {
    max: u64,
    listed: u64,
}

impl DirBudget {
    fn new(max: u64) -> Self {
        Self { max, listed: 0 }
    }

    /// Take one listing slot. `false` means the fuse tripped.
    fn take(&mut self) -> bool {
        if self.max != 0 && self.listed >= self.max {
            return false;
        }
        self.listed += 1;
        true
    }
}

fn normalize_root(root: &str) -> String {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return "/".to_string();
    }
    let mut path = trimmed.to_string();
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        format!("/{name}")
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Breadth-first walk over one account. Returns the slice-3 reply; only a
/// driver failure (a directory that cannot be listed) is a hard error.
fn walk_tree(
    driver: &dyn WdmmVfs,
    root: &str,
    max_depth: u64,
    max_entries: u64,
    filter: &WalkFilter,
) -> Result<WalkReply, String> {
    let root = normalize_root(root);
    let mut budget = DirBudget::new(max_entries);
    let mut queue: VecDeque<(String, u64)> = VecDeque::new();
    queue.push_back((root, 1));
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries: Vec<WalkEntry> = Vec::new();

    while let Some((dir, level)) = queue.pop_front() {
        if !budget.take() {
            return Ok(WalkReply::limited(entries, budget.listed));
        }
        let items = rt()
            .block_on(WdmmVfs::list(driver, &dir))
            .map_err(|e| format!("list {dir}: {e}"))?;
        for item in items {
            if item.name.is_empty() || item.name == "." || item.name == ".." {
                continue;
            }
            let path = join_path(&dir, &item.name);
            if !seen.insert(path.clone()) {
                continue;
            }
            let within_depth = max_depth == 0 || level.saturating_add(1) <= max_depth;
            if item.is_dir && within_depth {
                queue.push_back((path.clone(), level + 1));
            }
            if item.is_dir || filter.accepts(&item.name) {
                entries.push(WalkEntry {
                    path,
                    name: item.name,
                    size: item.size,
                    mtime: item.modified.map(|m| m.timestamp_millis()),
                    is_dir: item.is_dir,
                });
            }
        }
    }

    Ok(WalkReply {
        ok: true,
        error: None,
        entries,
        truncated: false,
        scanned: budget.listed,
    })
}

/// Recursively walk `root` of `account_id`.
///
/// `account_id` and `root` are required; `max_depth` / `max_entries` are
/// decimal strings (`0` = unlimited) and `kinds_json` may be null for
/// `{"kinds":["any"]}`.
#[no_mangle]
pub extern "C" fn wdmm_account_walk(
    account_id: *const c_char,
    root: *const c_char,
    max_depth: *const c_char,
    max_entries: *const c_char,
    kinds_json: *const c_char,
) -> FfiString {
    let account_id = match require_arg(account_id, "account_id") {
        Ok(value) => value,
        Err(err) => return err,
    };
    let root = match require_arg(root, "root") {
        Ok(value) => value,
        Err(err) => return err,
    };
    let max_depth = match parse_u64_arg(max_depth, "max_depth") {
        Ok(value) => value,
        Err(err) => return err,
    };
    let max_entries = match parse_u64_arg(max_entries, "max_entries") {
        Ok(value) => value,
        Err(err) => return err,
    };
    let filter = match WalkFilter::parse(ptr_to_str(kinds_json)) {
        Ok(filter) => filter,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let driver = match crate::driver_for_account(account_id) {
        Ok(driver) => driver,
        Err(err) => return err,
    };
    let reply = match walk_tree(&*driver, root, max_depth, max_entries, &filter) {
        Ok(reply) => reply,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    json_reply(Ok(reply.into_value()))

}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(json: &str) -> WalkFilter {
        WalkFilter::parse(Some(json)).expect("valid kinds_json")
    }

    #[test]
    fn extension_of_is_case_insensitive_and_ignores_missing_dots() {
        assert_eq!(extension_of("Movie.MKV"), "mkv");
        assert_eq!(extension_of("a.b.Mp4"), "mp4");
        assert_eq!(extension_of("no_extension"), "");
        assert_eq!(extension_of("trailing."), "");
        assert_eq!(extension_of(".hidden"), "hidden");
    }

    #[test]
    fn normalize_extension_accepts_dotted_and_upper_case_lists() {
        assert_eq!(normalize_extension(".MP4"), "mp4");
        assert_eq!(normalize_extension(" Flac "), "flac");
        assert_eq!(normalize_extension("."), "");
    }

    #[test]
    fn empty_kinds_json_defaults_to_any() {
        let default = WalkFilter::parse(None).expect("null kinds_json is the default");
        assert_eq!(default.kinds, vec!["any".to_string()]);
        assert!(default.extensions.is_none());
        assert!(default.accepts("anything.bin"));
        assert!(default.accepts("no_extension"));
    }

    #[test]
    fn kinds_are_judged_on_the_built_in_extension_tables() {
        let video = filter(r#"{"kinds":["video"]}"#);
        assert!(video.accepts("movie.mkv"));
        assert!(!video.accepts("song.flac"));
        assert!(!video.accepts("cover.jpg"));

        let audio = filter(r#"{"kinds":["audio"]}"#);
        assert!(audio.accepts("song.FLAC"));
        assert!(!audio.accepts("movie.mp4"));

        let images = filter(r#"{"kinds":["image"]}"#);
        assert!(images.accepts("page.webp"));
        assert!(!images.accepts("book.cbz"));

        let manga = filter(r#"{"kinds":["manga"]}"#);
        assert!(manga.accepts("book.cbz"));
        assert!(!manga.accepts("page.webp"));

        let multi = filter(r#"{"kinds":["audio","video"]}"#);
        assert!(multi.accepts("song.mp3"));
        assert!(multi.accepts("movie.ts"));
        assert!(!multi.accepts("page.png"));
    }

    #[test]
    fn explicit_extension_lists_override_the_built_in_tables() {
        let custom = filter(r#"{"kinds":["video"],"extensions":[".MP4","m4v"]}"#);
        assert!(custom.accepts("movie.mp4"));
        assert!(custom.accepts("clip.m4v"));
        assert!(!custom.accepts("movie.mkv"));

        let empty = filter(r#"{"kinds":["any"],"extensions":[]}"#);
        assert!(!empty.accepts("movie.mp4"));
    }

    #[test]
    fn invalid_kinds_json_is_loud() {
        assert!(WalkFilter::parse(Some("not json")).is_err());
        assert!(WalkFilter::parse(Some("[]")).is_err());
        assert!(WalkFilter::parse(Some(r#"{"kinds":["podcast"]}"#)).is_err());
        assert!(WalkFilter::parse(Some(r#"{"kinds":"video"}"#)).is_err());
        assert!(WalkFilter::parse(Some(r#"{"extensions":"mp4"}"#)).is_err());
    }

    #[test]
    fn directory_budget_counts_listed_directories_and_zero_is_unlimited() {
        let mut unlimited = DirBudget::new(0);
        for _ in 0..1000 {
            assert!(unlimited.take());
        }
        assert_eq!(unlimited.listed, 1000);

        let mut fused = DirBudget::new(2);
        assert!(fused.take());
        assert!(fused.take());
        assert!(!fused.take());
        assert_eq!(fused.listed, 2);
    }

    #[test]
    fn root_and_child_paths_are_normalised() {
        assert_eq!(normalize_root("/music/"), "/music");
        assert_eq!(normalize_root("music"), "music");
        assert_eq!(normalize_root(""), "/");
        assert_eq!(normalize_root("/"), "/");
        assert_eq!(join_path("/", "song.mp3"), "/song.mp3");
        assert_eq!(join_path("music", "song.mp3"), "music/song.mp3");
        assert_eq!(join_path("music/", "song.mp3"), "music/song.mp3");
        assert_eq!(join_path("/music", "song.mp3"), "/music/song.mp3");
    }

    #[test]
    fn limit_reply_carries_the_partial_result() {
        let reply = WalkReply::limited(
            vec![WalkEntry {
                path: "/a.mp4".to_string(),
                name: "a.mp4".to_string(),
                size: 1,
                mtime: Some(1_700_000_000_000),
                is_dir: false,
            }],
            7,
        );
        let value = reply.into_value();
        assert_eq!(value["ok"], serde_json::json!(false));
        assert_eq!(value["error"], serde_json::json!("limit"));
        assert_eq!(value["truncated"], serde_json::json!(true));
        assert_eq!(value["scanned"], serde_json::json!(7));
        assert_eq!(value["entries"][0]["path"], serde_json::json!("/a.mp4"));
        assert_eq!(value["entries"][0]["is_dir"], serde_json::json!(false));
    }

    #[test]
    fn ok_reply_omits_the_error_member() {
        let reply = WalkReply {
            ok: true,
            error: None,
            entries: Vec::new(),
            truncated: false,
            scanned: 1,
        };
        let value = reply.into_value();
        assert_eq!(value["ok"], serde_json::json!(true));
        assert!(value.get("error").is_none());
        assert_eq!(value["scanned"], serde_json::json!(1));
    }
}

