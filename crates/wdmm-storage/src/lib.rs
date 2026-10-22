//! `wdmm-storage` — rusqlite library DB (schema v2, music library essentials).
//!
//! Aligns with WDMM `music_library.db` direction (schema v18 from architecture
//! report). Schema v2 (Round 5) adds artists/albums tables, extends `tracks`
//! with library metadata columns, and adds search + delete. Manga tables are
//! skeleton stubs (Round 6 for full parity). Migrations are one-shot additive:
//! each `open()` runs the full DDL with `IF NOT EXISTS`, then a version bump.

#![forbid(unsafe_code)]

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::sync::Mutex;
use thiserror::Error as ThisError;

use wdmm_core::models::{CloudAccount, LibraryAlbum, LibraryArtist, LibraryTrack};

#[derive(Debug, ThisError)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(String),

    #[error("not found: {0}")]
    NotFound(String),
}

impl From<rusqlite::Error> for StorageError {
    fn from(e: rusqlite::Error) -> Self {
        StorageError::Database(e.to_string())
    }
}

pub type StorageResult<T> = std::result::Result<T, StorageError>;

pub struct LibraryDatabase {
    conn: Mutex<Connection>,
}

impl LibraryDatabase {
    /// Open or create the library database.
    ///
    /// Schema v1 (Round 4): base tables.
    /// Schema v2 (Round 5): artists, albums, extended tracks columns, manga stubs.
    /// Migrations are additive one-shot: all DDL uses `IF NOT EXISTS` / `IF NOT EXISTS`.
    pub fn open(path: &std::path::Path) -> StorageResult<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;

            -- ── Schema version ────────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS _schema_version (
                version INTEGER PRIMARY KEY
            );

            -- ── Cloud accounts ────────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS accounts (
                id                    TEXT PRIMARY KEY,
                provider_type         TEXT NOT NULL,
                display_name          TEXT NOT NULL,
                base_url              TEXT,
                credentials_encrypted BLOB NOT NULL,
                created_at            INTEGER NOT NULL,
                updated_at            INTEGER NOT NULL
            );

