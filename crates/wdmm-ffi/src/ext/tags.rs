//! Slice: embedded tag/cover extraction owned by Rust (audit item 8).
//!
//! `wdmm_tags_extract(account_id, remote_path)` lets [`lofty`] probe a remote
//! file through a byte-range reader: every read goes through
//! [`crate::read_account_range`], so credentials, token refresh, and HTTP Range
//! handling stay in Rust. Replaces the hand-rolled Dio range requests, sparse
//! temp files, and container parsers that lived in
//! `lib/services/stream_cover_reader.dart`.
//!
//! Reply on success:
//! `{"ok":true,"format":"mp3","tags":{…},"duration_ms":n,"bitrate":n,"sample_rate":n,"cover":{…}|null}`.
//! `bitrate` is bits per second (Dart's `ReadTags` unit); `cover` carries
//! `{"mime","bytes_b64","width","height","description"}`. Failures use the
//! non-session FFI convention: `error:…`.

use std::io::{self, Read, Seek, SeekFrom};
use std::os::raw::c_char;

use base64::Engine as _;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::picture::{Picture, PictureInformation, PictureType};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};

use super::{json_reply, require_arg};
use crate::{driver_for_account, read_account_range, rt, FfiString};

/// Byte-range block size for the remote `Read + Seek` adapter. Big enough that
/// ID3/FLAC/MP4 probing needs a handful of requests, small enough to stay cheap
/// over mobile connections.
const READ_BLOCK: u64 = 256 * 1024;

/// Embedded covers larger than this are dropped (the UI never displayed them,
/// the old Dart path capped at the same size).
const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;

/// Minimal byte-range source used by [`RemoteReader`].
trait RangeFetcher {
    /// Fetch `start..=end_inclusive`.
    fn fetch(&mut self, start: u64, end_inclusive: u64) -> Result<Vec<u8>, String>;
}

/// Fetcher backed by the account driver (`read_account_range` refreshes the
/// token once on `AuthFailed`, so this stays the single network entry point).
struct AccountRange {
    account_id: String,
    remote_path: String,
}

impl RangeFetcher for AccountRange {
    fn fetch(&mut self, start: u64, end_inclusive: u64) -> Result<Vec<u8>, String> {
        read_account_range(
            &self.account_id,
            &self.remote_path,
            start,
            Some(end_inclusive),
        )
    }
}

/// `Read + Seek` view over a remote file, with a block cache so `lofty`'s many
/// small seeks do not turn into one request each.
struct RemoteReader<F: RangeFetcher> {
    fetcher: F,
    size: u64,
    pos: u64,
    block_start: u64,
    block: Vec<u8>,
}

impl<F: RangeFetcher> RemoteReader<F> {
    fn new(fetcher: F, size: u64) -> Self {
        Self {
            fetcher,
            size,
            pos: 0,
            block_start: 0,
            block: Vec::new(),
        }
    }

    /// Make sure [`Self::pos`] is covered by [`Self::block`].
    fn ensure_block(&mut self) -> io::Result<()> {
        if self.pos >= self.size {
            return Ok(());
        }
        let in_block = self.pos >= self.block_start
            && (self.pos - self.block_start) < self.block.len() as u64;
        if in_block {
            return Ok(());
        }
        let start = self.pos;
        let end = (start + READ_BLOCK - 1).min(self.size.saturating_sub(1));
        let block = self
            .fetcher
            .fetch(start, end)
            .map_err(io::Error::other)?;
        if block.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "empty range",
            ));
        }
        self.block_start = start;
        self.block = block;
        Ok(())
    }
}

impl<F: RangeFetcher> Read for RemoteReader<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        self.ensure_block()?;
        let offset = (self.pos - self.block_start) as usize;
        let available = self.block.len().saturating_sub(offset);
        if available == 0 {
            return Ok(0);
        }
        let n = available.min(buf.len());
        buf[..n].copy_from_slice(&self.block[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<F: RangeFetcher> Seek for RemoteReader<F> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target: i128 = match from {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.size) + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.pos) + i128::from(offset),
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "negative seek",
            ));
        }
        self.pos = u64::try_from(target).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek position overflow")
        })?;
        Ok(self.pos)
    }
}

