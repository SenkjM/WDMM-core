//! Slice 6: CBZ (ZIP) parsing over remote byte ranges owned by Rust (audit item 6).
//!
//! `wdmm_zip_open(account_id, remote_path)` returns the central directory, and
//! `wdmm_zip_read_entry(handle, index)` returns one entry's bytes. Replaces
//! `lib/utils/cbz_range_zip.dart`, which built its own `Dio` and re-implemented
//! EOCD/central-directory parsing in Dart.
//!
//! Reply contracts (`dart-cutover-slices.md` §Wave 3 Slice 6):
//!
//! ```text
//! wdmm_zip_open        {"ok":true,"handle":"1","size":N,
//!                       "entries":[{"index","name","compressed_size","uncompressed_size","method"}],
//!                       "images":[index,...]}
//! wdmm_zip_read_entry  {"ok":true,"index":n,"name":"...","bytes_b64":"..."}
//! ```
//!
//! `images` lists the entry indices that look like comic pages, ordered by the
//! same natural key as Dart's `mangaPageSortKey` (lower-cased name, every digit
//! run left-padded to eight characters) so the reader shows the same page order
//! it always had. The key itself is implemented here (and unit-tested below).
//!
//! Bytes come from `crate::read_account_range` / the account driver, read in
//! 64 KiB blocks with a small LRU (≈4 MiB), because a `zip` central directory is
//! walked with many tiny seeks — one HTTP request per byte would be unusable.
//! A server that ignores `Range` is reported as `error:range_unsupported` so the
//! Dart caller keeps its existing "cache the whole archive" fallback.
//!
//! Local archives are supported through the same reader (empty `account_id`
//! means "`remote_path` is a local file path"), so a cached CBZ is still read
//! page by page without loading it whole.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use base64::prelude::*;
use wdmm_drivers::{DriverError, WdmmVfs};
use zip::{CompressionMethod, ZipArchive};

use super::{json_reply, parse_u64_arg, require_arg};
use crate::{ptr_to_str, rt, FfiString};

/// Block size used for the remote range cache.
const BLOCK_SIZE: u64 = 64 * 1024;
/// How many blocks stay cached per open archive (≈4 MiB).
const MAX_CACHED_BLOCKS: usize = 64;
/// How many archives stay open; handles are evicted oldest-first because the
/// slice contract has no explicit `close` symbol.
const MAX_OPEN_ARCHIVES: usize = 4;
/// Refuse to materialise absurdly large entries into a single base64 reply.
const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// Page suffixes the reader shows, mirroring `kMangaImageExtensions` on the Dart
/// side (`lib/utils/manga_file_types.dart`).
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "avif"];

/// Byte provider behind [`RangeReader`]: a local file or a remote account file.
trait ByteSource: Send {
    /// Total length of the underlying file.
    fn size(&self) -> u64;
    /// Read `[start, end)` — callers never ask beyond [`ByteSource::size`].
    fn read_at(&mut self, start: u64, end: u64) -> Result<Vec<u8>, String>;
}

/// Local cached archive.
struct LocalSource {
    file: File,
    size: u64,
}

impl ByteSource for LocalSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, start: u64, end: u64) -> Result<Vec<u8>, String> {
        self.file
            .seek(SeekFrom::Start(start))
            .map_err(|e| format!("local seek: {e}"))?;
        let want = (end - start) as usize;
        let mut buf = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            match self.file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => return Err(format!("local read: {e}")),
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }
}

/// Remote archive read through the account driver.
struct RemoteSource {
    driver: Arc<dyn WdmmVfs>,
    account_id: String,
    path: String,
    size: u64,
}

impl ByteSource for RemoteSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, start: u64, end: u64) -> Result<Vec<u8>, String> {
        let want = (end - start) as usize;
        let end_inclusive = end.saturating_sub(1);
        let result = rt().block_on(WdmmVfs::get_content_range(
            &*self.driver,
            &self.path,
            start,
            Some(end_inclusive),
        ));
        let data = match result {
            Ok(data) => data,
            // One token refresh, then rebuild the driver so later blocks use it.
            Err(DriverError::AuthFailed) => {
                let data =
                    crate::read_account_range(&self.account_id, &self.path, start, Some(end_inclusive))?;
                if let Ok(driver) = crate::driver_for_account(&self.account_id) {
                    self.driver = driver;
                }
                data
            }
            Err(e) => return Err(e.to_string()),
        };
        if data.len() > want {
            // Server answered 200 with the whole file: Range is not honoured.
            return Err("range_unsupported".to_string());
        }
        if data.len() != want {
            return Err(format!("range_short: {} of {} bytes", data.len(), want));
        }
        Ok(data)
    }
}

