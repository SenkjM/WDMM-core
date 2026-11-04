//! Slice: backup header probe owned by Rust (audit item 1).
//!
//! `wdmm_backup_probe(data_path, passphrase)` answers "would this file restore
//! with this passphrase?" without Dart ever implementing the backup container
//! format. Replaces `lib/utils/backup_crypto.dart` + the Dart PBKDF2/GCM probe
//! in `lib/utils/backup_pass_check.dart`.

use std::os::raw::c_char;

use super::{json_reply, require_arg};
use crate::{ptr_to_str, FfiString};

/// `wdmm_backup_probe(data_path, passphrase)` — read a backup file and verdict it.
///
/// `data_path` is a path to the raw download (Dart writes a temp file first).
/// Reply on success:
/// `{"ok":true,"valid":true|false,"version":<u32>,"created_at":<string|null>,"reason":"bad-passphrase"|"not-backup"|null}`.
/// On a read failure: `error:...` (non-session API convention).
#[no_mangle]
pub extern "C" fn wdmm_backup_probe(data_path: *const c_char, passphrase: *const c_char) -> FfiString {
    let path = match require_arg(data_path, "data_path") {
        Ok(path) => path,
        Err(reply) => return reply,
    };
    let passphrase = ptr_to_str(passphrase).unwrap_or("");
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) => return FfiString::from(format!("error:read backup: {e}")),
    };
    json_reply(
        serde_json::to_value(wdmm_sync::probe_backup_bytes(&data, passphrase))
            .map_err(|e| format!("serde: {e}"))
            .map(|mut value| {
                if let Some(object) = value.as_object_mut() {
                    object.insert("ok".into(), serde_json::Value::Bool(true));
                }
                value
            }),
    )
}
