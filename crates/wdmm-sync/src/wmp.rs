//! Shared WMP (Webdav Media Manager Protocol) container helpers.
//!
//! Supports WDMM binary container format used by backup (WDMMBK01, WDMMCV01, WDMMPL01)
//! and library shards (WDMMLB01, WDMMLS01, WDMMLT01).
//!
//! Container structure: magic(8) + count(2) + flags(2) + section_table + payloads.
//! Each section: id(1) + codec(1) + offset(4) + storedLen(4) + rawLen(4) + crc32(4).

use crate::{SyncError, SyncResult};
use std::collections::BTreeMap;

/// Section IDs (Android WmpSections).
pub const SEC_META: u8 = 1;
pub const SEC_TRACKS: u8 = 2;
pub const SEC_COVERS: u8 = 3;
pub const SEC_TOMBS: u8 = 4;
pub const SEC_CREDENTIALS: u8 = 5;
pub const SEC_PLAYLISTS: u8 = 6;
pub const SEC_SETTINGS: u8 = 7;
pub const SEC_CUEALBUMS: u8 = 8;
pub const SEC_PASS_CHECK: u8 = 11;
pub const SEC_MANGA_LIBRARY: u8 = 13;
pub const SEC_USER_FOLDERS: u8 = 14;
pub const SEC_MANGA_COVERS: u8 = 15;
pub const SEC_DMR_PREFS: u8 = 16;

/// Container magic prefixes.
pub const MAGIC_BK: &[u8; 8] = b"WDMMBK01";  // backup
pub const MAGIC_CV: &[u8; 8] = b"WDMMCV01";  // credential vault
pub const MAGIC_PL: &[u8; 8] = b"WDMMPL01";  // playlist
pub const MAGIC_LB: &[u8; 8] = b"WDMMLB01";  // library base shard
pub const MAGIC_LS: &[u8; 8] = b"WDMMLS01";  // library segment shard
pub const MAGIC_LT: &[u8; 8] = b"WDMMLT01";  // library tombstone shard

/// Codec for a section: 0=raw, 1=deflate.
#[derive(Debug, Clone, Copy)]
pub enum Codec {
    Raw = 0,
    Deflate = 1,
}

/// Record value: int (varint u64), string (UTF-8), or bytes.
#[derive(Debug, Clone)]
pub enum RecordValue {
    Int(u64),
    Str(String),
    Bytes(Vec<u8>),
}

impl RecordValue {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            RecordValue::Int(v) => Some(*v as i64),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            RecordValue::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            RecordValue::Bytes(b) => Some(b.as_slice()),
            _ => None,
        }
    }
}

pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (!(crc & 1)).wrapping_add(1); // 0 or 0xFFFFFFFF
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Encode varint (u64).
pub fn encode_varint(mut val: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while val >= 0x80 {
        out.push((val as u8 & 0x7F) | 0x80);
        val >>= 7;
    }
    out.push(val as u8);
    out
}

/// Decode varint, returning (value, next_offset).
pub fn decode_varint(data: &[u8], mut at: usize) -> SyncResult<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        if at >= data.len() {
            return Err(SyncError::Other("truncated varint".into()));
        }
        let byte = data[at];
        at += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if (byte & 0x80) == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err(SyncError::Other("varint overflow".into()));
        }
    }
    Ok((result, at))
}

/// Decode one tag/value record from Android binary format.
/// Returns map of tag → value (int or String), and next offset.
pub fn decode_record(
    data: &[u8],
    mut at: usize,
    int_tags: &[u8],
) -> SyncResult<(BTreeMap<u8, RecordValue>, usize)> {
    let mut rec = BTreeMap::new();
    while at < data.len() {
        let tag = data[at];
        at += 1;
        if tag == 0 {
            // end of record
            break;
        }
        if int_tags.contains(&tag) {
            let (val, next) = decode_varint(data, at)?;
            rec.insert(tag, RecordValue::Int(val));
            at = next;
        } else {
            // length-prefixed UTF-8 string or bytes
            let (len, next) = decode_varint(data, at)?;
            at = next;
            let end = at + len as usize;
            if end > data.len() {
                return Err(SyncError::Other("record value out of bounds".into()));
            }
            let slice = &data[at..end];
            // Try UTF-8 decode; if it fails treat as bytes
            if let Ok(s) = std::str::from_utf8(slice) {
                rec.insert(tag, RecordValue::Str(s.to_string()));
            } else {
                rec.insert(tag, RecordValue::Bytes(slice.to_vec()));
            }
            at = end;
        }
    }
    Ok((rec, at))
}

