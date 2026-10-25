//! `wdmm-storage` — rusqlite library DB (schema v4, music + manga library).
//!
//! Schema v1 (Round 4): base tables.
//! Schema v2 (Round 5): artists/albums tables, extended `tracks` columns,
//!   search + delete.
//! Schema v3 (Round 12): `user_folders` column fix (`created` → `created_at`,
//!   add `sort_order`), new `user_folder_items` table + indexes. Migrations are
//!   idempotent: base DDL uses `IF NOT EXISTS`; v3 ALTER statements ignore
//!   duplicate-column errors so opening any DB version converges to v3.
//! Schema v4 (Wave D): replace manga stubs with real `mangas` / `manga_pages` /
//!   `manga_progress` / `deleted_mangas` tables.

#![forbid(unsafe_code)]

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags};
use std::sync::Mutex;
use thiserror::Error as ThisError;
use uuid::Uuid;

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

mod dmr_prefs;
pub use dmr_prefs::{DmrPrefRow, DmrPrefsDatabase};

/// Summary returned by [`LibraryDatabase::import_from_sqflite`].
#[derive(Debug, Default, serde::Serialize)]
pub struct ImportSummary {
    /// `true` if the KV flag `sqflite_imported = 1` was already set on `dest`.
    pub already_done: bool,
    /// Number of tracks inserted.
    pub tracks: usize,
    /// Number of playlists inserted.
    pub playlists: usize,
    /// Number of playlist entries inserted.
    pub entries: usize,
}

/// A manga item stored in the library.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Manga {
    pub id: String,
    pub source_name: String,
    pub remote_path: String,
    pub title: String,
    pub author: Option<String>,
    pub series: Option<String>,
    pub volume: Option<String>,
    pub chapter_number: Option<String>,
    pub page_count: i64,
    pub cover_relpath: Option<String>,
    /// Format tag (e.g. `"cbz"`, `"pdf"`, `"images"`, `"unknown"`).
    pub format: String,
    /// Non-zero when the remote source no longer has this file.
    pub remote_missing: i64,
    pub page_turn: Option<String>,
    pub rev: i64,
    /// RFC-3339 timestamp string.
    pub updated_at: String,
}

/// A single page within a [`Manga`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MangaPage {
    pub manga_id: String,
    pub page_index: i64,
    pub rel_path: String,
}

/// Reading progress for a manga.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MangaProgress {
    pub manga_id: String,
    pub page_index: i64,
    /// RFC-3339 timestamp string.
    pub updated_at: String,
}

/// Tombstone appended to `deleted_mangas` when a manga is deleted locally.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeletedManga {
    pub manga_id: String,
    pub source_name: String,
    pub remote_path: String,
    pub sync_key: String,
    pub rev: Option<i64>,
    /// RFC-3339 timestamp string.
    pub deleted_at: String,
    /// `1` once the tombstone has been pushed to the sync server.
    pub pushed: i64,
}

/// A distinct series tag with its derived id.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SeriesTag {
    /// SHA-1 hex of `"mangaSeries"` ‖ `title`. See [`series_id_for_title`].
    pub id: String,
    pub title: String,
}