/// `Read + Seek` adapter over a [`ByteSource`] with a block cache.
struct RangeReader {
    source: Box<dyn ByteSource>,
    pos: u64,
    size: u64,
    blocks: HashMap<u64, Vec<u8>>,
    /// Insertion order of cached blocks, oldest first.
    order: Vec<u64>,
}

impl RangeReader {
    fn new(source: Box<dyn ByteSource>) -> Self {
        let size = source.size();
        Self {
            source,
            pos: 0,
            size,
            blocks: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn ensure_block(&mut self, index: u64) -> io::Result<()> {
        if self.blocks.contains_key(&index) {
            return Ok(());
        }
        let start = index * BLOCK_SIZE;
        if start >= self.size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "zip block past end of file",
            ));
        }
        let end = (start + BLOCK_SIZE).min(self.size);
        let data = self
            .source
            .read_at(start, end)
            .map_err(io::Error::other)?;
        if data.len() as u64 != end - start {
            return Err(io::Error::other(format!(
                "zip block short: {} of {} bytes",
                data.len(),
                end - start
            )));
        }
        if self.blocks.len() >= MAX_CACHED_BLOCKS && !self.order.is_empty() {
            let oldest = self.order.remove(0);
            self.blocks.remove(&oldest);
        }
        self.blocks.insert(index, data);
        self.order.push(index);
        Ok(())
    }

    fn cached(&self, index: u64) -> &[u8] {
        self.blocks
            .get(&index)
            .map(|block| block.as_slice())
            .unwrap_or(&[])
    }
}

impl Read for RangeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        let want = (self.size - self.pos).min(buf.len() as u64) as usize;
        let mut done = 0;
        while done < want {
            let abs = self.pos + done as u64;
            let index = abs / BLOCK_SIZE;
            self.ensure_block(index)?;
            let offset = (abs % BLOCK_SIZE) as usize;
            let block = self.cached(index);
            let n = (want - done).min(block.len().saturating_sub(offset));
            if n == 0 {
                break;
            }
            buf[done..done + n].copy_from_slice(&block[offset..offset + n]);
            done += n;
        }
        self.pos += done as u64;
        Ok(done)
    }
}

impl Seek for RangeReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let next = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.size as i64 + offset,
            SeekFrom::Current(offset) => self.pos as i64 + offset,
        };
        if next < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zip seek before start",
            ));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

/// One central-directory entry as reported to Dart.
#[derive(Clone, Debug)]
struct EntryMeta {
    index: usize,
    name: String,
    compressed_size: u64,
    uncompressed_size: u64,
    method: u16,
    is_dir: bool,
}

impl EntryMeta {
    fn as_value(&self) -> serde_json::Value {
        serde_json::json!({
            "index": self.index,
            "name": self.name,
            "compressed_size": self.compressed_size,
            "uncompressed_size": self.uncompressed_size,
            "method": self.method,
        })
    }
}

/// An archive kept open so `read_entry` can reuse the central directory.
struct OpenArchive {
    size: u64,
    entries: Vec<EntryMeta>,
    /// Indices into `entries`, in comic-page order.
    images: Vec<usize>,
    archive: Mutex<ZipArchive<RangeReader>>,
}

fn registry() -> &'static Mutex<HashMap<u64, Arc<OpenArchive>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, Arc<OpenArchive>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_handle() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn register(archive: Arc<OpenArchive>) -> u64 {
    let handle = next_handle();
    let mut map = registry().lock().unwrap();
    // Oldest handle is the smallest id; evict until there is room.
    while map.len() >= MAX_OPEN_ARCHIVES {
        match map.keys().min().copied() {
            Some(oldest) => {
                map.remove(&oldest);
            }
            None => break,
        }
    }
    map.insert(handle, archive);
    handle
}

/// Natural page key, byte-for-byte the same rule as Dart's `mangaPageSortKey`:
/// lower-case the name, then left-pad every digit run with zeros to width 8.
fn manga_page_sort_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    let mut digits = String::new();
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        if !digits.is_empty() {
            push_padded(&mut out, &digits);
            digits.clear();
        }
        out.push(ch);
    }
    if !digits.is_empty() {
        push_padded(&mut out, &digits);
    }
    out
}

fn push_padded(out: &mut String, digits: &str) {
    if digits.len() < 8 {
        for _ in 0..8 - digits.len() {
            out.push('0');
        }
    }
    out.push_str(digits);
}

