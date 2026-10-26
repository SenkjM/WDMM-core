# Backup Library Source of Truth

## Current State

### Core Coverage (via FFI/storage)

Core (`wdmm-storage` + `wdmm-sync`) already handles the following library operations:

- **Accounts**: Insert, list, delete cloud accounts (WebDAV, cloud drivers)
- **Tracks**: Upsert, list, delete tracks (via `wdmm_track_*` FFI), delete by account/source
- **Playlists**: Create, update, list, delete playlists and entries
- **Manga**: Insert, list manga + pages + reading progress
- **DMR Preferences**: Upsert, list, replace DMR device preferences (`dmr_prefs` table)
- **Backup Create/Restore**: 
  - **Create**: JSON sections (codec 0) written to `.wdmm` archive
  - **Restore**: Dual-read supporting both Core JSON sections AND Android binary sections (codec 1, nested WDMMCV01/WDMMPL01)
- **User Folders**: Create, list, delete user folder hierarchy + items
- **CUE Sheets** (v5): Upsert, delete CUE albums and slices
- **Deleted Tracks** (v6): Tombstone tracking with upsert/clear/list/unpushed/mark_pushed/purge/tombstoned_remote_paths
- **Sync State** (v6): Per-remote sync cursors with get/save/clear
- **Library Clear Operations**: Clear all library data (tracks/playlists/mangas) or index-only (tracks/CUE/tombstones)

### Android SoT via sqflite `LibraryDatabase`

Android's Kotlin `LibraryDatabase` (sqflite) remains the source of truth for:

- Library rows that feed `BackupManager.buildArchiveBytes()` during backup creation
- Track indexing, album/artist aggregation, search queries
- UI state tied to sqflite queries (RecyclerView adapters, search fragments)

**Important**: Android **must not** switch backup create to Core yet, as Core's Rust DB may be empty or partial during migration. Android's backup create still reads from its sqflite DB and encodes binary sections (codec 1, deflated).

### Dual-Read Restore (Complete)

Backup **restore** is fully dual-read: Core can restore both:
1. **Core JSON sections** (codec 0) — created by Desktop or future Core-based Android
2. **Android binary sections** (codec 1) — created by current Android sqflite path

This allows seamless cross-platform restore: Desktop can restore Android backups, and vice versa.

## Migration Blockers

### Remaining Gaps for Full Library SoT

The following gaps remain before Android can fully switch to Core as library source of truth:

1. **Dual-write parity**: Android must write to both sqflite and Core during migration phase
2. ~~**Album/artist aggregation**: Core has tables but no track → album/artist auto-aggregation logic yet~~ ✅ **DONE (schema v7)**: Implemented in Phase A with stable hash-based IDs, automatic upsert on track insert, orphan pruning on delete, and rebuild helper
3. ~~**Search indexes**: FTS or optimized search indexes (current Core uses basic LIKE queries)~~ ✅ **DONE (schema v8)**: Implemented FTS5 full-text search over track title/artist/album/album_artist with automatic index maintenance on upsert/delete/import
4. **Migration validation**: Comprehensive testing of sqflite → Core data migration at scale

Once these are resolved, Android can:
- Switch backup **create** to read from Core's Rust DB (removing sqflite dependency)
- Deprecate and remove `music_library_database_impl.dart` / sqflite layer

## Explicit Status

- ✅ **Dual-read restore**: Complete and tested (both Core JSON and Android binary)
- ✅ **Cutover FFI** (v5): `wdmm_track_delete_by_source`, `wdmm_library_clear_all`, `wdmm_library_clear_index`, CUE slice upsert/delete/remaining
- ✅ **Tombstones + sync_state** (v6): `deleted_tracks` and `sync_state` tables in Core with full FFI CRUD
- ⏸ **Create cutover**: Blocked on dual-write validation and aggregation logic
- 📋 **Full SoT migration**: Requires resolving gaps listed above

## References

- `wdmm-sync/src/backup.rs`: Backup create/restore implementation
- `docs/106-ANDROID-BINARY-COMPATIBILITY.md`: Android binary section format details
- Android `BackupManager.kt`: Current Android backup creation (sqflite → binary)

## Verification Harness (no-device + on-device)

### No-device (CI / local)
- **Core**: `cargo test -p wdmm-storage --lib cue_json_roundtrip_schema` — documents CueAlbum/CueSlice JSON field set and upsert → remaining_slices → delete roundtrip.
- **Android**: `flutter test test/cue_rust_json_parity_test.dart` — asserts `RustLibraryService.cueAlbumJson` / `cueSliceJson` emit the same snake_case keys/types as Core (no FFI/.so required).

### On-device (after dual-write stabilizes)
- Debug startup still runs `RustLibraryProbe` for track row-count parity (`[RustLibraryProbe] tracks … (match)`).
- After CUE ingest with `kRustLibraryWriteEnabled`, compare Core `wdmm_cue_remaining_slices(source, cuePath)` against sqflite `remainingSlicesForCue` for the same album; mismatch means dual-write drift.

### Status notes (2026-10-26 schema v8)
- Android CUE **upsert** on ingest/scan is dual-written (Rust first, then sqflite); delete/clear paths already dual-written earlier.
- Music **tombstones** (`deleted_tracks`) + **sync_state** now landed in Core schema v6 with full FFI CRUD. Android dual-write remains TODO.
- Album/artist aggregation (schema v7) and FTS5 search (schema v8) are complete.
- Backup **create** remains sqflite/JSON; do not half-cut.