            -- ── Artists (schema v2) ───────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS artists (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                sort_name  TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_artists_name ON artists (name COLLATE NOCASE);

            -- ── Albums (schema v2) ────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS albums (
                id         TEXT PRIMARY KEY,
                title      TEXT NOT NULL,
                artist_id  TEXT,
                year       INTEGER,
                art_url    TEXT,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (artist_id) REFERENCES artists(id) ON DELETE SET NULL
            );
            CREATE INDEX IF NOT EXISTS idx_albums_artist ON albums (artist_id);

            -- ── Tracks (v1 columns + v2 columns all in one DDL) ───────────────
            --  The v2 extra columns have defaults so old rows stay valid.
            CREATE TABLE IF NOT EXISTS tracks (
                id            TEXT PRIMARY KEY,   -- SHA-1 identity
                title         TEXT NOT NULL,
                artist        TEXT,               -- denormalised display artist
                remote_path   TEXT NOT NULL,
                account_id    TEXT NOT NULL,
                size          INTEGER NOT NULL DEFAULT 0,
                modified      INTEGER,            -- Unix timestamp (s)
                created_at    INTEGER NOT NULL,   -- Unix timestamp (s)
                -- v2 columns ──────────────────────────────────────────────────
                album_id      TEXT,
                artist_id     TEXT,
                duration_ms   INTEGER NOT NULL DEFAULT 0,
                genre         TEXT,
                track_number  INTEGER,
                disc_number   INTEGER,
                year          INTEGER,
                mime_type     TEXT,
                FOREIGN KEY (album_id)  REFERENCES albums(id)  ON DELETE SET NULL,
                FOREIGN KEY (artist_id) REFERENCES artists(id) ON DELETE SET NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tracks_account  ON tracks (account_id);
            CREATE INDEX IF NOT EXISTS idx_tracks_album    ON tracks (album_id);
            CREATE INDEX IF NOT EXISTS idx_tracks_artist   ON tracks (artist_id);
            CREATE INDEX IF NOT EXISTS idx_tracks_title    ON tracks (title COLLATE NOCASE);

            -- ── User folders ──────────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS user_folders (
                id        TEXT PRIMARY KEY,
                name      TEXT NOT NULL,
                parent_id TEXT,
                created   INTEGER NOT NULL
            );

            -- ── Deleted items tombstone ───────────────────────────────────────
            CREATE TABLE IF NOT EXISTS deleted (
                id         TEXT PRIMARY KEY,
                deleted_at INTEGER NOT NULL
            );

            -- ── Queue tasks (stub — state managed in-process by wdmm-queue) ───
            CREATE TABLE IF NOT EXISTS queue_tasks (
                id      TEXT PRIMARY KEY,
                kind    TEXT NOT NULL,
                status  TEXT NOT NULL,
                created INTEGER NOT NULL
            );

            -- ── Playlists (Round 4) ───────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS playlists (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            -- ── Playlist entries ──────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS playlist_entries (
                playlist_id TEXT    NOT NULL,
                position    INTEGER NOT NULL,
                music_id    TEXT    NOT NULL,
                source_name TEXT    NOT NULL,
                remote_path TEXT    NOT NULL,
                title       TEXT    NOT NULL,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (playlist_id, position),
                FOREIGN KEY (playlist_id) REFERENCES playlists(id) ON DELETE CASCADE
            );

            -- ── KV store (Round 4: OpenAPI token persistence) ─────────────────
            CREATE TABLE IF NOT EXISTS kv_store (
                key        TEXT PRIMARY KEY,
                value      TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            -- ── Manga stubs (skeleton — Round 6 for full parity) ─────────────
            CREATE TABLE IF NOT EXISTS manga_series (
                id          TEXT PRIMARY KEY,
                title       TEXT NOT NULL,
                source_name TEXT NOT NULL,
                remote_path TEXT NOT NULL,
                created_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS manga_chapters (
                id          TEXT PRIMARY KEY,
                series_id   TEXT NOT NULL,
                title       TEXT,
                remote_path TEXT NOT NULL,
                page_count  INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL,
                FOREIGN KEY (series_id) REFERENCES manga_series(id) ON DELETE CASCADE
            );
            "#,
        )?;

        // Bump schema version to 2 (idempotent).
        conn.execute("INSERT OR IGNORE INTO _schema_version (version) VALUES (1)", [])?;
        conn.execute(
            "INSERT OR REPLACE INTO _schema_version (version) VALUES (2)",
            [],
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn schema_version(&self) -> StorageResult<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT MAX(version) FROM _schema_version",
            [],
            |row| row.get(0),
        )?)
    }

    // ── Tracks ────────────────────────────────────────────────────────────────

    /// Upsert a track (insert or replace all columns).
    pub fn upsert_track(&self, track: &LibraryTrack) -> StorageResult<()> {
        let now = Utc::now().timestamp();
        let modified_ts = track.modified.map(|dt| dt.timestamp());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tracks
             (id, title, artist, remote_path, account_id, size, modified, created_at,
              album_id, artist_id, duration_ms, genre, track_number, disc_number, year, mime_type)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8, ?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                &track.id,
                &track.title,
                &track.artist,
                &track.remote_path,
                &track.account_id,
                track.size as i64,
                modified_ts,
                now,
                &track.album_id,
                &track.artist_id,
                track.duration_ms,
                &track.genre,
                &track.track_number,
                &track.disc_number,
                &track.year,
                &track.mime_type,
            ],
        )?;
        Ok(())
    }

    /// Backward-compat alias.
    pub fn insert_track(&self, track: &LibraryTrack) -> StorageResult<()> {
        self.upsert_track(track)
    }

