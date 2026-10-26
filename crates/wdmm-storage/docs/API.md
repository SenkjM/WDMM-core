# wdmm-storage — Public API

rusqlite library database (schema v7). Tracks, artists, albums, accounts,
playlists, playlist entries, KV store, and a one-shot sqflite migration.

---

## Error types

### `enum StorageError`
`Database(String)`, `NotFound(String)`.

### `type StorageResult<T> = std::result::Result<T, StorageError>`

---

## Playlist models (defined in this crate)

### `struct Playlist` (Serialize, Deserialize)
Fields: `id`, `name`, `created_at: DateTime<Utc>`, `updated_at: DateTime<Utc>`.
`updated_at` drives last-write-wins sync semantics.

### `struct PlaylistEntry` (Serialize, Deserialize)
Fields: `playlist_id`, `position: i64` (zero-based), `music_id` (SHA-1 track id),
`source_name`, `remote_path`, `title`, `duration_ms: i64` (0 = unknown).

---

## Import summary

### `struct ImportSummary` (Serialize, Default)
Fields: `already_done: bool`, `tracks: usize`, `playlists: usize`, `entries: usize`,
`deleted_tracks: usize` (v6), `sync_state: usize` (v6).
Returned by `import_from_sqflite`. The two v6 fields are additive — the JSON
shape is backward compatible for consumers that ignore unknown keys.

---

## Database

### `struct LibraryDatabase`
Mutex-guarded rusqlite connection. Schema v1 (base) + v2 (artists, albums, extended
tracks columns, manga stubs) + v7 (album/album_artist TEXT columns on tracks).
All DDL uses `IF NOT EXISTS` (additive migrations).

### `fn LibraryDatabase::open(path: &Path) -> StorageResult<Self>`
Open or create. `:memory:` is valid. Runs full DDL + bumps schema version to 7.

### `fn LibraryDatabase::schema_version(&self) -> StorageResult<i64>`

---

### Tracks

### `fn upsert_track(&self, track: &LibraryTrack) -> StorageResult<()>`
Insert or replace all track columns. When `album`, `album_artist`, or `artist` text
fields are present, automatically derives `artist_id` and `album_id` from stable
hashes and upserts into artists/albums tables. Album/artist IDs passed in the
LibraryTrack are ignored when text is present (text is source of truth).

### `fn insert_track(&self, track: &LibraryTrack) -> StorageResult<()>`
Backward-compat alias for `upsert_track`.

### `fn get_track(&self, id: &str) -> StorageResult<Option<LibraryTrack>>`

### `fn list_tracks(&self) -> StorageResult<Vec<LibraryTrack>>`
Ordered by `title COLLATE NOCASE`.

### `fn delete_track(&self, id: &str) -> StorageResult<()>`
Deletes the track and prunes orphaned artists/albums with zero referencing tracks.

### `fn search_tracks(&self, query: &str) -> StorageResult<Vec<LibraryTrack>>`
Case-insensitive LIKE on `title` and `artist`. `%` and `_` in `query` are escaped.

### `fn tracks_by_artist_name(&self, name: &str) -> StorageResult<Vec<LibraryTrack>>`
Returns all tracks matching the given artist name (case-insensitive).

### `fn tracks_by_album_title(&self, title: &str) -> StorageResult<Vec<LibraryTrack>>`
Returns all tracks matching the given album title (case-insensitive), ordered by
disc_number, track_number, title.

---

### Artists (schema v2, v7 aggregation)

Artist IDs are stable hashes (SHA-1 hex) of normalized artist names, prefixed "art-".
Artists are automatically created/updated via `upsert_track` when artist text is present.

### `fn upsert_artist(&self, artist: &LibraryArtist) -> StorageResult<()>`
### `fn get_artist(&self, id: &str) -> StorageResult<Option<LibraryArtist>>`
### `fn list_artists(&self) -> StorageResult<Vec<LibraryArtist>>`
Ordered by `name`.
### `fn delete_artist(&self, id: &str) -> StorageResult<()>`
Tracks referencing this artist have their `artist_id` set to NULL (ON DELETE SET NULL).
### `fn list_distinct_artist_names(&self) -> StorageResult<Vec<String>>`
Returns all distinct artist names from the artists table, ordered NOCASE.

---

### Albums (schema v2, v7 aggregation)

Album IDs are stable hashes (SHA-1 hex) of (album_title, album_artist_or_artist) pairs,
prefixed "alb-". Albums are automatically created/updated via `upsert_track` when album
text is present. Same album title under different album_artists remain distinct.

### `fn upsert_album(&self, album: &LibraryAlbum) -> StorageResult<()>`
### `fn get_album(&self, id: &str) -> StorageResult<Option<LibraryAlbum>>`
### `fn list_albums(&self) -> StorageResult<Vec<LibraryAlbum>>`
Ordered by `title`.
### `fn delete_album(&self, id: &str) -> StorageResult<()>`
Tracks referencing this album have their `album_id` set to NULL.
### `fn list_distinct_album_titles(&self) -> StorageResult<Vec<String>>`
Returns all distinct album titles from the albums table, ordered NOCASE.

---

### Aggregation helpers (schema v7)

### `fn ensure_aggregates_for_track(&self, track: &LibraryTrack) -> StorageResult<(Option<String>, Option<String>)>`
Derives artist_id and album_id from track text fields, upserts into artists/albums tables.
Returns (artist_id, album_id). Called automatically by `upsert_track`.

### `fn rebuild_aggregates_from_tracks(&self) -> StorageResult<()>`
Clears artists and albums tables, walks all tracks, re-derives aggregates and updates
track FKs. Idempotent. Called automatically at end of `import_from_sqflite`.

