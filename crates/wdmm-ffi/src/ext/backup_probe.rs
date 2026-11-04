//! Slice: backup header probe owned by Rust (audit item 1).
//!
//! `wdmm_backup_probe(data_path, passphrase)` answers "would this file restore
//! with this passphrase?" without Dart ever implementing the backup container
//! format. Replaces `lib/utils/backup_crypto.dart` + the Dart PBKDF2/GCM probe
//! in `lib/utils/backup_pass_check.dart`.

use std::os::raw::c_char;

use crate::FfiString;

#[no_mangle]
pub extern "C" fn wdmm_backup_probe(_data_path: *const c_char, _passphrase: *const c_char) -> FfiString {
    FfiString::from("error:unimplemented:backup_probe")
}
