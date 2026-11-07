//! Slice: credential-vault and playlist wire formats owned by Rust (audit item 3).
//!
//! These four symbols are the only way Dart encodes/decodes the WMP documents
//! (`WDMMCV01` vault, `WDMMPL01` playlist, `WDMMPD01` deletion pack). The wire
//! format lives in `wdmm_sync::wmp`; this file is a thin JSON edge:
//!
//! * one string argument (JSON) for encode, one byte buffer for decode,
//! * one `FfiString` reply — `{"ok":true,...}` on success, `error:<msg>` on
//!   failure (the Dart side already parses both shapes),
//! * document bytes travel base64 inside the JSON.

use std::os::raw::{c_char, c_uchar};

use base64::prelude::*;
use serde_json::{json, Value};
use wdmm_sync::wmp;

use super::json_reply;
use crate::FfiString;

/// Read the byte buffer behind `wdmm_*_decode`.
fn read_bytes(data: *const c_uchar, len: usize) -> Result<Vec<u8>, String> {
    if len == 0 {
        return Ok(Vec::new());
    }
    if data.is_null() {
        return Err("invalid data".to_string());
    }
    Ok(unsafe { std::slice::from_raw_parts(data, len) }.to_vec())
}

/// Read the JSON string argument behind `wdmm_*_encode`.
fn read_json(pointer: *const c_char, what: &str) -> Result<Value, String> {
    let text = crate::ptr_to_str(pointer).ok_or_else(|| format!("invalid {what}"))?;
    serde_json::from_str(text).map_err(|error| format!("invalid {what}: {error}"))
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn opt_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Driver config is a JSON object in Dart; the document stores it as a string.
fn driver_config_json(value: &Value) -> Option<String> {
    match value.get("driver_config") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => serde_json::to_string(other).ok(),
    }
}

fn driver_config_value(stored: Option<&str>) -> Value {
    stored
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or(Value::Null)
}

// ── credential vault (WDMMCV01) ──────────────────────────────────────────────

fn encode_vault(pointer: *const c_char) -> Result<Value, String> {
    let input = read_json(pointer, "vault json")?;
    let records = input
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let document = wmp::VaultDocument {
        format_version: input
            .get("format_version")
            .and_then(Value::as_u64)
            .unwrap_or(2) as u32,
        device_id: str_field(&input, "device_id"),
        created_at: str_field(&input, "created_at"),
        records: records
            .iter()
            .map(|record| wmp::VaultRecordDoc {
                id: str_field(record, "id"),
                name: str_field(record, "name"),
                provider_type: str_field(record, "provider_type"),
                url: str_field(record, "url"),
                username: str_field(record, "username"),
                password: str_field(record, "password"),
                password_encrypted: bool_field(record, "password_encrypted"),
                remote_path: str_field(record, "remote_path"),
                driver_config: driver_config_json(record),
                engine_updated_at: 0,
            })
            .collect(),
        // The user vault codec has no tombstones; the sync engine owns those.
        deleted: Vec::new(),
    };
    let bytes = document.encode();
    Ok(json!({
        "ok": true,
        "file_name": wmp::VAULT_FILE_NAME,
        "record_count": document.records.len(),
        "bytes_base64": BASE64_STANDARD.encode(&bytes),
    }))
}