### `fn prune_orphan_aggregates(&self) -> StorageResult<()>`
Deletes artists and albums with zero referencing tracks. Called automatically by
`delete_track` and `delete_tracks_by_account`.

---

### Cloud accounts

### `fn insert_account(&self, account: &CloudAccount) -> StorageResult<()>`
INSERT OR REPLACE.
### `fn get_account(&self, id: &str) -> StorageResult<Option<CloudAccount>>`
### `fn list_accounts(&self) -> StorageResult<Vec<CloudAccount>>`
### `fn delete_account(&self, id: &str) -> StorageResult<()>`

---

### Playlists

### `fn upsert_playlist(&self, playlist: &Playlist) -> StorageResult<()>`
INSERT OR REPLACE. Caller controls `updated_at`.

### `fn get_playlist(&self, id: &str) -> StorageResult<Option<Playlist>>`
### `fn list_playlists(&self) -> StorageResult<Vec<Playlist>>`
Ordered by `name`.
### `fn delete_playlist(&self, id: &str) -> StorageResult<()>`
ON DELETE CASCADE removes all playlist entries.

### `fn set_playlist_entries(&self, playlist_id: &str, entries: &[PlaylistEntry]) -> StorageResult<()>`
Atomic delete-all + re-insert in a transaction.

### `fn list_playlist_entries(&self, playlist_id: &str) -> StorageResult<Vec<PlaylistEntry>>`
Ordered by `position`.

---

### User folders (schema v3)

**Ref kinds** (canonical Android set): `"album"` | `"artist"` | `"genre"` | `"track"` | `"manga"` | `"mangaSeries"`.
Storage accepts any non-empty string; the above values are the Android `UserFolderRefKind` enum.

### `fn list_user_folder_children(&self, parent_id: Option<&str>) -> StorageResult<Vec<UserFolder>>`
Direct children of a folder. `None`, `""`, or `"root"` → virtual root (rows with `parent_id IS NULL`).

### `fn get_user_folder(&self, id: &str) -> StorageResult<Option<UserFolder>>`
### `fn create_user_folder(&self, folder: &UserFolder) -> StorageResult<()>`
### `fn rename_user_folder(&self, id: &str, name: &str) -> StorageResult<()>`
### `fn move_user_folder(&self, id: &str, new_parent_id: Option<&str>) -> StorageResult<()>`
### `fn delete_user_folder_recursive(&self, id: &str) -> StorageResult<()>`
BFS child-first delete of folder + all descendants + their items.

### `fn list_user_folder_items(&self, folder_id: &str) -> StorageResult<Vec<UserFolderItem>>`
Items in a folder ordered by `sort_order`. `folder_id = "root"` is valid.

### `fn add_user_folder_item(&self, item: &UserFolderItem) -> StorageResult<()>`
INSERT OR REPLACE.

### `fn set_user_folder_items(&self, folder_id: &str, items: &[UserFolderItem]) -> StorageResult<()>`
Atomic delete-all + re-insert for a single folder in one transaction. Mirrors `set_playlist_entries`.
Only items for `folder_id` are deleted; other folders are unaffected. The `folder_id` field of each
element in `items` is ignored — the explicit `folder_id` argument is always used.

### `fn remove_user_folder_item(&self, folder_id: &str, ref_kind: &str, ref_key: &str) -> StorageResult<()>`

---

### KV store

### `fn save_kv(&self, key: &str, value: &str) -> StorageResult<()>`
INSERT OR REPLACE.
### `fn load_kv(&self, key: &str) -> StorageResult<Option<String>>`
`Ok(None)` if key absent.
### `fn delete_kv(&self, key: &str) -> StorageResult<()>`

---

### sqflite migration

### `fn LibraryDatabase::import_from_sqflite(old_db_path: &Path, dest: &LibraryDatabase) -> StorageResult<ImportSummary>`
One-shot import from an old sqflite `music_library.db`.

Opens `old_db_path` read-only; copies **five** surfaces into `dest`: tracks,
playlists, playlist entries, music tombstones (`deleted_tracks`, v6), and sync
cursors (`sync_state`, v6). Defensive: malformed rows are skipped, and a missing
`deleted_tracks` / `sync_state` table yields zero rows rather than an error. A
`sync_state` table without an `updated_at` column falls back to the current time.

Two KV flags gate progress:
- `sqflite_imported` — set after the legacy (v1–v5) import.
- `sqflite_imported_v6` — set after the tombstones + sync_state import.

Behaviour:
- both set → full no-op: `already_done: true`, all counts `0`.
- only `sqflite_imported` set → **v6 backfill**: imports *only* `deleted_tracks` +
  `sync_state` (devices that already ran the older import have tracks but empty
  tombstones/cursors). Returns `already_done: true` with `tracks`/`playlists`/
  `entries` = 0 and non-zero v6 counts; sets `sqflite_imported_v6`.
- neither set → full import of all five surfaces, then sets **both** flags.

**Track column mapping (PRAGMA-detected).** Real Android `music_library.db`
stores tracks with `music_id` (PK) + `source_name` — no `id`, `account_id`,
`size`, or `modified`. Desktop/legacy DBs use `id` + `account_id`. The tracks
columns are detected via `PRAGMA table_info(tracks)`: source → `source_name` if
present else `account_id`; id → `music_id` if present else `id`. The detected
source value is written to Core `LibraryTrack.account_id` (Core/FFI column name
**unchanged** — Android `source_name` == Core `account_id`, matching
`compare_with_sqflite`). Missing optional columns default: `title` → file
basename of `remote_path`, `artist` → `None`, `size` → `0`, `modified` → `None`,
`duration_ms` → `0`. A missing `tracks` table, or one with neither a source nor
an id column, yields **0 tracks** (not an error); the other surfaces still import.
