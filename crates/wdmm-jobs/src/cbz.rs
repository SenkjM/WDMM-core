//! CBZ (ZIP) helpers: write archives for manga pack/build and read ComicInfo.xml
//! from existing archives.
//!
//! The write side fixes every entry timestamp to 1980-01-01 00:00:00 (the ZIP
//! epoch, which is also `zip::DateTime::default()`) so re-packing the same pages
//! produces a byte-identical archive. That is what makes pack output
//! reproducible (and keeps conditional uploads / content comparisons stable).
//!
//! The read side is a small in-memory port of the Dart `CbzRangeZip` surface
//! (`lib/utils/cbz_range_zip.dart`). It does not perform HTTP range reads — the
//! caller supplies the bytes (already-cached archive, or a full
//! `WdmmVfs::get_content` when the driver has no range support).

use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::read::ZipArchive;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

/// Canonical ComicInfo entry name. Dart compares basenames case-insensitively.
pub const COMIC_INFO_ENTRY: &str = "ComicInfo.xml";

/// Image extensions understood as comic pages (Dart `kMangaImageExtensions`).
pub const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "avif"];

/// Subset of the ComicInfo.xml schema the old Dart parser exposes.
///
/// `writer` is the display credit: `Writer`, else `Penciller`, else `Author`
/// (same fallback as `ComicInfoMeta.parse`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComicInfo {
    pub title: Option<String>,
    pub series: Option<String>,
    pub writer: Option<String>,
    pub volume: Option<String>,
    pub number: Option<String>,
}

impl ComicInfo {
    /// True when no field the parser exposes is present.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.series.is_none()
            && self.writer.is_none()
            && self.volume.is_none()
            && self.number.is_none()
    }

    /// Serialise to a minimal, deterministic ComicInfo.xml document.
    ///
    /// Only present fields are written, in a stable order.
    pub fn to_xml(&self) -> String {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<ComicInfo>\n");
        if let Some(v) = &self.series {
            push_xml_field(&mut xml, "Series", v);
        }
        if let Some(v) = &self.title {
            push_xml_field(&mut xml, "Title", v);
        }
        if let Some(v) = &self.number {
            push_xml_field(&mut xml, "Number", v);
        }
        if let Some(v) = &self.volume {
            push_xml_field(&mut xml, "Volume", v);
        }
        if let Some(v) = &self.writer {
            push_xml_field(&mut xml, "Writer", v);
        }
        xml.push_str("</ComicInfo>\n");
        xml
    }
}

fn push_xml_field(out: &mut String, name: &str, value: &str) {
    out.push_str("  <");
    out.push_str(name);
    out.push('>');
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out.push_str("</");
    out.push_str(name);
    out.push_str(">\n");
}