fn extension_of(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

fn is_image_entry(entry: &EntryMeta) -> bool {
    if entry.is_dir {
        return false;
    }
    IMAGE_EXTENSIONS.contains(&extension_of(&entry.name).as_str())
}

/// Zip "compression method" number (0 = store, 8 = deflate). Anything else is
/// reported as 99 ("other"): only `store`/`deflate` are enabled for reading, and
/// the Dart side treats the field as informational.
fn compression_code(method: CompressionMethod) -> u16 {
    if method == CompressionMethod::Stored {
        0
    } else if method == CompressionMethod::Deflated {
        8
    } else {
        99
    }
}

/// Comic pages among `entries`, in natural page order (indices into `entries`).
fn image_indices(entries: &[EntryMeta]) -> Vec<usize> {
    let mut images: Vec<usize> = entries
        .iter()
        .filter(|entry| is_image_entry(entry))
        .map(|entry| entry.index)
        .collect();
    images.sort_by(|a, b| {
        manga_page_sort_key(&entries[*a].name)
            .cmp(&manga_page_sort_key(&entries[*b].name))
            .then_with(|| a.cmp(b))
    });
    images
}

/// Read the central directory and pick out the comic pages.
///
/// Every central-directory slot gets an [`EntryMeta`], even one whose metadata
/// cannot be decoded (empty name, method 99, flagged as a directory). Keeping
/// the vector dense means `EntryMeta::index` is both the `zip` CD index and the
/// position in the list Dart receives, so `images` indices address the same
/// array on both sides.
fn list_entries(archive: &ZipArchive<RangeReader>) -> (Vec<EntryMeta>, Vec<usize>) {
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        match archive.by_index_data(index) {
            Ok(file) => {
                let raw_name = file.name_raw();
                let name = String::from_utf8_lossy(raw_name)
                    .into_owned()
                    .replace('\\', "/");
                let is_dir = file.is_dir() || name.ends_with('/');
                entries.push(EntryMeta {
                    index,
                    name,
                    compressed_size: file.compressed_size(),
                    uncompressed_size: file.size(),
                    method: compression_code(file.compression()),
                    is_dir,
                });
            }
            Err(_) => entries.push(EntryMeta {
                index,
                name: String::new(),
                compressed_size: 0,
                uncompressed_size: 0,
                method: 99,
                is_dir: true,
            }),
        }
    }
    let images = image_indices(&entries);
    (entries, images)
}

fn take_ffi_string(value: FfiString) -> String {
    let text = unsafe { CStr::from_ptr(value.ptr) }
        .to_string_lossy()
        .into_owned();
    crate::wdmm_ffi_free_string(value);
    text
}

/// Remote file size: reuses the account entry FFI so the token-refresh path is
/// the same one every other account call takes.
fn remote_size(account_id: &str, path: &str) -> Result<u64, String> {
    let account = CString::new(account_id).map_err(|_| "invalid account_id".to_string())?;
    let remote = CString::new(path).map_err(|_| "invalid remote_path".to_string())?;
    let reply = crate::wdmm_account_get_entry(account.as_ptr(), remote.as_ptr());
    let text = take_ffi_string(reply);
    if let Some(error) = text.strip_prefix("error:") {
        return Err(error.trim().to_string());
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("entry json: {e}"))?;
    value
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .filter(|size| *size > 0)
        .ok_or_else(|| "empty archive".to_string())
}

fn open_archive(account_id: &str, remote_path: &str) -> Result<Arc<OpenArchive>, String> {
    let (reader, size) = if account_id.is_empty() {
        let file = File::open(remote_path).map_err(|e| format!("open local: {e}"))?;
        let size = file
            .metadata()
            .map_err(|e| format!("local stat: {e}"))?
            .len();
        (RangeReader::new(Box::new(LocalSource { file, size })), size)
    } else {
        let size = remote_size(account_id, remote_path)?;
        let driver = crate::driver_for_account(account_id).map_err(take_ffi_string)?;
        let mut source = RemoteSource {
            driver,
            account_id: account_id.to_string(),
            path: remote_path.to_string(),
            size,
        };
        // Fail fast (and cheaply) when Range is not honoured.
        source.read_at(0, 1)?;
        (RangeReader::new(Box::new(source)), size)
    };
    if size < 22 {
        return Err("zip too small".to_string());
    }
    let archive = ZipArchive::new(reader).map_err(|e| format!("zip: {e}"))?;
    let (entries, images) = list_entries(&archive);
    Ok(Arc::new(OpenArchive {
        size,
        entries,
        images,
        archive: Mutex::new(archive),
    }))
}