/// Build a remote reader using the driver's file size (`WdmmVfs::get`), which
/// keeps `lofty`'s `SeekFrom::End` probes correct for MP4/APE tags.
fn remote_reader(
    account_id: &str,
    remote_path: &str,
) -> Result<RemoteReader<AccountRange>, String> {
    let driver = driver_for_account(account_id).map_err(|_| "account not found".to_string())?;
    let size = rt()
        .block_on(driver.get(remote_path))
        .map_err(|e| format!("stat: {e}"))?
        .size;
    Ok(RemoteReader::new(
        AccountRange {
            account_id: account_id.to_string(),
            remote_path: remote_path.to_string(),
        },
        size,
    ))
}

/// Probe the remote container and serialise tags/properties/cover.
fn extract(account_id: &str, remote_path: &str) -> Result<serde_json::Value, String> {
    let mut reader = remote_reader(account_id, remote_path)?;
    let tagged = {
        let probe = Probe::new(io::BufReader::new(&mut reader))
            .guess_file_type()
            .map_err(|e| format!("probe: {e}"))?;
        probe.read().map_err(|e| format!("parse: {e}"))?
    };
    let properties = tagged.properties();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let tags = tag.map(tags_json).unwrap_or_default();
    let cover = tag.and_then(cover_json).unwrap_or(serde_json::Value::Null);
    let bitrate = properties
        .audio_bitrate()
        .or_else(|| properties.overall_bitrate())
        .filter(|kbps| *kbps > 0)
        .map(|kbps| u64::from(kbps) * 1000);
    Ok(serde_json::json!({
        "ok": true,
        "format": format_name(tagged.file_type()),
        "tags": tags,
        "duration_ms": properties.duration().as_millis() as u64,
        "bitrate": bitrate,
        "sample_rate": properties.sample_rate(),
        "cover": cover,
    }))
}

/// Container label consumed by the Dart side (`mp3`, `flac`, `m4a`, …).
fn format_name(file_type: FileType) -> &'static str {
    match file_type {
        FileType::Mpeg => "mp3",
        FileType::Mp4 => "m4a",
        FileType::Flac => "flac",
        FileType::Opus => "opus",
        FileType::Vorbis => "ogg",
        FileType::Speex => "speex",
        FileType::Wav => "wav",
        FileType::Ape => "ape",
        FileType::Aac => "aac",
        FileType::WavPack => "wv",
        FileType::Mpc => "mpc",
        FileType::Aiff => "aiff",
        FileType::Custom(name) => name,
        _ => "unknown",
    }
}

fn optional_text(value: Option<std::borrow::Cow<'_, str>>) -> serde_json::Value {
    value.map_or(serde_json::Value::Null, |value| {
        serde_json::Value::String(value.into_owned())
    })
}

fn optional_number(value: Option<u32>) -> serde_json::Value {
    value.map_or(serde_json::Value::Null, serde_json::Value::from)
}

fn text_item(tag: &Tag, key: ItemKey) -> serde_json::Value {
    tag.get_string(key).map_or(serde_json::Value::Null, |value| {
        serde_json::Value::String(value.to_string())
    })
}

fn year_of(tag: &Tag) -> Option<u32> {
    if let Some(date) = tag.date() {
        return Some(u32::from(date.year));
    }
    for key in [
        ItemKey::Year,
        ItemKey::RecordingDate,
        ItemKey::OriginalReleaseDate,
    ] {
        let Some(text) = tag.get_string(key) else {
            continue;
        };
        let digits: String = text
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(4)
            .collect();
        if digits.len() == 4 {
            if let Ok(year) = digits.parse::<u32>() {
                return Some(year);
            }
        }
    }
    None
}

