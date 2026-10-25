# Backup Library Source of Truth

## Current State

### Core Coverage (via FFI/storage)

Core (`wdmm-storage` + `wdmm-sync`) already handles the following library operations:

- **Accounts**: Insert, list, delete cloud accounts (WebDAV, cloud drivers)
- **Tracks**: Upsert, list, delete tracks (via `wdmm_track_*` FFI)
- **Playlists**: Create, update, list, delete playlists and entries
- **Manga**: Insert, list manga + pages + reading progress
- **DMR Preferences**: Upsert, list, replace DMR device preferences (`dmr_prefs` table)
- **Backup Create/Restore**: 
  - **Create**: JSON sections (codec 0) written to `.wdmm` archive
  - **Restore**: Dual-read supporting both Core JSON sections AND Android binary sections (codec 1, nested WDMMCV01/WDMMPL01)
- **User Folders**: Create, list, delete user folder hierarchy + items

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

### Cutover TODOs Observed in Android

The following operations are marked as TODOs for Core migration in Android codebase:

1. **`wdmm_track_delete_by_source`**: Delete all tracks for a given account/source
2. **`wdmm_library_clear_all`**: Clear entire library (all tracks, accounts, playlists)
3. **`wdmm_library_clear_index`**: Clear library index/search cache
4. **CUE slice delete**: Delete CUE-sheet track slices through Rust FFI

Once these are implemented in Core and exposed via FFI, Android can switch to Core for backup creation.

## Explicit Status

- ✅ **Dual-read restore**: Complete and tested (both Core JSON and Android binary)
- ⏸ **Create cutover**: Blocked until Android completes library data migration to Core
- 📋 **Remaining FFI**: Track delete, library clear, CUE slice management

## References

- `wdmm-sync/src/backup.rs`: Backup create/restore implementation
- `docs/106-ANDROID-BINARY-COMPATIBILITY.md`: Android binary section format details
- Android `BackupManager.kt`: Current Android backup creation (sqflite → binary)