/// Derive the series id from a series title.
///
/// Formula: `sha1(utf8("mangaSeries") || utf8(title))` encoded as lower-case hex.
/// Matches the client-side `mangaSeriesId` helper.
pub fn series_id_for_title(title: &str) -> String {
    use sha1::Digest as _;
    let mut h = sha1::Sha1::new();
    h.update(b"mangaSeries");
    h.update(title.as_bytes());
    hex::encode(h.finalize())
}

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

            -- ── User folders (v3: created_at + sort_order; ALTER migration handles old DBs) ──
            CREATE TABLE IF NOT EXISTS user_folders (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                parent_id  TEXT,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
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

            -- ── Manga (schema v4) ─────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS mangas (
                id              TEXT NOT NULL PRIMARY KEY,
                source_name     TEXT NOT NULL,
                remote_path     TEXT NOT NULL,
                title           TEXT NOT NULL,
                author          TEXT,
                series          TEXT,
                volume          TEXT,
                chapter_number  TEXT,
                page_count      INTEGER NOT NULL DEFAULT 0,
                cover_relpath   TEXT,
                format          TEXT NOT NULL DEFAULT 'unknown',
                remote_missing  INTEGER NOT NULL DEFAULT 0,
                page_turn       TEXT,
                rev             INTEGER NOT NULL DEFAULT 0,
                updated_at      TEXT NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_mangas_source_path
                ON mangas (source_name, remote_path)
                WHERE format <> 'images';

            CREATE TABLE IF NOT EXISTS manga_pages (
                manga_id    TEXT    NOT NULL,
                page_index  INTEGER NOT NULL,
                rel_path    TEXT    NOT NULL,
                PRIMARY KEY (manga_id, page_index),
                FOREIGN KEY (manga_id) REFERENCES mangas(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS manga_progress (
                manga_id    TEXT    PRIMARY KEY,
                page_index   INTEGER NOT NULL DEFAULT 0,
                updated_at  TEXT    NOT NULL,
                FOREIGN KEY (manga_id) REFERENCES mangas(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS deleted_mangas (
                manga_id    TEXT    PRIMARY KEY,
                source_name TEXT    NOT NULL,
                remote_path TEXT    NOT NULL,
                sync_key    TEXT    NOT NULL,
                rev         INTEGER,
                deleted_at  TEXT    NOT NULL,
                pushed      INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )?;

        // Seed schema version rows for v1 and v2.
        conn.execute("INSERT OR IGNORE INTO _schema_version (version) VALUES (1)", [])?;
        conn.execute("INSERT OR IGNORE INTO _schema_version (version) VALUES (2)", [])?;

        // ── v3 migration: fix user_folders columns + create user_folder_items ──
        let current_ver: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _schema_version",
            [],
            |row| row.get(0),
        )?;
        if current_ver < 3 {
            // Rename old 'created' → 'created_at'. On fresh DBs the DDL
            // above already uses created_at, so the RENAME will fail and the
            // error is silently discarded (idempotent).
            let _ = conn.execute_batch(
                "ALTER TABLE user_folders RENAME COLUMN created TO created_at;",
            );
            // Add sort_order column. Silently discarded if already present.
            let _ = conn.execute_batch(
                "ALTER TABLE user_folders ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;",
            );
            // Create user_folder_items table + indexes (all idempotent).
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS user_folder_items (
                    folder_id  TEXT    NOT NULL,
                    ref_kind   TEXT    NOT NULL,
                    ref_key    TEXT    NOT NULL,
                    sort_order INTEGER NOT NULL DEFAULT 0,
                    added_at   INTEGER NOT NULL,
                    PRIMARY KEY (folder_id, ref_kind, ref_key)
                );
                CREATE INDEX IF NOT EXISTS idx_user_folders_parent
                    ON user_folders (parent_id);
                CREATE INDEX IF NOT EXISTS idx_user_folder_items_folder
                    ON user_folder_items (folder_id);
                "#,
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO _schema_version (version) VALUES (3)",
                [],
            )?;
        }

        // ── v4 migration: drop manga stubs; real tables live in base DDL above ──
        // The v1/v2 base DDL created `manga_series` / `manga_chapters` stubs. The
        // current base DDL instead creates the real `mangas` / `manga_pages` /
        // `manga_progress` / `deleted_mangas` tables with `IF NOT EXISTS`, so for
        // an upgrading DB they are already present by the time we reach here —
        // we only need to drop the obsolete stubs and record the new version.
        if current_ver < 4 {
            conn.execute_batch(
                r#"
                DROP TABLE IF EXISTS manga_chapters;
                DROP TABLE IF EXISTS manga_series;
                "#,
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO _schema_version (version) VALUES (4)",
                [],
            )?;
        }

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

    /// Dump all KV pairs (for backup).
    pub fn list_kv(&self) -> StorageResult<Vec<(String, String)>> {
        let conn = self.conn.lock().map_err(|e| StorageError::Database(e.to_string()))?;
        let mut stmt = conn.prepare("SELECT key, value FROM kv_store ORDER BY key")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn delete_kv(&self, key: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM kv_store WHERE key = ?1", params![key])?;
        Ok(())
    }

    // ── sqflite migration ──────────────────────────────────────────────────────

    /// One-shot import from an old sqflite `music_library.db`.
    ///
    /// Opens `old_db_path` read-only and copies tracks, playlists, and playlist
    /// entries into `dest`. If the KV flag `sqflite_imported = 1` is already set
    /// on `dest` the function returns early with `already_done: true` and makes
    /// no changes.
    ///
    /// The read is defensive: if expected columns are absent (old schema), the
    /// row is skipped and import continues.
    pub fn import_from_sqflite(
        old_db_path: &std::path::Path,
        dest: &LibraryDatabase,
    ) -> StorageResult<ImportSummary> {
        // ── Early-exit if already imported ────────────────────────────────────
        if dest.load_kv("sqflite_imported")?.as_deref() == Some("1") {
            return Ok(ImportSummary {
                already_done: true,
                ..Default::default()
            });
        }

        // ── Open old DB read-only ─────────────────────────────────────────────
        let old = Connection::open_with_flags(
            old_db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| {
            StorageError::Database(format!(
                "cannot open old db {}: {e}",
                old_db_path.display()
            ))
        })?;

        let now = chrono::Utc::now().timestamp();

        // ── Read old tracks ───────────────────────────────────────────────────
        let tracks: Vec<wdmm_core::models::LibraryTrack> = {
            let mut stmt = old.prepare(
                "SELECT id, title, artist, remote_path, account_id, size, modified FROM tracks",
            ).map_err(|e| StorageError::Database(format!("tracks prepare: {e}")))?;

            let rows = stmt
                .query_map([], |row| {
                    Ok(wdmm_core::models::LibraryTrack {
                        id: row.get::<_, String>(0).unwrap_or_default(),
                        title: row.get::<_, String>(1).unwrap_or_default(),
                        artist: row.get::<_, Option<String>>(2).unwrap_or(None),
                        remote_path: row.get::<_, String>(3).unwrap_or_default(),
                        account_id: row.get::<_, String>(4).unwrap_or_default(),
                        size: row.get::<_, i64>(5).unwrap_or(0) as u64,
                        modified: row
                            .get::<_, Option<i64>>(6)
                            .unwrap_or(None)
                            .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0)),
                        ..Default::default()
                    })
                })
                .map_err(|e| StorageError::Database(format!("tracks query: {e}")))?;

            let mut out = Vec::new();
            for r in rows {
                match r {
                    Ok(t) => out.push(t),
                    Err(_) => {} // skip malformed rows defensively
                }
            }
            out
        };

        // ── Read old playlists ────────────────────────────────────────────────
        let playlists: Vec<Playlist> = {
            let mut stmt = old
                .prepare("SELECT id, name, created_at, updated_at FROM playlists")
                .map_err(|e| StorageError::Database(format!("playlists prepare: {e}")))?;

            let rows = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let name: String = row.get(1)?;
                    let created_ts: i64 = row.get::<_, i64>(2).unwrap_or(0);
                    let updated_ts: i64 = row.get::<_, i64>(3).unwrap_or(0);
                    let created_at = chrono::DateTime::from_timestamp(created_ts, 0)
                        .unwrap_or_else(chrono::Utc::now);
                    let updated_at = chrono::DateTime::from_timestamp(updated_ts, 0)
                        .unwrap_or_else(chrono::Utc::now);
                    Ok(Playlist { id, name, created_at, updated_at })
                })
                .map_err(|e| StorageError::Database(format!("playlists query: {e}")))?;

            let mut out = Vec::new();
            for r in rows {
                match r {
                    Ok(p) => out.push(p),
                    Err(_) => {}
                }
            }
            out
        };

        // ── Read old playlist_entries ─────────────────────────────────────────
        let entries: Vec<PlaylistEntry> = {
            let mut stmt = old
                .prepare(
                    "SELECT playlist_id, position, music_id, source_name, remote_path, title, duration_ms \
                     FROM playlist_entries",
                )
                .map_err(|e| StorageError::Database(format!("playlist_entries prepare: {e}")))?;

            let rows = stmt
                .query_map([], |row| {
                    Ok(PlaylistEntry {
                        playlist_id: row.get::<_, String>(0).unwrap_or_default(),
                        position: row.get::<_, i64>(1).unwrap_or(0),
                        music_id: row.get::<_, String>(2).unwrap_or_default(),
                        source_name: row.get::<_, String>(3).unwrap_or_default(),
                        remote_path: row.get::<_, String>(4).unwrap_or_default(),
                        title: row.get::<_, String>(5).unwrap_or_default(),
                        duration_ms: row.get::<_, i64>(6).unwrap_or(0),
                    })
                })
                .map_err(|e| StorageError::Database(format!("playlist_entries query: {e}")))?;

            let mut out = Vec::new();
            for r in rows {
                match r {
                    Ok(e) => out.push(e),
                    Err(_) => {}
                }
            }
            out
        };

        // ── Write everything to dest in one logical batch ─────────────────────
        let track_count = tracks.len();
        let playlist_count = playlists.len();
        let entry_count = entries.len();

        for t in &tracks {
            dest.upsert_track(t)?;
        }

        for p in &playlists {
            dest.upsert_playlist(p)?;
        }

        // Group entries by playlist_id and call set_playlist_entries per playlist.
        let mut by_playlist: std::collections::HashMap<String, Vec<PlaylistEntry>> =
            std::collections::HashMap::new();
        for e in entries {
            by_playlist.entry(e.playlist_id.clone()).or_default().push(e);
        }
        for (pid, mut ents) in by_playlist {
            ents.sort_by_key(|e| e.position);
            dest.set_playlist_entries(&pid, &ents)?;
        }

        // Mark as done.
        dest.save_kv("sqflite_imported", "1")?;

        // suppress unused-variable warning for `now`
        let _ = now;

        Ok(ImportSummary {
            already_done: false,
            tracks: track_count,
            playlists: playlist_count,
            entries: entry_count,
        })
    }

    // ── User folders ──────────────────────────────────────────────────────────

    /// List direct children of a folder.
    ///
    /// `parent_id = None`, `""`, or `"root"` all mean the virtual root —
    /// returns folders whose `parent_id IS NULL`.
    pub fn list_user_folder_children(
        &self,
        parent_id: Option<&str>,
    ) -> StorageResult<Vec<UserFolder>> {
        let normalized = normalize_parent(parent_id);
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, parent_id, sort_order, created_at
             FROM user_folders WHERE parent_id IS ?1
             ORDER BY sort_order, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![normalized], row_to_folder)?;
        rows.map(|r| r.map_err(StorageError::from)).collect()
    }

    /// Fetch a single folder by id. Returns `None` if not found.
    pub fn get_user_folder(&self, id: &str) -> StorageResult<Option<UserFolder>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, parent_id, sort_order, created_at
             FROM user_folders WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_folder(row)?))
        } else {
            Ok(None)
        }
    }

    /// Create a folder with a generated UUID id. `sort_order` defaults to 0.
    pub fn create_user_folder(
        &self,
        name: &str,
        parent_id: Option<&str>,
        sort_order: Option<i64>,
    ) -> StorageResult<UserFolder> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        let sort = sort_order.unwrap_or(0);
        let normalized = normalize_parent(parent_id);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user_folders (id, name, parent_id, sort_order, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, normalized, sort, now],
        )?;
        Ok(UserFolder {
            id,
            name: name.to_string(),
            parent_id: normalized.map(|s| s.to_string()),
            sort_order: sort,
            created_at: now,
        })
    }

    /// Rename a folder. Returns `NotFound` if the id is absent.
    pub fn rename_user_folder(&self, id: &str, name: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE user_folders SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        if n == 0 {
            return Err(StorageError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Move a folder to a new parent. `new_parent_id = None / "" / "root"`
    /// places it under the virtual root. Rejects moves that would form a cycle.
    pub fn move_user_folder(
        &self,
        id: &str,
        new_parent_id: Option<&str>,
    ) -> StorageResult<()> {
        let normalized = normalize_parent(new_parent_id);
        let conn = self.conn.lock().unwrap();
        // Cycle guard: new parent must not be the folder itself or a descendant.
        if let Some(target) = normalized {
            if target == id {
                return Err(StorageError::Database(
                    "move would create a cycle (target is self)".to_string(),
                ));
            }
            let desc = collect_descendants(&conn, id)?;
            if desc.iter().any(|d| d == target) {
                return Err(StorageError::Database(
                    "move would create a cycle".to_string(),
                ));
            }
        }
        let n = conn.execute(
            "UPDATE user_folders SET parent_id = ?1 WHERE id = ?2",
            params![normalized, id],
        )?;
        if n == 0 {
            return Err(StorageError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Delete a folder and recursively all descendants + their items.
    /// Deletion order is children-first; no FK cascade is assumed.
    pub fn delete_user_folder(&self, id: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let descendants = collect_descendants(&conn, id)?;
        let tx = conn.unchecked_transaction()?;
        // Deepest descendants first (collect_descendants is BFS so we reverse).
        for desc_id in descendants.iter().rev() {
            tx.execute(
                "DELETE FROM user_folder_items WHERE folder_id = ?1",
                params![desc_id],
            )?;
            tx.execute(
                "DELETE FROM user_folders WHERE id = ?1",
                params![desc_id],
            )?;
        }
        // Self items + self.
        tx.execute(
            "DELETE FROM user_folder_items WHERE folder_id = ?1",
            params![id],
        )?;
        tx.execute("DELETE FROM user_folders WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// List items in a folder. `folder_id = "root"` is valid (virtual root).
    pub fn list_user_folder_items(
        &self,
        folder_id: &str,
    ) -> StorageResult<Vec<UserFolderItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT folder_id, ref_kind, ref_key, sort_order, added_at
             FROM user_folder_items WHERE folder_id = ?1
             ORDER BY sort_order",
        )?;
        let rows = stmt.query_map(params![folder_id], row_to_folder_item)?;
        rows.map(|r| r.map_err(StorageError::from)).collect()
    }

    /// Insert an item (or replace its sort_order if the PK already exists).
    pub fn add_user_folder_item(&self, item: &UserFolderItem) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO user_folder_items
             (folder_id, ref_kind, ref_key, sort_order, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &item.folder_id,
                &item.ref_kind,
                &item.ref_key,
                item.sort_order,
                item.added_at,
            ],
        )?;
        Ok(())
    }

    /// Remove a specific item by (folder_id, ref_kind, ref_key).
    pub fn remove_user_folder_item(
        &self,
        folder_id: &str,
        ref_kind: &str,
        ref_key: &str,
    ) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM user_folder_items
             WHERE folder_id = ?1 AND ref_kind = ?2 AND ref_key = ?3",
            params![folder_id, ref_kind, ref_key],
        )?;
        Ok(())
    }

    /// Atomically replace all items in a folder (delete-all + re-insert in one
    /// transaction). Mirrors [`set_playlist_entries`] semantics. Only items
    /// belonging to `folder_id` are touched; other folders are unaffected.
    ///
    /// The `folder_id` field of each item in `items` is ignored — the method
    /// always binds the explicit `folder_id` argument to avoid cross-folder writes.
    pub fn set_user_folder_items(
        &self,
        folder_id: &str,
        items: &[UserFolderItem],
    ) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM user_folder_items WHERE folder_id = ?1",
            params![folder_id],
        )?;
        for item in items {
            tx.execute(
                "INSERT INTO user_folder_items
                 (folder_id, ref_kind, ref_key, sort_order, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    folder_id,
                    &item.ref_kind,
                    &item.ref_key,
                    item.sort_order,
                    item.added_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ── Manga (schema v4) ─────────────────────────────────────────────────────

    pub fn list_mangas(&self) -> StorageResult<Vec<Manga>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, source_name, remote_path, title, author, series, volume,
                    chapter_number, page_count, cover_relpath, format,
                    remote_missing, page_turn, rev, updated_at
             FROM mangas ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_manga)?;
        rows.map(|r| r.map_err(StorageError::from)).collect()
    }

    pub fn get_manga(&self, id: &str) -> StorageResult<Option<Manga>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, source_name, remote_path, title, author, series, volume,
                    chapter_number, page_count, cover_relpath, format,
                    remote_missing, page_turn, rev, updated_at
             FROM mangas WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_manga(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn upsert_manga(&self, m: &Manga) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO mangas
             (id, source_name, remote_path, title, author, series, volume,
              chapter_number, page_count, cover_relpath, format,
              remote_missing, page_turn, rev, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                &m.id, &m.source_name, &m.remote_path, &m.title,
                &m.author, &m.series, &m.volume, &m.chapter_number,
                m.page_count, &m.cover_relpath, &m.format, m.remote_missing,
                &m.page_turn, m.rev, &m.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Delete a manga. Cascades to `manga_pages` and `manga_progress`. Appends
    /// a tombstone row to `deleted_mangas` using `source_name:remote_path` as
    /// the `sync_key` and the current UTC timestamp for `deleted_at`.
    pub fn delete_manga(&self, id: &str) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        // Look up the manga to build the tombstone.
        let (source_name, remote_path, rev) = {
            let mut stmt = conn.prepare(
                "SELECT source_name, remote_path, rev FROM mangas WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(r) => (
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ),
                None => return Err(StorageError::NotFound(id.to_string())),
            }
        };
        let sync_key = format!("{}:{}", source_name, remote_path);
        let deleted_at = Utc::now().to_rfc3339();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO deleted_mangas
             (manga_id, source_name, remote_path, sync_key, rev, deleted_at, pushed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![id, &source_name, &remote_path, &sync_key, rev, &deleted_at],
        )?;
        // ON DELETE CASCADE removes manga_pages and manga_progress.
        tx.execute("DELETE FROM mangas WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_pages(&self, manga_id: &str) -> StorageResult<Vec<MangaPage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT manga_id, page_index, rel_path
             FROM manga_pages WHERE manga_id = ?1 ORDER BY page_index",
        )?;
        let rows = stmt.query_map(params![manga_id], |row| {
            Ok(MangaPage {
                manga_id: row.get(0)?,
                page_index: row.get(1)?,
                rel_path: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    /// Atomically replace all pages for a manga.
    pub fn replace_pages(&self, manga_id: &str, pages: &[MangaPage]) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM manga_pages WHERE manga_id = ?1", params![manga_id])?;
        for p in pages {
            tx.execute(
                "INSERT INTO manga_pages (manga_id, page_index, rel_path) VALUES (?1, ?2, ?3)",
                params![&p.manga_id, p.page_index, &p.rel_path],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_progress(&self, manga_id: &str) -> StorageResult<Option<MangaProgress>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT manga_id, page_index, updated_at
             FROM manga_progress WHERE manga_id = ?1",
        )?;
        let mut rows = stmt.query(params![manga_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(MangaProgress {
                manga_id: row.get(0)?,
                page_index: row.get(1)?,
                updated_at: row.get(2)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn set_progress(&self, manga_id: &str, page_index: i64) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let updated_at = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR REPLACE INTO manga_progress (manga_id, page_index, updated_at)
             VALUES (?1, ?2, ?3)",
            params![manga_id, page_index, &updated_at],
        )?;
        Ok(())
    }

    pub fn list_progress(&self) -> StorageResult<Vec<MangaProgress>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT manga_id, page_index, updated_at
             FROM manga_progress ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(MangaProgress {
                manga_id: row.get(0)?,
                page_index: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    /// Return distinct non-null series tags with their derived IDs.
    pub fn list_series_tags(&self) -> StorageResult<Vec<SeriesTag>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT series FROM mangas
             WHERE series IS NOT NULL ORDER BY series COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            let title = r?;
            out.push(SeriesTag { id: series_id_for_title(&title), title });
        }
        Ok(out)
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

// ── User folder models (schema v3) ─────────────────────────────────────────

/// A user-created folder in the library hierarchy.
///
/// `parent_id = None` means the folder is a direct child of the virtual root
/// (`我的文件夹` / Android `kUserFolderRootId = "root"`). The virtual root has
/// no DB row; its leaf references live in `user_folder_items` with
/// `folder_id = "root"`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserFolder {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub sort_order: i64,
    /// Unix timestamp (seconds).
    pub created_at: i64,
}

/// An item reference within a user folder.
///
/// `folder_id` may be `"root"` (the virtual root; no corresponding DB row).
/// `ref_kind` is one of `"album"`, `"artist"`, `"genre"`, `"track"`, `"manga"`,
/// or `"mangaSeries"` (mirrors Android `UserFolderRefKind`). Storage accepts any
/// non-empty string; the above values are the canonical Android set.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserFolderItem {
    pub folder_id: String,
    pub ref_kind: String,
    pub ref_key: String,
    pub sort_order: i64,
    /// Unix timestamp (seconds).
    pub added_at: i64,
}

fn ts_to_dt(ts: i64) -> StorageResult<DateTime<Utc>> {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Utc))
        .ok_or_else(|| StorageError::Database(format!("invalid timestamp {ts}")))
}

/// Map a rusqlite `Row` (15-column manga select) to a [`Manga`].
fn row_to_manga(row: &rusqlite::Row<'_>) -> rusqlite::Result<Manga> {
    Ok(Manga {
        id:             row.get(0)?,
        source_name:    row.get(1)?,
        remote_path:    row.get(2)?,
        title:          row.get(3)?,
        author:         row.get(4)?,
        series:         row.get(5)?,
        volume:         row.get(6)?,
        chapter_number: row.get(7)?,
        page_count:     row.get(8)?,
        cover_relpath:  row.get(9)?,
        format:         row.get(10)?,
        remote_missing: row.get(11)?,
        page_turn:      row.get(12)?,
        rev:            row.get(13)?,
        updated_at:     row.get(14)?,
    })
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

fn row_to_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserFolder> {
    Ok(UserFolder {
        id: row.get(0)?,
        name: row.get(1)?,
        parent_id: row.get(2)?,
        sort_order: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn row_to_folder_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserFolderItem> {
    Ok(UserFolderItem {
        folder_id: row.get(0)?,
        ref_kind: row.get(1)?,
        ref_key: row.get(2)?,
        sort_order: row.get(3)?,
        added_at: row.get(4)?,
    })
}

/// Normalise virtual-root sentinels (`None`, `""`, `"root"`) to `None`
/// so SQL can use `parent_id IS NULL`.
fn normalize_parent(parent_id: Option<&str>) -> Option<&str> {
    match parent_id {
        None | Some("") | Some("root") => None,
        Some(s) => Some(s),
    }
}

/// BFS-collect all descendant folder IDs of `root_id` (not including root).
fn collect_descendants(conn: &Connection, root_id: &str) -> StorageResult<Vec<String>> {
    let mut result = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root_id.to_string());
    while let Some(parent) = queue.pop_front() {
        let mut stmt = conn.prepare_cached(
            "SELECT id FROM user_folders WHERE parent_id = ?1",
        )?;
        let ids: Vec<String> = stmt
            .query_map(params![parent], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        for child_id in ids {
            queue.push_back(child_id.clone());
            result.push(child_id);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_v4() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(db.schema_version().unwrap(), 4);
    }

    #[test]
    fn manga_stubs_dropped() {
        // The v4 migration must remove the obsolete manga_series / manga_chapters
        // stub tables from older schemas.
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let conn = db.conn.lock().unwrap();
        let exists = |name: &str| -> bool {
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
                > 0
        };
        assert!(!exists("manga_series"), "manga_series must be dropped");
        assert!(!exists("manga_chapters"), "manga_chapters must be dropped");
        assert!(exists("mangas"));
        assert!(exists("manga_pages"));
        assert!(exists("manga_progress"));
        assert!(exists("deleted_mangas"));
    }

    #[test]
    fn manga_crud_and_pages_and_progress() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();

        let mk = |i: usize, src: &str, path: &str, fmt: &str| Manga {
            id: format!("m-{i}"),
            source_name: src.into(),
            remote_path: path.into(),
            title: format!("Title {i}"),
            author: Some("Author".into()),
            series: Some("Saga One".into()),
            volume: Some("1".into()),
            chapter_number: Some(format!("{i}")),
            page_count: 2,
            cover_relpath: Some("cover.jpg".into()),
            format: fmt.into(),
            remote_missing: 0,
            page_turn: Some("ltr".into()),
            rev: 0,
            updated_at: now.clone(),
        };

        // upsert two mangas sharing the same series tag
        db.upsert_manga(&mk(1, "nas", "/m/a.cbz", "cbz")).unwrap();
        db.upsert_manga(&mk(2, "nas", "/m/b.cbz", "cbz")).unwrap();
        assert_eq!(db.list_mangas().unwrap().len(), 2);
        assert_eq!(db.get_manga("m-1").unwrap().unwrap().title, "Title 1");
        assert!(db.get_manga("nope").unwrap().is_none());

        // partial unique index: two 'cbz' entries with same (source, path) collapse
        db.upsert_manga(&mk(1, "nas", "/m/a.cbz", "cbz")).unwrap();
        assert_eq!(db.list_mangas().unwrap().len(), 2);
        // 'images' format is exempt from the unique constraint
        db.upsert_manga(&mk(3, "nas", "/m/b.cbz", "images")).unwrap();
        assert_eq!(db.list_mangas().unwrap().len(), 3);

        // pages replace
        let pages = vec![
            MangaPage { manga_id: "m-1".into(), page_index: 0, rel_path: "p0.jpg".into() },
            MangaPage { manga_id: "m-1".into(), page_index: 1, rel_path: "p1.jpg".into() },
        ];
        db.replace_pages("m-1", &pages).unwrap();
        assert_eq!(db.list_pages("m-1").unwrap().len(), 2);
        db.replace_pages("m-1", &[MangaPage {
            manga_id: "m-1".into(),
            page_index: 0,
            rel_path: "only.jpg".into(),
        }])
        .unwrap();
        assert_eq!(db.list_pages("m-1").unwrap().len(), 1);

        // progress round-trip
        assert!(db.get_progress("m-1").unwrap().is_none());
        db.set_progress("m-1", 1).unwrap();
        assert_eq!(db.get_progress("m-1").unwrap().unwrap().page_index, 1);
        db.set_progress("m-1", 0).unwrap();
        assert_eq!(db.get_progress("m-1").unwrap().unwrap().page_index, 0);
        assert_eq!(db.list_progress().unwrap().len(), 1);

        // series derived
        let series = db.list_series_tags().unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].title, "Saga One");
        assert_eq!(series[0].id, series_id_for_title("Saga One"));

        // delete cascades pages+progress and writes a tombstone
        db.delete_manga("m-1").unwrap();
        assert!(db.get_manga("m-1").unwrap().is_none());
        assert!(db.list_pages("m-1").unwrap().is_empty());
        assert!(db.get_progress("m-1").unwrap().is_none());
        // verify the deleted_mangas tombstone
        let conn = db.conn.lock().unwrap();
        let (sync_key, pushed): (String, i64) = conn
            .query_row(
                "SELECT sync_key, pushed FROM deleted_mangas WHERE manga_id = ?1",
                params!["m-1"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(sync_key, "nas:/m/a.cbz");
        assert_eq!(pushed, 0);
        drop(conn);

        // series tag persists for the remaining manga
        assert_eq!(db.list_series_tags().unwrap().len(), 1);
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

    #[test]
    fn import_from_sqflite_roundtrip() {
        // Build old DB in a temp file (open_with_flags requires a real path).
        let mut old_path = std::env::temp_dir();
        old_path.push("wdmm_sqflite_import_test.db");

        {
            let old = Connection::open(&old_path).expect("create old db");
            old.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS tracks (
                    id          TEXT PRIMARY KEY,
                    title       TEXT NOT NULL,
                    artist      TEXT,
                    remote_path TEXT NOT NULL,
                    account_id  TEXT NOT NULL,
                    size        INTEGER NOT NULL DEFAULT 0,
                    modified    INTEGER
                );
                CREATE TABLE IF NOT EXISTS playlists (
                    id         TEXT PRIMARY KEY,
                    name       TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS playlist_entries (
                    playlist_id TEXT    NOT NULL,
                    position    INTEGER NOT NULL,
                    music_id    TEXT    NOT NULL,
                    source_name TEXT    NOT NULL,
                    remote_path TEXT    NOT NULL,
                    title       TEXT    NOT NULL,
                    duration_ms INTEGER NOT NULL DEFAULT 0
                );

                INSERT INTO tracks VALUES ('t1','Song One','Artist A','/m/1.mp3','acc1',1000,NULL);
                INSERT INTO tracks VALUES ('t2','Song Two','Artist B','/m/2.mp3','acc1',2000,NULL);
                INSERT INTO tracks VALUES ('t3','Song Three',NULL,'/m/3.mp3','acc2',3000,NULL);

                INSERT INTO playlists VALUES ('pl1','My Playlist',1700000000,1700000001);
                INSERT INTO playlists VALUES ('pl2','Another',1700000002,1700000003);

                INSERT INTO playlist_entries VALUES ('pl1',0,'t1','acc1','/m/1.mp3','Song One',180000);
                INSERT INTO playlist_entries VALUES ('pl1',1,'t2','acc1','/m/2.mp3','Song Two',240000);
                INSERT INTO playlist_entries VALUES ('pl2',0,'t3','acc2','/m/3.mp3','Song Three',300000);
                "#,
            )
            .expect("seed old db");
        }

        let dest = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();

        // First import.
        let summary = LibraryDatabase::import_from_sqflite(&old_path, &dest).unwrap();
        assert!(!summary.already_done);
        assert_eq!(summary.tracks, 3);
        assert_eq!(summary.playlists, 2);
        assert_eq!(summary.entries, 3);

        // Verify data landed.
        assert_eq!(dest.list_tracks().unwrap().len(), 3);
        assert_eq!(dest.list_playlists().unwrap().len(), 2);
        assert_eq!(dest.list_playlist_entries("pl1").unwrap().len(), 2);

        // Second call must be a no-op.
        let summary2 = LibraryDatabase::import_from_sqflite(&old_path, &dest).unwrap();
        assert!(summary2.already_done);
        assert_eq!(summary2.tracks, 0);

        // Clean up temp file (best-effort).
        let _ = std::fs::remove_file(&old_path);
    }

    // ── User folder tests (schema v3) ─────────────────────────────────────────

    #[test]
    fn user_folder_tree_crud() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();

        // Virtual root is empty initially.
        assert!(db.list_user_folder_children(None).unwrap().is_empty());
        assert!(db.list_user_folder_children(Some("root")).unwrap().is_empty());
        assert!(db.list_user_folder_children(Some("")).unwrap().is_empty());

        // Create two root-level folders.
        let f1 = db.create_user_folder("Music", None, None).unwrap();
        let f2 = db.create_user_folder("Playlists", None, Some(1)).unwrap();
        assert_eq!(db.list_user_folder_children(None).unwrap().len(), 2);

        // Create a child under f1.
        let f3 = db.create_user_folder("Rock", Some(&f1.id), None).unwrap();
        let children = db.list_user_folder_children(Some(&f1.id)).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "Rock");

        // get_user_folder returns correct fields.
        let fetched = db.get_user_folder(&f2.id).unwrap().unwrap();
        assert_eq!(fetched.name, "Playlists");
        assert_eq!(fetched.sort_order, 1);
        assert!(db.get_user_folder("nonexistent").unwrap().is_none());

        // Rename.
        db.rename_user_folder(&f1.id, "Musik").unwrap();
        assert_eq!(db.get_user_folder(&f1.id).unwrap().unwrap().name, "Musik");
        // NotFound.
        assert!(db.rename_user_folder("bad-id", "X").is_err());

        // Move f3 from under f1 to under f2.
        db.move_user_folder(&f3.id, Some(&f2.id)).unwrap();
        assert!(db.list_user_folder_children(Some(&f1.id)).unwrap().is_empty());
        assert_eq!(db.list_user_folder_children(Some(&f2.id)).unwrap().len(), 1);

        // Move to virtual root using "" sentinel.
        db.move_user_folder(&f3.id, Some("")).unwrap();
        assert_eq!(db.list_user_folder_children(None).unwrap().len(), 3);

        // Cycle rejection: create f4 under f1, then try to move f1 under f4.
        let f4 = db.create_user_folder("Sub", Some(&f1.id), None).unwrap();
        let cycle = db.move_user_folder(&f1.id, Some(&f4.id));
        assert!(cycle.is_err(), "move into own descendant must fail");
        // Self-move also rejected.
        assert!(db.move_user_folder(&f1.id, Some(&f1.id)).is_err());

        // Recursive delete: f1 and f4 (child of f1) both removed.
        db.delete_user_folder(&f1.id).unwrap();
        assert!(db.get_user_folder(&f1.id).unwrap().is_none());
        assert!(db.get_user_folder(&f4.id).unwrap().is_none());
    }

    #[test]
    fn user_folder_items_crud() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let folder = db.create_user_folder("Favs", None, None).unwrap();
        let now = Utc::now().timestamp();

        // Add item to virtual root (folder_id = "root").
        db.add_user_folder_item(&UserFolderItem {
            folder_id: "root".to_string(),
            ref_kind: "track".to_string(),
            ref_key: "sha1abc".to_string(),
            sort_order: 0,
            added_at: now,
        })
        .unwrap();

        // Add item to named folder.
        db.add_user_folder_item(&UserFolderItem {
            folder_id: folder.id.clone(),
            ref_kind: "album".to_string(),
            ref_key: "alb-001".to_string(),
            sort_order: 0,
            added_at: now,
        })
        .unwrap();

        let root_items = db.list_user_folder_items("root").unwrap();
        assert_eq!(root_items.len(), 1);
        assert_eq!(root_items[0].ref_kind, "track");

        let folder_items = db.list_user_folder_items(&folder.id).unwrap();
        assert_eq!(folder_items.len(), 1);
        assert_eq!(folder_items[0].ref_key, "alb-001");

        // Remove item from root.
        db.remove_user_folder_item("root", "track", "sha1abc").unwrap();
        assert!(db.list_user_folder_items("root").unwrap().is_empty());

        // Delete folder — items inside are cleaned up too.
        db.delete_user_folder(&folder.id).unwrap();
        assert!(db.list_user_folder_items(&folder.id).unwrap().is_empty());
    }

    #[test]
    fn user_folder_recursive_delete_removes_items() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now().timestamp();

        let parent = db.create_user_folder("P", None, None).unwrap();
        let child = db.create_user_folder("C", Some(&parent.id), None).unwrap();

        db.add_user_folder_item(&UserFolderItem {
            folder_id: child.id.clone(),
            ref_kind: "artist".to_string(),
            ref_key: "art-x".to_string(),
            sort_order: 0,
            added_at: now,
        })
        .unwrap();

        // Delete parent → child and its items must also vanish.
        db.delete_user_folder(&parent.id).unwrap();
        assert!(db.get_user_folder(&child.id).unwrap().is_none());
        assert!(db.list_user_folder_items(&child.id).unwrap().is_empty());
    }

    #[test]
    fn playlist_crud_unchanged() {
        // Quick sanity: existing playlist API still works after schema v3.
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now();
        let pl = Playlist {
            id: "pl-v3".into(),
            name: "v3 Test".into(),
            created_at: now,
            updated_at: now,
        };
        db.upsert_playlist(&pl).unwrap();
        assert_eq!(db.get_playlist("pl-v3").unwrap().unwrap().name, "v3 Test");
        db.delete_playlist("pl-v3").unwrap();
        assert!(db.get_playlist("pl-v3").unwrap().is_none());
    }
}