    pub fn get_track(&self, id: &str) -> StorageResult<Option<LibraryTrack>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, artist, remote_path, account_id, size, modified,
                    album_id, artist_id, duration_ms, genre, track_number, disc_number, year, mime_type
             FROM tracks WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_track(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn delete_track(&self, id: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Search tracks by title or artist (case-insensitive LIKE).
    pub fn search_tracks(&self, query: &str) -> StorageResult<Vec<LibraryTrack>> {
        let conn = self.conn.lock().unwrap();
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(
            "SELECT id, title, artist, remote_path, account_id, size, modified,
                    album_id, artist_id, duration_ms, genre, track_number, disc_number, year, mime_type
             FROM tracks
             WHERE title LIKE ?1 ESCAPE '\\' OR artist LIKE ?1 ESCAPE '\\'
             ORDER BY title COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![pattern], row_to_track)?;
        rows.map(|r| r.map_err(StorageError::from)).collect()
    }

    // ── Accounts ──────────────────────────────────────────────────────────────

    pub fn insert_account(&self, account: &CloudAccount) -> StorageResult<()> {
        let now = Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO accounts (id, provider_type, display_name, base_url, credentials_encrypted, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &account.id,
                &account.provider_type,
                &account.display_name,
                &account.base_url,
                &account.credentials_encrypted,
                now,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn get_account(&self, id: &str) -> StorageResult<Option<CloudAccount>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, provider_type, display_name, base_url, credentials_encrypted FROM accounts WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(CloudAccount {
                id: row.get(0)?,
                provider_type: row.get(1)?,
                display_name: row.get(2)?,
                base_url: row.get(3)?,
                credentials_encrypted: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_accounts(&self) -> StorageResult<Vec<CloudAccount>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, provider_type, display_name, base_url, credentials_encrypted FROM accounts",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(CloudAccount {
                id: row.get(0)?,
                provider_type: row.get(1)?,
                display_name: row.get(2)?,
                base_url: row.get(3)?,
                credentials_encrypted: row.get(4)?,
            })
        })?;

        let mut accounts = Vec::new();
        for account in rows {
            accounts.push(account?);
        }
        Ok(accounts)
    }

    pub fn delete_account(&self, id: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ── Tracks listing ─────────────────────────────────────────────────────────

    pub fn list_tracks(&self) -> StorageResult<Vec<LibraryTrack>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, artist, remote_path, account_id, size, modified,
                    album_id, artist_id, duration_ms, genre, track_number, disc_number, year, mime_type
             FROM tracks ORDER BY title COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], row_to_track)?;
        rows.map(|r| r.map_err(StorageError::from)).collect()
    }

    // ── Artists (schema v2) ──────────────────────────────────────────────────────

    pub fn upsert_artist(&self, artist: &LibraryArtist) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO artists (id, name, sort_name, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                &artist.id,
                &artist.name,
                &artist.sort_name,
                artist.created_at.timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn get_artist(&self, id: &str) -> StorageResult<Option<LibraryArtist>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, name, sort_name, created_at FROM artists WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(LibraryArtist {
                id: row.get(0)?,
                name: row.get(1)?,
                sort_name: row.get(2)?,
                created_at: ts_to_dt(row.get(3)?)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_artists(&self) -> StorageResult<Vec<LibraryArtist>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, name, sort_name, created_at FROM artists ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok(LibraryArtist {
                id: row.get(0)?,
                name: row.get(1)?,
                sort_name: row.get(2)?,
                created_at: ts_to_dt(row.get(3)?).unwrap_or_else(|_| Utc::now()),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_artist(&self, id: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM artists WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ── Albums (schema v2) ──────────────────────────────────────────────────────

    pub fn upsert_album(&self, album: &LibraryAlbum) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO albums (id, title, artist_id, year, art_url, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &album.id,
                &album.title,
                &album.artist_id,
                &album.year,
                &album.art_url,
                album.created_at.timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn get_album(&self, id: &str) -> StorageResult<Option<LibraryAlbum>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, title, artist_id, year, art_url, created_at FROM albums WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(LibraryAlbum {
                id: row.get(0)?,
                title: row.get(1)?,
                artist_id: row.get(2)?,
                year: row.get(3)?,
                art_url: row.get(4)?,
                created_at: ts_to_dt(row.get(5)?)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_albums(&self) -> StorageResult<Vec<LibraryAlbum>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, title, artist_id, year, art_url, created_at FROM albums ORDER BY title")?;
        let rows = stmt.query_map([], |row| {
            Ok(LibraryAlbum {
                id: row.get(0)?,
                title: row.get(1)?,
                artist_id: row.get(2)?,
                year: row.get(3)?,
                art_url: row.get(4)?,
                created_at: ts_to_dt(row.get(5)?).unwrap_or_else(|_| Utc::now()),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_album(&self, id: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM albums WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ── Playlists (Round 4: full CRUD + entries per docs/10-PLAYLIST-FORMAT.md) ─

    /// Insert or replace a playlist row (caller controls `updated_at`).
    pub fn upsert_playlist(&self, playlist: &Playlist) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO playlists (id, name, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                &playlist.id,
                &playlist.name,
                playlist.created_at.timestamp(),
                playlist.updated_at.timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn get_playlist(&self, id: &str) -> StorageResult<Option<Playlist>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, name, created_at, updated_at FROM playlists WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Playlist {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: ts_to_dt(row.get(2)?)?,
                updated_at: ts_to_dt(row.get(3)?)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_playlists(&self) -> StorageResult<Vec<Playlist>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, name, created_at, updated_at FROM playlists ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok(Playlist {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: ts_to_dt(row.get(2)?).unwrap(),
                updated_at: ts_to_dt(row.get(3)?).unwrap(),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_playlist(&self, id: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        // ON DELETE CASCADE removes entries.
        conn.execute("DELETE FROM playlists WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Replace all entries for a playlist in one shot.
    pub fn set_playlist_entries(
        &self,
        playlist_id: &str,
        entries: &[PlaylistEntry],
    ) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM playlist_entries WHERE playlist_id = ?1",
            params![playlist_id],
        )?;
        for e in entries {
            tx.execute(
                "INSERT INTO playlist_entries
                 (playlist_id, position, music_id, source_name, remote_path, title, duration_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    playlist_id,
                    e.position,
                    &e.music_id,
                    &e.source_name,
                    &e.remote_path,
                    &e.title,
                    e.duration_ms,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_playlist_entries(&self, playlist_id: &str) -> StorageResult<Vec<PlaylistEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT playlist_id, position, music_id, source_name, remote_path, title, duration_ms
             FROM playlist_entries WHERE playlist_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map(params![playlist_id], |row| {
            Ok(PlaylistEntry {
                playlist_id: row.get(0)?,
                position: row.get(1)?,
                music_id: row.get(2)?,
                source_name: row.get(3)?,
                remote_path: row.get(4)?,
                title: row.get(5)?,
                duration_ms: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ── KV store (Round 4: OpenAPI token persistence; extensible for settings) ─

    /// Persist a key/value pair (insert or replace).
    pub fn save_kv(&self, key: &str, value: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT OR REPLACE INTO kv_store (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params![key, value, now],
        )?;
        Ok(())
    }

    /// Load a value by key. Returns `Ok(None)` if the key is absent.
    pub fn load_kv(&self, key: &str) -> StorageResult<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM kv_store WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub fn delete_kv(&self, key: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM kv_store WHERE key = ?1", params![key])?;
        Ok(())
    }
}

// ── Playlist models (storage-local; identity aligns with wdmm-core) ──────────

/// A playlist row. `updated_at` drives last-write-wins sync semantics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A playlist entry — a reference to a track (identity + display fields),
/// per docs/10-PLAYLIST-FORMAT.md §4.2 ENTRIES. Tags are never copied; the
/// library resolves display data from `music_id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlaylistEntry {
    pub playlist_id: String,
    /// Zero-based position within the playlist.
    pub position: i64,
    /// SHA-1 track identity (wdmm-core::identity::track_identity).
    pub music_id: String,
    pub source_name: String,
    pub remote_path: String,
    pub title: String,
    /// Duration in milliseconds (0 if unknown).
    pub duration_ms: i64,
}

fn ts_to_dt(ts: i64) -> StorageResult<DateTime<Utc>> {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Utc))
        .ok_or_else(|| StorageError::Database(format!("invalid timestamp {ts}")))
}

/// Map a rusqlite `Row` (in the v2 column order) to a `LibraryTrack`.
fn row_to_track(row: &rusqlite::Row<'_>) -> rusqlite::Result<LibraryTrack> {
    let modified_ts: Option<i64> = row.get(6)?;
    let modified = modified_ts.and_then(|ts| DateTime::from_timestamp(ts, 0));
    Ok(LibraryTrack {
        id: row.get(0)?,
        title: row.get(1)?,
        artist: row.get(2)?,
        remote_path: row.get(3)?,
        account_id: row.get(4)?,
        size: row.get::<_, i64>(5)? as u64,
        modified,
        album_id: row.get(7)?,
        artist_id: row.get(8)?,
        duration_ms: row.get(9)?,
        genre: row.get(10)?,
        track_number: row.get(11)?,
        disc_number: row.get(12)?,
        year: row.get(13)?,
        mime_type: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_v2() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
    }

    #[test]
    fn create_and_query() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();

        let track = LibraryTrack {
            id: "test-id".into(),
            title: "Test Track".into(),
            artist: Some("Artist".into()),
            remote_path: "/music/test.mp3".into(),
            account_id: "acc1".into(),
            size: 12345,
            modified: None,
            ..Default::default()
        };
        db.insert_track(&track).unwrap();

        let loaded = db.get_track("test-id").unwrap().unwrap();
        assert_eq!(loaded.title, "Test Track");
    }

    #[test]
    fn track_crud_v2_fields() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now();

        // Parents must exist before the track (FK constraints are enforced).
        db.upsert_artist(&LibraryArtist {
            id: "art1".into(),
            name: "Artist".into(),
            sort_name: None,
            created_at: now,
        })
        .unwrap();
        db.upsert_album(&LibraryAlbum {
            id: "alb1".into(),
            title: "Album".into(),
            artist_id: Some("art1".into()),
            year: Some(2024),
            art_url: None,
            created_at: now,
        })
        .unwrap();

        let track = LibraryTrack {
            id: "t1".into(),
            title: "Song".into(),
            artist: Some("Artist".into()),
            remote_path: "/music/song.mp3".into(),
            account_id: "acc1".into(),
            size: 5000,
            modified: None,
            album_id: Some("alb1".into()),
            artist_id: Some("art1".into()),
            duration_ms: 210_000,
            genre: Some("Rock".into()),
            track_number: Some(3),
            disc_number: Some(1),
            year: Some(2024),
            mime_type: Some("audio/mpeg".into()),
        };
        db.upsert_track(&track).unwrap();

        let loaded = db.get_track("t1").unwrap().unwrap();
        assert_eq!(loaded.album_id.as_deref(), Some("alb1"));
        assert_eq!(loaded.duration_ms, 210_000);
        assert_eq!(loaded.genre.as_deref(), Some("Rock"));
        assert_eq!(loaded.track_number, Some(3));
        assert_eq!(loaded.mime_type.as_deref(), Some("audio/mpeg"));

        // Delete
        db.delete_track("t1").unwrap();
        assert!(db.get_track("t1").unwrap().is_none());
    }

    #[test]
    fn search_tracks() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();

        let tracks = [
            ("a", "Yesterday", Some("The Beatles")),
            ("b", "Tomorrow", Some("Silver Sun")),
            ("c", "Daydream", Some("Beatles Solo")),
        ];
        for (id, title, artist) in tracks {
            db.upsert_track(&LibraryTrack {
                id: id.into(),
                title: title.into(),
                artist: artist.map(Into::into),
                remote_path: format!("/m/{id}.mp3"),
                account_id: "acc".into(),
                size: 1,
                modified: None,
                ..Default::default()
            })
            .unwrap();
        }

        // Title match
        let r = db.search_tracks("day").unwrap();
        assert_eq!(r.len(), 2, "title 'Day' + 'daydream'");

        // Artist match (case-insensitive)
        let r = db.search_tracks("beatles").unwrap();
        assert_eq!(r.len(), 2, "two beatles artists");

        // No match
        assert!(db.search_tracks("zzz").unwrap().is_empty());
    }

    #[test]
    fn artists_and_albums_crud() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now();

        let artist = LibraryArtist {
            id: "art1".into(),
            name: "The Beatles".into(),
            sort_name: Some("Beatles, The".into()),
            created_at: now,
        };
        db.upsert_artist(&artist).unwrap();
        assert_eq!(db.list_artists().unwrap().len(), 1);
        assert_eq!(
            db.get_artist("art1").unwrap().unwrap().sort_name.as_deref(),
            Some("Beatles, The")
        );

        let album = LibraryAlbum {
            id: "alb1".into(),
            title: "Abbey Road".into(),
            artist_id: Some("art1".into()),
            year: Some(1969),
            art_url: None,
            created_at: now,
        };
        db.upsert_album(&album).unwrap();
        assert_eq!(db.list_albums().unwrap().len(), 1);
        assert_eq!(db.get_album("alb1").unwrap().unwrap().year, Some(1969));

        db.delete_album("alb1").unwrap();
        db.delete_artist("art1").unwrap();
        assert!(db.list_albums().unwrap().is_empty());
        assert!(db.list_artists().unwrap().is_empty());
    }

    #[test]
    fn kv_store_roundtrip() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(db.load_kv("missing").unwrap(), None);

        db.save_kv("openapi_token", "tok-abc-123").unwrap();
        assert_eq!(
            db.load_kv("openapi_token").unwrap(),
            Some("tok-abc-123".into())
        );

        // Overwrite
        db.save_kv("openapi_token", "tok-new").unwrap();
        assert_eq!(db.load_kv("openapi_token").unwrap(), Some("tok-new".into()));

        db.delete_kv("openapi_token").unwrap();
        assert_eq!(db.load_kv("openapi_token").unwrap(), None);
    }

    #[test]
    fn playlist_crud() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        assert!(db.list_playlists().unwrap().is_empty());

        let now = Utc::now();
        let pl = Playlist {
            id: "pl-1".into(),
            name: "Favourites".into(),
            created_at: now,
            updated_at: now,
        };
        db.upsert_playlist(&pl).unwrap();
        assert_eq!(db.list_playlists().unwrap().len(), 1);
        assert_eq!(db.get_playlist("pl-1").unwrap().unwrap().name, "Favourites");

        // Entries
        let entries = vec![
            PlaylistEntry {
                playlist_id: "pl-1".into(),
                position: 0,
                music_id: "aa".repeat(20),
                source_name: "my_nas".into(),
                remote_path: "/music/a.mp3".into(),
                title: "Track A".into(),
                duration_ms: 180_000,
            },
            PlaylistEntry {
                playlist_id: "pl-1".into(),
                position: 1,
                music_id: "bb".repeat(20),
                source_name: "my_nas".into(),
                remote_path: "/music/b.mp3".into(),
                title: "Track B".into(),
                duration_ms: 240_000,
            },
        ];
        db.set_playlist_entries("pl-1", &entries).unwrap();
        let loaded = db.list_playlist_entries("pl-1").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].title, "Track A");
        assert_eq!(loaded[1].duration_ms, 240_000);

        // Delete playlist cascades entries
        db.delete_playlist("pl-1").unwrap();
        assert!(db.list_playlists().unwrap().is_empty());
        assert!(db.list_playlist_entries("pl-1").unwrap().is_empty());
    }
}
