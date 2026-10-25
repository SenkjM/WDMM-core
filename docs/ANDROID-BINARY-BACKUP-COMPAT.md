# Android Binary Backup Compatibility

**Last updated:** 2026-10-26  
**Component:** `wdmm-sync`  
**Status:** ✅ Implemented

## Overview

Core restore (`restore_backup_bytes`) now dual-reads Android binary backup sections **OR** Core JSON sections, enabling seamless restore from backups created by either platform.

Core **create** (`build_backup_bytes`) remains unchanged: JSON sections with codec=0 (raw).

## Binary Format Support

### Codec Support
- **Codec 0** (raw): unchanged, used by Core create
- **Codec 1** (deflate): raw deflate (zlib `raw: true`), used by Android for TRACKS section

### Section Formats

| Section ID | Name          | Android Binary Format              | Core JSON Format          |
|------------|---------------|-------------------------------------|---------------------------|
| 5          | CREDENTIALS   | Nested WDMMCV01 vault container     | JSON array of accounts    |
| 2          | TRACKS        | Deflated WMP tag/value records      | JSON array of LibraryTrack|
| 6          | PLAYLISTS     | WDMMPL01 blob per playlist record   | JSON array with entries   |

Restore checks `looks_like_json` for CREDENTIALS, byte-level format detection for TRACKS/PLAYLISTS, and falls back to JSON if binary decode fails.

### Tag Constants (from Android reference)

**WMP Track** (section 2, deflated):
- String tags: sourceName=1, remotePath=2, fileName=3, title=5, artist=6, album=8, genre=15
- Int tags: 4,9,10,11,12,13,14,16,17,20,23,24,26 (rev, durationMs, trackNumber, etc.)

**WDMMCV01 Vault Account** (section 5, nested):
- String tags: id=1, name=2, providerType=3, url=4, username=5, password=6
- Int tags: passwordEncrypted=7
- driverConfig=9 (JSON string, may contain AESGCMv1: encrypted fields)

**WDMMPL01 Playlist** (section 6):
- META section: playlistId=9, name=10, updatedAt=11 (RFC3339)
- ENTRIES section: musicId=1, sourceName=2, remotePath=3, title=4, durationMs=5

### AESGCMv1 Field Encryption

Android encrypts sensitive fields (passwords, cloud driver tokens) with AESGCMv1 before embedding in backup:
- Wire format: `AESGCMv1:<base64(salt16|nonce12|ciphertext|mac16)>`
- KDF: PBKDF2-HMAC-SHA256, 120k iterations (same as envelope)
- Core restore decrypts these fields using the backup passphrase

## Implementation Notes

- `parse_wmp_container`: generic WMP container parser (CV/PL/BK), supports codec 0/1
- `decode_records`: tag/value decoder with varint length-prefixing
- `try_decrypt_aesgcmv1`: field-level decryption (distinct from envelope)
- Returns `"ok:restored:{sections},android-binary"` when binary path used

## Testing

See `backup::tests::android_binary_track_restore` for deflated binary track section test.

All existing Core JSON roundtrip tests remain passing.