/// Decompress one entry. Shared by the FFI edge and the local-file unit test.
fn read_entry_bytes(archive: &OpenArchive, index: usize) -> Result<Vec<u8>, String> {
    let meta = archive
        .entries
        .get(index)
        .ok_or_else(|| "index out of range".to_string())?;
    if meta.uncompressed_size > MAX_ENTRY_BYTES {
        return Err("entry too large".to_string());
    }
    let mut zip = archive.archive.lock().unwrap();
    let mut file = zip
        .by_index(index)
        .map_err(|e| format!("zip entry: {e}"))?;
    let mut data = Vec::with_capacity(meta.uncompressed_size as usize);
    io::copy(&mut file, &mut data).map_err(|e| format!("zip entry read: {e}"))?;
    Ok(data)
}

fn entries_reply(
    handle: u64,
    size: u64,
    entries: &[EntryMeta],
    images: &[usize],
) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "handle": handle.to_string(),
        "size": size,
        "entries": entries.iter().map(EntryMeta::as_value).collect::<Vec<_>>(),
        "images": images,
    })
}

/// Open a CBZ/ZIP archive and cache its central directory.
///
/// `account_id` empty means `remote_path` is a local file path (the cached-CBZ
/// path). Returns the JSON documented at the top of this module, or
/// `error:range_unsupported` when the remote ignores HTTP Range.
#[no_mangle]
pub extern "C" fn wdmm_zip_open(
    account_id: *const c_char,
    remote_path: *const c_char,
) -> FfiString {
    let account_id = ptr_to_str(account_id).unwrap_or_default();
    let remote_path = match require_arg(remote_path, "remote_path") {
        Ok(path) => path,
        Err(error) => return error,
    };
    match open_archive(account_id, remote_path) {
        Ok(archive) => {
            let handle = register(Arc::clone(&archive));
            json_reply(Ok(entries_reply(
                handle,
                archive.size,
                &archive.entries,
                &archive.images,
            )))
        }
        Err(error) => json_reply(Err(error)),
    }
}