fn decode_vault(data: *const c_uchar, len: usize) -> Result<Value, String> {
    let bytes = read_bytes(data, len)?;
    let document = wmp::VaultDocument::decode(&bytes).map_err(|error| error.to_string())?;
    let records: Vec<Value> = document
        .records
        .iter()
        .map(|record| {
            json!({
                "id": record.id,
                "name": record.name,
                "provider_type": record.provider_type,
                "url": record.url,
                "username": record.username,
                "password": record.password,
                "password_encrypted": record.password_encrypted,
                "remote_path": record.remote_path,
                "driver_config": driver_config_value(record.driver_config.as_deref()),
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "file_name": wmp::VAULT_FILE_NAME,
        "format_version": document.format_version,
        "device_id": document.device_id,
        "created_at": document.created_at,
        "records": records,
    }))
}

// ── playlist / playlist deletion (WDMMPL01 / WDMMPD01) ───────────────────────

/// `covers` is parallel to `entries`: `null` or a base64 image. The compacted
/// `COVERS` index is assigned here, so Dart never has to compute it.
fn playlist_covers(input: &Value, entry_count: usize) -> (Vec<wmp::PlaylistCoverDoc>, Vec<Option<i64>>) {
    let mut documents = Vec::new();
    let mut indexes: Vec<Option<i64>> = vec![None; entry_count];
    let covers = input
        .get("covers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (position, cover) in covers.iter().enumerate() {
        let Some(text) = cover.as_str() else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let Ok(bytes) = BASE64_STANDARD.decode(text) else {
            continue;
        };
        if position < indexes.len() {
            indexes[position] = Some(documents.len() as i64);
        }
        documents.push(wmp::PlaylistCoverDoc { kind: 0, bytes });
    }
    (documents, indexes)
}

fn encode_playlist(pointer: *const c_char) -> Result<Value, String> {
    let input = read_json(pointer, "playlist json")?;
    match input.get("kind").and_then(Value::as_str).unwrap_or_default() {
        "playlist" => {
            let playlist = input
                .get("playlist")
                .ok_or_else(|| "missing playlist".to_string())?;
            let entries = playlist
                .get("entries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let (covers, cover_indexes) = playlist_covers(&input, entries.len());
            let document = wmp::PlaylistDocument {
                id: str_field(playlist, "id"),
                name: str_field(playlist, "name"),
                created_at: str_field(playlist, "created_at"),
                updated_at: str_field(playlist, "updated_at"),
                device_id: str_field(&input, "device_id"),
                entries: entries
                    .iter()
                    .enumerate()
                    .map(|(position, entry)| wmp::PlaylistEntryDoc {
                        music_id: str_field(entry, "music_id"),
                        source_name: str_field(entry, "source_name"),
                        remote_path: str_field(entry, "remote_path"),
                        title: str_field(entry, "title"),
                        duration_ms: opt_i64(entry, "duration_ms"),
                        cue_track_index: opt_i64(entry, "cue_track_index"),
                        cover_index: cover_indexes
                            .get(position)
                            .copied()
                            .flatten()
                            .or_else(|| opt_i64(entry, "cover_index")),
                        remote_missing: bool_field(entry, "remote_missing"),
                    })
                    .collect(),
                covers,
            };
            if document.id.is_empty() {
                return Err("playlist has no id".to_string());
            }
            let file_name = wmp::safe_playlist_file_name(&document.name, &document.id);
            let bytes = document.encode();
            Ok(json!({
                "ok": true,
                "kind": "playlist",
                "file_name": file_name,
                "bytes_base64": BASE64_STANDARD.encode(&bytes),
            }))
        }
        "deletion" => {
            let playlist_id = str_field(&input, "playlist_id");
            if playlist_id.is_empty() {
                return Err("deletion pack has no playlist id".to_string());
            }
            let records = input
                .get("records")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let document = wmp::PlaylistDeletionDocument {
                playlist_id: playlist_id.clone(),
                device_id: str_field(&input, "device_id"),
                created_at: str_field(&input, "created_at"),
                updated_at: str_field(&input, "updated_at"),
                records: records
                    .iter()
                    .map(|record| wmp::PlaylistDeletionRecordDoc {
                        entry_identity: str_field(record, "entry_identity"),
                        deleted_at: str_field(record, "deleted_at"),
                    })
                    .collect(),
            };
            let file_name = wmp::playlist_deletion_file_name(&playlist_id);
            let bytes = document.encode();
            Ok(json!({
                "ok": true,
                "kind": "deletion",
                "file_name": file_name,
                "bytes_base64": BASE64_STANDARD.encode(&bytes),
            }))
        }
        other => Err(format!("unknown playlist document kind: {other}")),
    }
}

fn decode_playlist(data: *const c_uchar, len: usize) -> Result<Value, String> {
    let bytes = read_bytes(data, len)?;
    match wmp::container_file_kind(&bytes) {
        Some(kind) if kind == *wmp::FILE_KIND_PLAYLIST => {
            let document = wmp::PlaylistDocument::decode(&bytes).map_err(|error| error.to_string())?;
            let entries: Vec<Value> = document
                .entries
                .iter()
                .map(|entry| {
                    json!({
                        "music_id": entry.music_id,
                        "source_name": entry.source_name,
                        "remote_path": entry.remote_path,
                        "title": entry.title,
                        "duration_ms": entry.duration_ms,
                        "cue_track_index": entry.cue_track_index,
                        "cover_index": entry.cover_index,
                        "remote_missing": entry.remote_missing,
                    })
                })
                .collect();
            let covers: Vec<Value> = document
                .covers
                .iter()
                .map(|cover| {
                    json!({
                        "kind": cover.kind,
                        "bytes_base64": BASE64_STANDARD.encode(&cover.bytes),
                    })
                })
                .collect();
            Ok(json!({
                "ok": true,
                "kind": "playlist",
                "device_id": document.device_id,
                "created_at": document.created_at,
                "playlist": {
                    "id": document.id,
                    "name": document.name,
                    "created_at": document.created_at,
                    "updated_at": document.updated_at,
                    "entries": entries,
                },
                "covers": covers,
            }))
        }
        Some(kind) if kind == *wmp::FILE_KIND_PLAYLIST_DELETION => {
            let document =
                wmp::PlaylistDeletionDocument::decode(&bytes).map_err(|error| error.to_string())?;
            let records: Vec<Value> = document
                .records
                .iter()
                .map(|record| {
                    json!({
                        "entry_identity": record.entry_identity,
                        "deleted_at": record.deleted_at,
                    })
                })
                .collect();
            Ok(json!({
                "ok": true,
                "kind": "deletion",
                "device_id": document.device_id,
                "created_at": document.created_at,
                "playlist_id": document.playlist_id,
                "updated_at": document.updated_at,
                "records": records,
            }))
        }
        _ => Err("unsupported container kind".to_string()),
    }
}

// ── #[no_mangle] edges ───────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn wdmm_vault_encode(records_json: *const c_char) -> FfiString {
    json_reply(encode_vault(records_json))
}

#[no_mangle]
pub extern "C" fn wdmm_vault_decode(data: *const c_uchar, len: usize) -> FfiString {
    json_reply(decode_vault(data, len))
}

#[no_mangle]
pub extern "C" fn wdmm_playlist_encode(playlist_json: *const c_char) -> FfiString {
    json_reply(encode_playlist(playlist_json))
}

#[no_mangle]
pub extern "C" fn wdmm_playlist_decode(data: *const c_uchar, len: usize) -> FfiString {
    json_reply(decode_playlist(data, len))
}