/// Parse ComicInfo.xml. Returns `None` when no exposed field is present.
///
/// Text content is trimmed and empty values are dropped, matching the Dart
/// regex parser. `Writer` takes precedence over `Penciller`/`Author`
/// regardless of document order.
pub fn parse_comic_info(xml: &str) -> Option<ComicInfo> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut info = ComicInfo::default();
    let mut writer: Option<String> = None;
    let mut penciller: Option<String> = None;
    let mut author: Option<String> = None;
    let mut current: Option<String> = None;
    let mut text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current = Some(String::from_utf8_lossy(e.name().as_ref()).to_ascii_lowercase());
                text.clear();
            }
            Ok(Event::Text(e)) => {
                if current.is_some() {
                    if let Ok(decoded) = e.unescape() {
                        text.push_str(decoded.as_ref());
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if current.is_some() {
                    text.push_str(&String::from_utf8_lossy(e.as_ref()));
                }
            }
            Ok(Event::End(_)) => {
                if let Some(name) = current.take() {
                    let value = text.trim();
                    if !value.is_empty() {
                        match name.as_str() {
                            "title" => set_if_none(&mut info.title, value),
                            "series" => set_if_none(&mut info.series, value),
                            "volume" => set_if_none(&mut info.volume, value),
                            "number" => set_if_none(&mut info.number, value),
                            "writer" => set_if_none(&mut writer, value),
                            "penciller" => set_if_none(&mut penciller, value),
                            "author" => set_if_none(&mut author, value),
                            _ => {}
                        }
                    }
                    text.clear();
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }

    info.writer = writer.or(penciller).or(author);
    if info.is_empty() {
        None
    } else {
        Some(info)
    }
}

fn set_if_none(slot: &mut Option<String>, value: &str) {
    if slot.is_none() {
        *slot = Some(value.to_string());
    }
}

/// One entry of a CBZ archive, in central-directory order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CbzEntry {
    /// Index to pass back to [`CbzArchive::read_entry`].
    pub index: usize,
    /// Full entry name as stored in the archive (may contain `/`).
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    /// True when the basename carries a known comic image extension.
    pub is_image: bool,
}

impl CbzEntry {
    pub fn is_comic_info(&self) -> bool {
        entry_basename(&self.name).eq_ignore_ascii_case(COMIC_INFO_ENTRY)
    }
}

/// A parsed CBZ kept in memory.
pub struct CbzArchive {
    inner: ZipArchive<Cursor<Vec<u8>>>,
    entries: Vec<CbzEntry>,
}

impl CbzArchive {
    pub fn open_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        let mut inner =
            ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("err.cbzCorrupt|{e}"))?;
        let mut entries = Vec::with_capacity(inner.len());
        for index in 0..inner.len() {
            let file = inner
                .by_index(index)
                .map_err(|e| format!("err.cbzCorrupt|{e}"))?;
            let name = file.name().map(|n| n.to_string()).unwrap_or_default();
            entries.push(CbzEntry {
                index,
                is_image: is_image_name(&name),
                is_dir: file.is_dir(),
                size: file.size(),
                name,
            });
        }
        Ok(Self { inner, entries })
    }

    pub fn entries(&self) -> &[CbzEntry] {
        &self.entries
    }

    /// Read one entry's uncompressed bytes by its archive index.
    pub fn read_entry(&mut self, index: usize) -> Result<Vec<u8>, String> {
        let mut file = self
            .inner
            .by_index(index)
            .map_err(|e| format!("err.cbzCorrupt|{e}"))?;
        let mut buf = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut buf)
            .map_err(|e| format!("err.cbzRead|{e}"))?;
        Ok(buf)
    }

    /// Parse the first `ComicInfo.xml` entry found (basename, case-insensitive).
    pub fn comic_info(&mut self) -> Option<ComicInfo> {
        let index = self.entries.iter().find(|e| e.is_comic_info())?.index;
        let bytes = self.read_entry(index).ok()?;
        parse_comic_info(&String::from_utf8_lossy(&bytes))
    }

    /// Image entry indices in natural page order (Dart `mangaPageSortKey`).
    pub fn sorted_image_indices(&self) -> Vec<usize> {
        let mut images: Vec<&CbzEntry> = self.entries.iter().filter(|e| e.is_image).collect();
        images.sort_by(|a, b| {
            page_sort_key(&a.name)
                .cmp(&page_sort_key(&b.name))
                .then_with(|| a.name.cmp(&b.name))
        });
        images.iter().map(|e| e.index).collect()
    }

    /// Image entry names in natural page order.
    pub fn sorted_image_names(&self) -> Vec<String> {
        self.sorted_image_indices()
            .into_iter()
            .filter_map(|i| self.entries.get(i).map(|e| e.name.clone()))
            .collect()
    }
}

/// One page to write into an archive.
#[derive(Debug, Clone)]
pub struct PackPage {
    pub name: String,
    pub data: Vec<u8>,
}

/// Streaming CBZ writer with the deterministic timestamp baked in.
pub struct CbzWriter {
    writer: ZipWriter<File>,
    path: PathBuf,
}

impl CbzWriter {
    pub fn create(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("err.io|{e}"))?;
        }
        let file = File::create(path).map_err(|e| format!("err.io|{e}"))?;
        Ok(Self {
            writer: ZipWriter::new(file),
            path: path.to_path_buf(),
        })
    }

    /// Add one entry. `name` is used verbatim; callers own the page naming
    /// convention (`%05d_<basename>`).
    pub fn add_entry(&mut self, name: &str, data: &[u8]) -> Result<(), String> {
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(DateTime::default());
        self.writer
            .start_file(name, options)
            .map_err(|e| format!("err.cbzWrite|{e}"))?;
        self.writer
            .write_all(data)
            .map_err(|e| format!("err.cbzWrite|{e}"))?;
        Ok(())
    }

    /// Finalise the central directory and return the archive path.
    pub fn finish(self) -> Result<PathBuf, String> {
        let path = self.path.clone();
        self.writer
            .finish()
            .map_err(|e| format!("err.cbzWrite|{e}"))?;
        Ok(path)
    }
}

/// Write a complete CBZ from in-memory pages, plus an optional ComicInfo.xml.
///
/// An empty archive is rejected (`err.manga.noPages`): a CBZ with zero pages is
/// never valid for this app (Dart `_runMangaPack` fails "no pages to pack").
pub fn write_cbz(
    path: &Path,
    pages: &[PackPage],
    comic_info: Option<&ComicInfo>,
) -> Result<(), String> {
    if pages.is_empty() {
        return Err("err.manga.noPages".into());
    }
    let mut writer = CbzWriter::create(path)?;
    for page in pages {
        writer.add_entry(&page.name, &page.data)?;
    }
    if let Some(info) = comic_info {
        writer.add_entry(COMIC_INFO_ENTRY, info.to_xml().as_bytes())?;
    }
    writer.finish()?;
    Ok(())
}

/// Read a CBZ from disk (used when the archive is already in the manga cache).
pub fn open_cbz_path(path: &Path) -> Result<CbzArchive, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("err.io|{e}"))?;
    CbzArchive::open_bytes(bytes)
}

/// True when the basename carries a known comic image extension.
pub fn is_image_name(name: &str) -> bool {
    let base = entry_basename(name);
    let Some((_, ext)) = base.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    IMAGE_EXTENSIONS.iter().any(|known| *known == ext)
}