/// Tags consumed by Dart's `ReadTags`. `track_total`/`disc_total`/`language`/
/// `lyrics` are kept alongside the slice contract's nine keys so the in-app
/// metadata display does not regress.
fn tags_json(tag: &Tag) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("title".to_string(), optional_text(tag.title()));
    map.insert("artist".to_string(), optional_text(tag.artist()));
    map.insert("album".to_string(), optional_text(tag.album()));
    map.insert("album_artist".to_string(), text_item(tag, ItemKey::AlbumArtist));
    map.insert("track".to_string(), optional_number(tag.track()));
    map.insert("track_total".to_string(), optional_number(tag.track_total()));
    map.insert("disc".to_string(), optional_number(tag.disk()));
    map.insert("disc_total".to_string(), optional_number(tag.disk_total()));
    map.insert("year".to_string(), optional_number(year_of(tag)));
    map.insert("genre".to_string(), optional_text(tag.genre()));
    map.insert("comment".to_string(), optional_text(tag.comment()));
    map.insert("language".to_string(), text_item(tag, ItemKey::Language));
    // ID3v2 stores unsynchronized lyrics in a USLT frame, which lofty maps to
    // `UnsyncLyrics`; MP4/Vorbis use `Lyrics`. Accept either.
    map.insert(
        "lyrics".to_string(),
        optional_text(
            tag.get_string(ItemKey::Lyrics)
                .or_else(|| tag.get_string(ItemKey::UnsyncLyrics))
                .map(std::borrow::Cow::Borrowed),
        ),
    );
    map
}

/// Prefer the front cover; fall back to the first picture that fits the cap.
fn cover_picture(pictures: &[Picture]) -> Option<&Picture> {
    let fits = |picture: &&Picture| picture.data().len() <= MAX_COVER_BYTES;
    pictures
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront && fits(picture))
        .or_else(|| pictures.iter().find(fits))
}

fn mime_from_magic(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if data.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if data.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if data.starts_with(&[0x49, 0x49, 0x2A, 0x00]) || data.starts_with(&[0x4D, 0x4D, 0x00, 0x2A])
    {
        return Some("image/tiff");
    }
    None
}

fn cover_json(tag: &Tag) -> Option<serde_json::Value> {
    let picture = cover_picture(tag.pictures())?;
    let data = picture.data();
    let mime = picture.mime_type().map_or_else(
        || {
            mime_from_magic(data)
                .unwrap_or("application/octet-stream")
                .to_string()
        },
        |mime| mime.as_str().to_string(),
    );
    let info = PictureInformation::from_picture(picture).unwrap_or_default();
    Some(serde_json::json!({
        "mime": mime,
        "bytes_b64": base64::engine::general_purpose::STANDARD.encode(data),
        "width": info.width,
        "height": info.height,
        "description": picture.description(),
    }))
}