/// Encode records (tag/value pairs) into Android binary format.
pub fn encode_records(records: &[BTreeMap<u8, RecordValue>], int_tags: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for rec in records {
        for (&tag, val) in rec {
            out.push(tag);
            if int_tags.contains(&tag) {
                if let RecordValue::Int(v) = val {
                    out.extend_from_slice(&encode_varint(*v));
                }
            } else {
                match val {
                    RecordValue::Str(s) => {
                        out.extend_from_slice(&encode_varint(s.len() as u64));
                        out.extend_from_slice(s.as_bytes());
                    }
                    RecordValue::Bytes(b) => {
                        out.extend_from_slice(&encode_varint(b.len() as u64));
                        out.extend_from_slice(b);
                    }
                    RecordValue::Int(_) => {
                        // Tag not in int_tags but value is int — encode as varint-prefixed empty
                        out.extend_from_slice(&encode_varint(0));
                    }
                }
            }
        }
        out.push(0); // end marker
    }
    out
}

/// Decode all records from a section.
pub fn decode_records(data: &[u8], int_tags: &[u8]) -> SyncResult<Vec<BTreeMap<u8, RecordValue>>> {
    let mut records = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let (rec, next) = decode_record(data, at, int_tags)?;
        if rec.is_empty() {
            break;
        }
        records.push(rec);
        at = next;
    }
    Ok(records)
}

/// Build a WMP container from section id → (payload, codec).
pub fn build_wmp_container(sections: &[(u8, Vec<u8>, Codec)], magic: &[u8; 8]) -> Vec<u8> {
    use flate2::write::DeflateEncoder;
    use std::io::Write;

    let count = sections.len() as u16;
    let header_len = 12usize;
    let table_len = sections.len() * 18;
    let payload_start = header_len + table_len;
    let mut payload = Vec::new();
    let mut table = Vec::with_capacity(table_len);

    for &(id, ref raw, codec) in sections {
        let offset = (payload_start + payload.len()) as u32;
        let raw_len = raw.len() as u32;
        let crc = crc32_ieee(raw);

        let (stored, codec_byte) = match codec {
            Codec::Raw => (raw.clone(), 0u8),
            Codec::Deflate => {
                let mut encoder = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(raw).expect("deflate write");
                let compressed = encoder.finish().expect("deflate finish");
                (compressed, 1u8)
            }
        };

        table.push(id);
        table.push(codec_byte);
        table.extend_from_slice(&offset.to_le_bytes());
        table.extend_from_slice(&(stored.len() as u32).to_le_bytes()); // storedLen
        table.extend_from_slice(&raw_len.to_le_bytes()); // rawLen
        table.extend_from_slice(&crc.to_le_bytes());
        payload.extend_from_slice(&stored);
    }

    let mut out = Vec::with_capacity(payload_start + payload.len());
    out.extend_from_slice(magic);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&table);
    out.extend_from_slice(&payload);
    out
}

/// Parse a WMP container, returning section_id → raw_payload map.
/// Validates magic prefix, checks CRC, inflates compressed sections.
pub fn parse_wmp_container(
    bytes: &[u8],
    expected_kind: Option<&[u8; 2]>,
) -> SyncResult<BTreeMap<u8, Vec<u8>>> {
    if bytes.len() < 12 || &bytes[0..4] != b"WDMM" {
        return Err(SyncError::Other("not a WDMM container".into()));
    }
    let kind = &bytes[4..6];
    if let Some(exp) = expected_kind {
        if kind != exp {
            return Err(SyncError::Other(format!(
                "container kind mismatch: got {}, want {}",
                String::from_utf8_lossy(kind),
                String::from_utf8_lossy(exp)
            )));
        }
    }
    let count = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let table_end = 12 + count * 18;
    if bytes.len() < table_end {
        return Err(SyncError::Other("truncated WMP section table".into()));
    }
    let mut map = BTreeMap::new();
    for i in 0..count {
        let off = 12 + i * 18;
        let id = bytes[off];
        let codec = bytes[off + 1];
        let offset = u32::from_le_bytes(bytes[off + 2..off + 6].try_into().unwrap()) as usize;
        let stored_len = u32::from_le_bytes(bytes[off + 6..off + 10].try_into().unwrap()) as usize;
        let raw_len = u32::from_le_bytes(bytes[off + 10..off + 14].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[off + 14..off + 18].try_into().unwrap());

        if offset + stored_len > bytes.len() {
            return Err(SyncError::Other(format!("WMP section {id} bounds")));
        }

        let stored = &bytes[offset..offset + stored_len];
        let raw = match codec {
            0 => stored.to_vec(),
            1 => {
                use flate2::Decompress;
                let mut decomp = Decompress::new(false);
                let mut out = Vec::with_capacity(raw_len);
                decomp
                    .decompress_vec(stored, &mut out, flate2::FlushDecompress::Finish)
                    .map_err(|e| SyncError::Other(format!("WMP section {id} inflate: {e}")))?;
                if out.len() != raw_len {
                    return Err(SyncError::Other(format!(
                        "WMP section {id} length mismatch: got {}, want {raw_len}",
                        out.len()
                    )));
                }
                out
            }
            _ => {
                return Err(SyncError::Other(format!("unsupported WMP codec {codec}")));
            }
        };

        if crc32_ieee(&raw) != crc {
            return Err(SyncError::Other(format!("WMP section {id} CRC mismatch")));
        }
        map.insert(id, raw);
    }
    Ok(map)
}