/// Last path segment of a ZIP/remote path, treating `\` like `/`.
pub fn entry_basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// Natural page ordering key (Dart `mangaPageSortKey`).
///
/// Lowercase the name, then left-pad every digit run to eight characters, so
/// `2.png` sorts before `10.png` while comparison stays byte-wise.
pub fn page_sort_key(name: &str) -> String {
    let lowered = name.to_ascii_lowercase();
    let mut out = String::with_capacity(lowered.len() + 8);
    let mut digits = String::new();
    for ch in lowered.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            if !digits.is_empty() {
                out.push_str(&format!("{digits:0>8}"));
                digits.clear();
            }
            out.push(ch);
        }
    }
    if !digits.is_empty() {
        out.push_str(&format!("{digits:0>8}"));
    }
    out
}

/// Sort entries in place by natural page order (name as tie-break).
pub fn sort_page_entries(entries: &mut [CbzEntry]) {
    entries.sort_by(|a, b| {
        page_sort_key(&a.name)
            .cmp(&page_sort_key(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(name: &str, byte: u8) -> PackPage {
        PackPage {
            name: name.into(),
            data: vec![byte; 64],
        }
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.cbz");
        let info = ComicInfo {
            title: Some("Chapter 1".into()),
            series: Some("Series".into()),
            writer: Some("Author".into()),
            volume: Some("2".into()),
            number: Some("7".into()),
        };
        write_cbz(
            &path,
            &[
                page("00001_1.png", 1),
                page("00002_2.png", 2),
                page("00003_3.png", 3),
            ],
            Some(&info),
        )
        .unwrap();

        let mut archive = open_cbz_path(&path).unwrap();
        let names: Vec<&str> = archive.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["00001_1.png", "00002_2.png", "00003_3.png", COMIC_INFO_ENTRY]
        );
        assert_eq!(archive.read_entry(1).unwrap(), vec![2u8; 64]);
        let parsed = archive.comic_info().expect("comic info");
        assert_eq!(parsed.title.as_deref(), Some("Chapter 1"));
        assert_eq!(parsed.series.as_deref(), Some("Series"));
        assert_eq!(parsed.writer.as_deref(), Some("Author"));
        assert_eq!(parsed.volume.as_deref(), Some("2"));
        assert_eq!(parsed.number.as_deref(), Some("7"));
    }

    #[test]
    fn empty_pages_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = write_cbz(&dir.path().join("empty.cbz"), &[], None).unwrap_err();
        assert_eq!(err, "err.manga.noPages");
    }

    #[test]
    fn archive_is_byte_identical_across_writes() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.cbz");
        let b = dir.path().join("b.cbz");
        let pages = [page("00001_1.png", 7), page("00002_2.png", 8)];
        write_cbz(&a, &pages, None).unwrap();
        write_cbz(&b, &pages, None).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    }

    #[test]
    fn image_entries_sort_naturally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("order.cbz");
        // Page names are not zero-padded, so archive order differs from natural order.
        write_cbz(
            &path,
            &[page("10.png", 1), page("2.png", 2), page("1.png", 3)],
            None,
        )
        .unwrap();
        let archive = open_cbz_path(&path).unwrap();
        assert_eq!(archive.sorted_image_names(), ["1.png", "2.png", "10.png"]);
    }

    #[test]
    fn comic_info_parser_handles_penciller_fallback_and_cdata() {
        let xml = r#"<?xml version="1.0"?>
<ComicInfo>
  <Title><![CDATA[The Title]]></Title>
  <Penciller>Pat</Penciller>
  <Number>3</Number>
</ComicInfo>"#;
        let info = parse_comic_info(xml).expect("parsed");
        assert_eq!(info.title.as_deref(), Some("The Title"));
        assert_eq!(info.writer.as_deref(), Some("Pat"));
        assert_eq!(info.number.as_deref(), Some("3"));
        assert_eq!(info.series, None);
    }

    #[test]
    fn comic_info_prefers_writer_over_penciller() {
        let xml = "<ComicInfo><Penciller>Pat</Penciller><Writer>Wen</Writer></ComicInfo>";
        let info = parse_comic_info(xml).expect("parsed");
        assert_eq!(info.writer.as_deref(), Some("Wen"));
    }

    #[test]
    fn comic_info_parser_returns_none_without_known_fields() {
        assert!(parse_comic_info("<ComicInfo><Pages>3</Pages></ComicInfo>").is_none());
        assert!(parse_comic_info("not xml at all").is_none());
    }

    #[test]
    fn page_sort_key_pads_digit_runs() {
        assert_eq!(page_sort_key("Page2.png"), "page00000002.png");
        assert!(page_sort_key("p2.png") < page_sort_key("p10.png"));
        assert!(page_sort_key("a/b9.png") < page_sort_key("a/b10.png"));
    }

    #[test]
    fn basename_handles_backslashes() {
        assert_eq!(entry_basename("a\\b\\c.png"), "c.png");
        assert!(is_image_name("dir/PAGE.JPEG"));
        assert!(!is_image_name("ComicInfo.xml"));
    }
}