/// Read one entry of a previously opened archive, base64-encoded.
#[no_mangle]
pub extern "C" fn wdmm_zip_read_entry(handle: *const c_char, index: *const c_char) -> FfiString {
    let handle_id = match parse_u64_arg(handle, "handle") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let index = match parse_u64_arg(index, "index") {
        Ok(value) => value as usize,
        Err(error) => return error,
    };
    let archive = match registry().lock().unwrap().get(&handle_id).cloned() {
        Some(archive) => archive,
        None => return FfiString::from("error:handle not found"),
    };
    let result = read_entry_bytes(&archive, index).map(|data| {
        serde_json::json!({
            "ok": true,
            "index": index,
            "name": archive.entries[index].name,
            "bytes_b64": BASE64_STANDARD.encode(&data),
        })
    });
    json_reply(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory [`ByteSource`] so the block cache can be tested without I/O.
    struct MemorySource {
        data: Vec<u8>,
    }

    impl ByteSource for MemorySource {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }

        fn read_at(&mut self, start: u64, end: u64) -> Result<Vec<u8>, String> {
            let start = start as usize;
            let end = (end as usize).min(self.data.len());
            if start >= end {
                return Ok(Vec::new());
            }
            Ok(self.data[start..end].to_vec())
        }
    }

    fn sample_entries() -> Vec<EntryMeta> {
        let mut entries = Vec::new();
        for (index, name) in [
            (0, "OEBPS/Images/"),
            (1, "OEBPS/Images/page10.png"),
            (2, "OEBPS/Images/page2.png"),
            (3, "OEBPS/Images/Cover.PNG"),
            (4, "OEBPS/Images/p1-a1.jpg"),
            (5, "OEBPS/Images/P2-A2.jpg"),
            (6, "ComicInfo.xml"),
            (7, "OEBPS/Images/notes.txt"),
        ] {
            entries.push(EntryMeta {
                index,
                name: name.to_string(),
                compressed_size: 10,
                uncompressed_size: 20,
                method: 8,
                is_dir: name.ends_with('/'),
            });
        }
        entries
    }

    #[test]
    fn manga_page_sort_key_pads_every_digit_run() {
        // Same rule as Dart's `mangaPageSortKey`.
        assert_eq!(manga_page_sort_key("p1.png"), "p00000001.png");
        assert_eq!(manga_page_sort_key("P12.png"), "p00000012.png");
        assert_eq!(
            manga_page_sort_key("vol2/ch1/p3.jpg"),
            "vol00000002/ch00000001/p00000003.jpg"
        );
        // Already-wide numbers stay as they are.
        assert_eq!(manga_page_sort_key("p123456789.png"), "p123456789.png");
    }

    #[test]
    fn natural_order_puts_page_2_before_page_10() {
        let mut names = vec!["page10.png", "page2.png", "cover.png"];
        names.sort_by_key(|name| manga_page_sort_key(name));
        assert_eq!(names, vec!["cover.png", "page2.png", "page10.png"]);
    }

    #[test]
    fn image_indices_skip_directories_and_non_images() {
        let entries = sample_entries();
        let images = image_indices(&entries);
        // 0 is a directory, 6 ComicInfo.xml and 7 notes.txt are not pages.
        assert_eq!(images, vec![3, 4, 5, 2, 1]);
    }

    #[test]
    fn open_reply_carries_handle_size_entries_and_images() {
        let entries = sample_entries();
        let images = vec![3, 4];
        let reply = entries_reply(42, 1234, &entries, &images);
        assert_eq!(reply["ok"], serde_json::json!(true));
        assert_eq!(reply["handle"], serde_json::json!("42"));
        assert_eq!(reply["size"], serde_json::json!(1234));
        assert_eq!(reply["images"], serde_json::json!([3, 4]));
        let first = &reply["entries"][0];
        assert_eq!(first["index"], serde_json::json!(0));
        assert_eq!(first["name"], serde_json::json!("OEBPS/Images/"));
        assert_eq!(first["compressed_size"], serde_json::json!(10));
        assert_eq!(first["uncompressed_size"], serde_json::json!(20));
        assert_eq!(first["method"], serde_json::json!(8));
    }

    #[test]
    fn range_reader_serves_seeks_across_block_boundaries() {
        let total = (BLOCK_SIZE * 3 + 17) as usize;
        let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let mut reader = RangeReader::new(Box::new(MemorySource {
            data: data.clone(),
        }));
        assert_eq!(reader.seek(SeekFrom::End(0)).unwrap(), total as u64);

        // Tail read spans the last two blocks.
        let mut tail = [0u8; 22];
        reader.seek(SeekFrom::End(-22)).unwrap();
        reader.read_exact(&mut tail).unwrap();
        assert_eq!(&tail[..], &data[total - 22..]);

        // A read straddling a block boundary.
        let mut straddle = [0u8; 40];
        reader.seek(SeekFrom::Start(BLOCK_SIZE - 20)).unwrap();
        reader.read_exact(&mut straddle).unwrap();
        let start = (BLOCK_SIZE - 20) as usize;
        assert_eq!(&straddle[..], &data[start..start + 40]);

        // Reading past the end yields EOF, not an error.
        reader.seek(SeekFrom::Start(total as u64 - 1)).unwrap();
        let mut last = [0u8; 8];
        assert_eq!(reader.read(&mut last).unwrap(), 1);
    }

    #[test]
    fn local_zip_roundtrip_lists_and_reads_entries() {
        use std::io::Write;

        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        let path = std::env::temp_dir().join(format!(
            "wdmm_zip_slice6_{}_{}.cbz",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let file = File::create(&path).unwrap();
            let mut writer = ZipWriter::new(file);
            writer
                .add_directory("images/", SimpleFileOptions::default())
                .unwrap();
            for (name, payload) in [
                ("images/p2.png", b"two".to_vec()),
                ("images/p10.png", b"ten".to_vec()),
                ("images/p1.jpg", b"one".to_vec()),
                ("ComicInfo.xml", b"<ComicInfo><Title>t</Title></ComicInfo>".to_vec()),
            ] {
                let options = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated);
                writer.start_file(name, options).unwrap();
                writer.write_all(&payload).unwrap();
            }
            writer.finish().unwrap();
        }

        let archive = open_archive("", path.to_str().unwrap()).unwrap();
        assert!(archive.size > 0);
        let names: Vec<&str> = archive
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert!(names.contains(&"images/p2.png"));
        assert!(names.contains(&"images/p10.png"));
        assert_eq!(archive.images.len(), 3);
        // Natural order: p1.jpg, p2.png, p10.png.
        let page_names: Vec<&str> = archive
            .images
            .iter()
            .map(|index| archive.entries[*index].name.as_str())
            .collect();
        assert_eq!(page_names, vec!["images/p1.jpg", "images/p2.png", "images/p10.png"]);

        let cover = archive.images[0];
        assert_eq!(read_entry_bytes(&archive, cover).unwrap(), b"one");
        let ten = archive.images[2];
        assert_eq!(read_entry_bytes(&archive, ten).unwrap(), b"ten");

        let _ = std::fs::remove_file(&path);
    }
}
