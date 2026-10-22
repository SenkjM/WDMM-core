# wdmm-storage — Public API

rusqlite library database (schema v2). Tracks, artists, albums, accounts,
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
Fields: `already_done: bool`, `tracks: usize`, `playlists: usize`, `entries: usize`.
Returned by `import_from_sqflite`.

---

## Database

### `struct LibraryDatabase`
Mutex-guarded rusqlite connection. Schema v1 (base) + v2 (artists, albums, extended
tracks columns, manga stubs). All DDL uses `IF NOT EXISTS` (additive migrations).

### `fn LibraryDatabase::open(path: &Path) -> StorageResult<Self>`
Open or create. `:memory:` is valid. Runs full DDL + bumps schema version to 2.

### `fn LibraryDatabase::schema_version(&self) -> StorageResult<i64>`

---

### Tracks

### `fn upsert_track(&self, track: &LibraryTrack) -> StorageResult<()>`
Insert or replace all track columns (including v2 fields).

### `fn insert_track(&self, track: &LibraryTrack) -> StorageResult<()>`
Backward-compat alias for `upsert_track`.

### `fn get_track(&self, id: &str) -> StorageResult<Option<LibraryTrack>>`

### `fn list_tracks(&self) -> StorageResult<Vec<LibraryTrack>>`
Ordered by `title COLLATE NOCASE`.

### `fn delete_track(&self, id: &str) -> StorageResult<()>`

### `fn search_tracks(&self, query: &str) -> StorageResult<Vec<LibraryTrack>>`
Case-insensitive LIKE on `title` and `artist`. `%` and `_` in `query` are escaped.

---

### Artists (schema v2)

### `fn upsert_artist(&self, artist: &LibraryArtist) -> StorageResult<()>`
### `fn get_artist(&self, id: &str) -> StorageResult<Option<LibraryArtist>>`
### `fn list_artists(&self) -> StorageResult<Vec<LibraryArtist>>`
Ordered by `name`.
### `fn delete_artist(&self, id: &str) -> StorageResult<()>`
Tracks referencing this artist have their `artist_id` set to NULL (ON DELETE SET NULL).

---

### Albums (schema v2)

### `fn upsert_album(&self, album: &LibraryAlbum) -> StorageResult<()>`
### `fn get_album(&self, id: &str) -> StorageResult<Option<LibraryAlbum>>`
### `fn list_albums(&self) -> StorageResult<Vec<LibraryAlbum>>`
Ordered by `title`.
### `fn delete_album(&self, id: &str) -> StorageResult<()>`
Tracks referencing this album have their `album_id` set to NULL.

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

Opens `old_db_path` read-only; copies tracks, playlists, and entries into `dest`.
Idempotent: returns `ImportSummary { already_done: true, … }` if KV key
`sqflite_imported = 1` is already set. Sets the flag after a successful import.
Defensive: malformed rows are skipped.