/// `wdmm_tags_extract(account_id, remote_path)` — embedded tags + cover as JSON.
#[no_mangle]
pub extern "C" fn wdmm_tags_extract(
    account_id: *const c_char,
    remote_path: *const c_char,
) -> FfiString {
    let account_id = match require_arg(account_id, "account_id") {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    let remote_path = match require_arg(remote_path, "remote_path") {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    json_reply(extract(account_id, remote_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::picture::MimeType;
    use lofty::tag::TagType;

    #[derive(Default)]
    struct VecFetcher {
        data: Vec<u8>,
    }

    impl RangeFetcher for VecFetcher {
        fn fetch(&mut self, start: u64, end_inclusive: u64) -> Result<Vec<u8>, String> {
            let start = start as usize;
            if start >= self.data.len() {
                return Ok(Vec::new());
            }
            let end = (end_inclusive as usize + 1).min(self.data.len());
            Ok(self.data[start..end].to_vec())
        }
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    fn reader(data: Vec<u8>) -> RemoteReader<VecFetcher> {
        let size = data.len() as u64;
        RemoteReader::new(VecFetcher { data }, size)
    }

    #[test]
    fn reader_spans_blocks_and_seeks() {
        let data = sample(READ_BLOCK as usize * 3 + 123);
        let mut reader = reader(data.clone());

        let mut head = [0u8; 10];
        reader.read_exact(&mut head).unwrap();
        assert_eq!(&head, &data[..10]);

        let boundary = READ_BLOCK as usize - 5;
        reader.seek(SeekFrom::Start(boundary as u64)).unwrap();
        let mut crossing = vec![0u8; 20];
        reader.read_exact(&mut crossing).unwrap();
        assert_eq!(&crossing, &data[boundary..boundary + 20]);

        reader.seek(SeekFrom::End(-10)).unwrap();
        let mut tail = [0u8; 10];
        reader.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, &data[data.len() - 10..]);

        reader.seek(SeekFrom::Start(data.len() as u64)).unwrap();
        let mut eof = [0u8; 4];
        assert_eq!(reader.read(&mut eof).unwrap(), 0);
        assert_eq!(reader.stream_position().unwrap(), data.len() as u64);
    }

    #[test]
    fn reader_rejects_negative_seek() {
        let mut reader = reader(sample(32));
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
    }

    #[test]
    fn format_names_are_stable() {
        assert_eq!(format_name(FileType::Mpeg), "mp3");
        assert_eq!(format_name(FileType::Mp4), "m4a");
        assert_eq!(format_name(FileType::Flac), "flac");
        assert_eq!(format_name(FileType::Opus), "opus");
        assert_eq!(format_name(FileType::Vorbis), "ogg");
        assert_eq!(format_name(FileType::Wav), "wav");
        assert_eq!(format_name(FileType::Ape), "ape");
        assert_eq!(format_name(FileType::Aac), "aac");
        assert_eq!(format_name(FileType::WavPack), "wv");
        assert_eq!(format_name(FileType::Mpc), "mpc");
        assert_eq!(format_name(FileType::Aiff), "aiff");
        assert_eq!(format_name(FileType::Custom("x")), "x");
    }

    #[test]
    fn tags_json_carries_dart_fields() {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("Title".to_string());
        tag.set_artist("Artist".to_string());
        tag.set_album("Album".to_string());
        tag.set_genre("Rock".to_string());
        tag.insert_text(ItemKey::AlbumArtist, "Album Artist".to_string());
        tag.insert_text(ItemKey::Language, "eng".to_string());
        tag.insert_text(ItemKey::UnsyncLyrics, "la la".to_string());
        // ID3v2 has no standalone "Year" frame; the date setter writes TDRC.
        tag.set_date(lofty::tag::items::Timestamp {
            year: 2021,
            ..Default::default()
        });
        tag.set_track(3);
        tag.set_disk(1);

        let map = tags_json(&tag);
        assert_eq!(map["title"], serde_json::json!("Title"));
        assert_eq!(map["artist"], serde_json::json!("Artist"));
        assert_eq!(map["album"], serde_json::json!("Album"));
        assert_eq!(map["album_artist"], serde_json::json!("Album Artist"));
        assert_eq!(map["genre"], serde_json::json!("Rock"));
        assert_eq!(map["language"], serde_json::json!("eng"));
        assert_eq!(map["lyrics"], serde_json::json!("la la"));
        assert_eq!(map["track"], serde_json::json!(3));
        assert_eq!(map["disc"], serde_json::json!(1));
        assert_eq!(map["year"], serde_json::json!(2021));
    }

    fn png_picture(data: Vec<u8>, pic_type: PictureType) -> Picture {
        Picture::unchecked(data)
            .pic_type(pic_type)
            .mime_type(MimeType::Png)
            .build()
    }

    #[test]
    fn cover_prefers_front_and_encodes_base64() {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.push_picture(png_picture(vec![7u8; 16], PictureType::Other));
        tag.push_picture(png_picture(vec![1u8, 2, 3, 4, 5, 6, 7, 8], PictureType::CoverFront));

        let cover = cover_json(&tag).expect("front cover");
        assert_eq!(cover["mime"], serde_json::json!("image/png"));
        let encoded = cover["bytes_b64"].as_str().expect("base64");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("decode");
        assert_eq!(decoded, vec![1u8, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(cover["width"], serde_json::json!(0));
    }

    #[test]
    fn cover_skips_oversized_pictures() {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.push_picture(png_picture(vec![0u8; MAX_COVER_BYTES + 1], PictureType::CoverFront));
        assert!(cover_json(&tag).is_none());
    }

    #[test]
    fn cover_absent_is_none() {
        let tag = Tag::new(TagType::Id3v2);
        assert!(cover_json(&tag).is_none());
    }
}
