# Android Binary Backup Compatibility

**Last updated:** 2026-10-26  
**Component:** `wdmm-sync`  
**Status:** ✅ Implemented (Phase D complete)

## Overview

Core backup **create** (`build_backup_bytes`) now emits Android-compatible binary backup sections with codec 1 (deflate) for most sections and codec 0 (raw) for covers/credentials/playlists. Nested WDMMCV01/WDMMPL01 containers match Android wire format.

Core **restore** (`restore_backup_bytes`) dual-reads: Android binary sections **OR** legacy Core JSON sections.

## Binary Format Support

### Codec Support
- **Codec 0** (raw): covers, credentials (WDMMCV01), playlists (WDMMPL01 blobs)
- **Codec 1** (deflate): META, TRACKS, SETTINGS, CUEALBUMS, MANGA_LIBRARY, USER_FOLDERS, DMR_PREFS

### Section Formats

| Section ID | Name          | Core Create Format                     | Core Restore Dual-Read       |
|------------|---------------|----------------------------------------|------------------------------|
| 1          | META          | Deflated WMP tag/value records         | Binary only                  |
| 2          | TRACKS        | Deflated WMP tag/value records         | Binary OR JSON               |
| 5          | CREDENTIALS   | Nested WDMMCV01 vault container (raw)  | Binary OR JSON               |
| 6          | PLAYLISTS     | WDMMPL01 blob per playlist record (raw)| Binary OR JSON               |
| 7          | SETTINGS      | Deflated JSON                          | JSON                         |
| 8          | CUEALBUMS     | Omitted (Core schema v5+ lacks export) | JSON if present              |
| 13         | MANGA_LIBRARY | Deflated JSON                          | JSON                         |
| 14         | USER_FOLDERS  | Deflated JSON                          | JSON                         |
| 16         | DMR_PREFS     | Deflated JSON                          | JSON                         |

Sections **not** emitted by Core create: COVERS (3), PASS_CHECK (11), MANGA_COVERS (15) — Core doesn't manage cover files on disk; Desktop/Android handle those.

Restore checks `looks_like_json` for CREDENTIALS, byte-level format detection for TRACKS/PLAYLISTS, and falls back to JSON if binary decode fails.

### Tag Constants (from Android reference)

**WMP META** (section 1, deflated):
- Int tags: kind=1, count=2
- String tags: deviceId=5, createdAt=6, note=9

**WMP Track** (section 2, deflated):
- String tags: sourceName=1, remotePath=2, fileName=3, title=5, artist=6, albumArtist=7, album=8, genre=15
- Int tags: 9,10,12,14 (durationMs, trackNumber, discNumber, year)

**WDMMCV01 Vault Account** (section 5, nested):
- String tags: id=1, name=2, providerType=3, url=4, username=5, password=6
- Int tags: passwordEncrypted=7
- driverConfig=9 (JSON string, may contain AESGCMv1: encrypted fields)

**WDMMPL01 Playlist** (section 6):
- META section: playlistId=9, name=10, updatedAt=11 (RFC3339)
- ENTRIES section: musicId=1, sourceName=2, remotePath=3, title=4, durationMs=5

### AESGCMv1 Field Encryption

Core encrypts sensitive fields (passwords, cloud driver tokens) with AESGCMv1 before embedding in backup:
- Wire format: `AESGCMv1:<base64(salt16|nonce12|ciphertext|mac16)>`
- KDF: PBKDF2-HMAC-SHA256, 120k iterations (same as envelope)
- Core restore decrypts these fields using the backup passphrase

## Implementation Notes

- `build_wmp_container`: generic WMP container builder (BK/CV/PL), supports codec 0/1
- `encode_records`: tag/value encoder with varint length-prefixing
- `encrypt_aesgcmv1` / `try_decrypt_aesgcmv1`: field-level encryption (distinct from envelope)
- Returns `"ok:restored:{sections},android-binary"` when binary path used in restore

## Testing

See `backup::tests::android_binary_track_restore` for deflated binary track section test.

All existing Core JSON roundtrip tests remain passing with the new binary create.
