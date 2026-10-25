//! `wdmm-storage` — rusqlite library DB (schema v6, music + manga library).
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
//! Schema v5: CUE sheet support (`cue_albums`, `cue_slices` tables).
//! Schema v6: Music tombstones (`deleted_tracks`) + sync state cursors (`sync_state`).

#![forbid(unsafe_code)]

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
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

/// Report from [`LibraryDatabase::compare_with_sqflite`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompareReport {
    pub tracks: TableDiff,
    pub deleted_tracks: TableDiff,
    pub sync_state: TableDiff,
    /// `true` iff all tables match on compared fields.
    pub ok: bool,
}

/// Diff for one table.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TableDiff {
    /// Row count in sqflite (left).
    pub left_count: usize,
    /// Row count in Core (right).
    pub right_count: usize,
    /// Keys present in sqflite but missing in Core.
    pub only_left: Vec<String>,
    /// Keys present in Core but missing in sqflite.
    pub only_right: Vec<String>,
    /// Field mismatches for keys present in both (e.g. "src|path: rev 1!=2").
    pub field_mismatches: Vec<String>,
}

impl TableDiff {
    fn is_ok(&self) -> bool {
        self.left_count == self.right_count
            && self.only_left.is_empty()
            && self.only_right.is_empty()
            && self.field_mismatches.is_empty()
    }
}

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

/// Tombstone appended to `deleted_tracks` when a music track is deleted (v6).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeletedTrack {
    pub source_name: String,
    pub remote_path: String,
    pub rev: i64,
    /// RFC-3339 timestamp string.
    pub deleted_at: String,
    /// `1` once the tombstone has been pushed to the sync server.
    pub pushed: i64,
}

/// Sync cursor state for a remote key (v6).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncState {
    pub remote_key: String,
    pub last_seq: i64,
    pub base_up_to: i64,
    /// JSON object string (stored as TEXT in DB).
    pub parts: String,
    /// RFC-3339 timestamp string.
    pub updated_at: String,
}

/// A CUE sheet album (v5).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CueAlbum {
    pub cue_id: String,
    pub source_name: String,
    pub cue_remote_path: String,
    pub title: Option<String>,
    pub performer: Option<String>,
    pub cache_group_id: Option<String>,
    /// RFC-3339 timestamp string.
    pub created_at: String,
}

/// A CUE sheet slice (virtual track) (v5).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CueSlice {
    pub music_id: String,
    pub cue_id: String,
    pub audio_music_id: String,
    pub source_name: String,
    pub remote_path: String,
    pub file_name: String,
    pub track_index: i64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<i64>,
    pub track_number: Option<i64>,
    pub track_total: Option<i64>,
    pub disc_number: Option<i64>,
    pub disc_total: Option<i64>,
    pub year: Option<i64>,
    pub genre: Option<String>,
    pub bitrate: Option<i64>,
    pub sample_rate: Option<i64>,
    pub cover_path: Option<String>,
    pub audio_remote_path: Option<String>,
    pub clip_start_ms: Option<i64>,
    pub clip_end_ms: Option<i64>,
    pub cache_group_id: Option<String>,
    pub rev: i64,
    pub last_downloaded_at: String,
    pub last_tag_read_at: String,
    pub remote_missing: i64,
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

        // ── v5 migration: CUE sheet support ───────────────────────────────────
        if current_ver < 5 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS cue_albums (
                  cue_id TEXT PRIMARY KEY,
                  source_name TEXT NOT NULL,
                  cue_remote_path TEXT NOT NULL,
                  title TEXT,
                  performer TEXT,
                  cache_group_id TEXT,
                  created_at TEXT NOT NULL,
                  UNIQUE(source_name, cue_remote_path)
                );

                CREATE TABLE IF NOT EXISTS cue_slices (
                  music_id TEXT PRIMARY KEY,
                  cue_id TEXT NOT NULL,
                  audio_music_id TEXT NOT NULL,
                  source_name TEXT NOT NULL,
                  remote_path TEXT NOT NULL,
                  file_name TEXT NOT NULL,
                  track_index INTEGER NOT NULL,
                  title TEXT,
                  artist TEXT,
                  album_artist TEXT,
                  album TEXT,
                  duration_ms INTEGER,
                  track_number INTEGER,
                  track_total INTEGER,
                  disc_number INTEGER,
                  disc_total INTEGER,
                  year INTEGER,
                  genre TEXT,
                  bitrate INTEGER,
                  sample_rate INTEGER,
                  cover_path TEXT,
                  audio_remote_path TEXT,
                  clip_start_ms INTEGER,
                  clip_end_ms INTEGER,
                  cache_group_id TEXT,
                  rev INTEGER NOT NULL DEFAULT 0,
                  last_downloaded_at TEXT NOT NULL DEFAULT '',
                  last_tag_read_at TEXT NOT NULL DEFAULT '',
                  remote_missing INTEGER NOT NULL DEFAULT 0,
                  UNIQUE(source_name, remote_path),
                  FOREIGN KEY (cue_id) REFERENCES cue_albums(cue_id) ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS idx_cue_slices_cue ON cue_slices(cue_id);
                CREATE INDEX IF NOT EXISTS idx_cue_albums_source ON cue_albums(source_name);
                "#,
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO _schema_version (version) VALUES (5)",
                [],
            )?;
        }

        // ── v6 migration: music tombstones + sync state ───────────────────────
        if current_ver < 6 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS deleted_tracks (
                  source_name TEXT NOT NULL,
                  remote_path TEXT NOT NULL,
                  rev INTEGER NOT NULL,
                  deleted_at TEXT NOT NULL,
                  pushed INTEGER NOT NULL DEFAULT 0,
                  PRIMARY KEY (source_name, remote_path)
                );

                CREATE TABLE IF NOT EXISTS sync_state (
                  remote_key TEXT PRIMARY KEY,
                  last_seq INTEGER NOT NULL DEFAULT 0,
                  base_up_to INTEGER NOT NULL DEFAULT 0,
                  parts TEXT NOT NULL DEFAULT '{}',
                  updated_at TEXT NOT NULL
                );
                "#,
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO _schema_version (version) VALUES (6)",
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

    // ── Track deletion by account (v5) ────────────────────────────────────────

    /// Delete all tracks for a given account_id. Returns number of rows deleted.
    pub fn delete_tracks_by_account(&self, account_id: &str) -> StorageResult<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM tracks WHERE account_id = ?1",
            params![account_id],
        )?;
        Ok(n)
    }

    // ── Clear operations (v5) ─────────────────────────────────────────────────

    /// Clear all library-persisted data except accounts and non-library KV keys.
    ///
    /// Clears: tracks, albums, artists, cue_albums, cue_slices, playlists,
    /// playlist_entries, user_folders, user_folder_items, mangas, manga_pages,
    /// manga_progress, deleted_mangas, deleted tombstone table.
    ///
    /// Does NOT delete: accounts, kv_store.
    pub fn clear_library_all(&self) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;

        // Order matters: child tables first to satisfy FK constraints.
        tx.execute("DELETE FROM playlist_entries", [])?;
        tx.execute("DELETE FROM playlists", [])?;
        tx.execute("DELETE FROM user_folder_items", [])?;
        tx.execute("DELETE FROM user_folders", [])?;
        tx.execute("DELETE FROM cue_slices", [])?;
        tx.execute("DELETE FROM cue_albums", [])?;
        tx.execute("DELETE FROM tracks", [])?;
        tx.execute("DELETE FROM albums", [])?;
        tx.execute("DELETE FROM artists", [])?;
        tx.execute("DELETE FROM manga_pages", [])?;
        tx.execute("DELETE FROM manga_progress", [])?;
        tx.execute("DELETE FROM mangas", [])?;
        tx.execute("DELETE FROM deleted_mangas", [])?;
        tx.execute("DELETE FROM deleted_tracks", [])?;
        tx.execute("DELETE FROM deleted", [])?;

        tx.commit()?;
        Ok(())
    }

    /// Clear library index: tracks, CUE tables, and track tombstones.
    ///
    /// Does NOT clear: playlists, user_folders, mangas, accounts.
    pub fn clear_library_index(&self) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;

        tx.execute("DELETE FROM cue_slices", [])?;
        tx.execute("DELETE FROM cue_albums", [])?;
        tx.execute("DELETE FROM tracks", [])?;
        tx.execute("DELETE FROM deleted_tracks", [])?;
        tx.execute("DELETE FROM deleted", [])?;

        tx.commit()?;
        Ok(())
    }

    // ── CUE operations (v5) ───────────────────────────────────────────────────

    pub fn upsert_cue_album(&self, album: &CueAlbum) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO cue_albums
             (cue_id, source_name, cue_remote_path, title, performer, cache_group_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &album.cue_id,
                &album.source_name,
                &album.cue_remote_path,
                &album.title,
                &album.performer,
                &album.cache_group_id,
                &album.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_cue_slice(&self, slice: &CueSlice) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO cue_slices
             (music_id, cue_id, audio_music_id, source_name, remote_path, file_name,
              track_index, title, artist, album_artist, album, duration_ms,
              track_number, track_total, disc_number, disc_total, year, genre,
              bitrate, sample_rate, cover_path, audio_remote_path,
              clip_start_ms, clip_end_ms, cache_group_id, rev,
              last_downloaded_at, last_tag_read_at, remote_missing)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29)",
            params![
                &slice.music_id,
                &slice.cue_id,
                &slice.audio_music_id,
                &slice.source_name,
                &slice.remote_path,
                &slice.file_name,
                slice.track_index,
                &slice.title,
                &slice.artist,
                &slice.album_artist,
                &slice.album,
                slice.duration_ms,
                slice.track_number,
                slice.track_total,
                slice.disc_number,
                slice.disc_total,
                slice.year,
                &slice.genre,
                slice.bitrate,
                slice.sample_rate,
                &slice.cover_path,
                &slice.audio_remote_path,
                slice.clip_start_ms,
                slice.clip_end_ms,
                &slice.cache_group_id,
                slice.rev,
                &slice.last_downloaded_at,
                &slice.last_tag_read_at,
                slice.remote_missing,
            ],
        )?;
        Ok(())
    }

    /// Delete a CUE slice by music_id. Returns number of rows deleted.
    pub fn delete_cue_slice(&self, music_id: &str) -> StorageResult<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM cue_slices WHERE music_id = ?1",
            params![music_id],
        )?;
        Ok(n)
    }

    /// Delete a CUE album and its slices (CASCADE handles slices).
    pub fn delete_cue_album(
        &self,
        source_name: &str,
        cue_remote_path: &str,
    ) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM cue_albums WHERE source_name = ?1 AND cue_remote_path = ?2",
            params![source_name, cue_remote_path],
        )?;
        Ok(())
    }

    /// Count remaining slices for a CUE album.
    pub fn remaining_slices_for_cue(
        &self,
        source_name: &str,
        cue_remote_path: &str,
    ) -> StorageResult<i64> {
        let conn = self.conn.lock().unwrap();
        // Find the cue_id first
        let cue_id: Option<String> = conn
            .query_row(
                "SELECT cue_id FROM cue_albums WHERE source_name = ?1 AND cue_remote_path = ?2",
                params![source_name, cue_remote_path],
                |r| r.get(0),
            )
            .optional()?;

        match cue_id {
            Some(id) => {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM cue_slices WHERE cue_id = ?1",
                    params![id],
                    |r| r.get(0),
                )?;
                Ok(count)
            }
            None => Ok(0),
        }
    }

    // ── Deleted tracks (v6 tombstones) ────────────────────────────────────────

    pub fn upsert_deleted_track(&self, track: &DeletedTrack) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO deleted_tracks
             (source_name, remote_path, rev, deleted_at, pushed)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &track.source_name,
                &track.remote_path,
                track.rev,
                &track.deleted_at,
                track.pushed,
            ],
        )?;
        Ok(())
    }

    pub fn clear_deleted_track(
        &self,
        source_name: &str,
        remote_path: &str,
    ) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM deleted_tracks WHERE source_name = ?1 AND remote_path = ?2",
            params![source_name, remote_path],
        )?;
        Ok(())
    }

    pub fn list_deleted_tracks(&self) -> StorageResult<Vec<DeletedTrack>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT source_name, remote_path, rev, deleted_at, pushed
             FROM deleted_tracks ORDER BY rev ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DeletedTrack {
                source_name: row.get(0)?,
                remote_path: row.get(1)?,
                rev: row.get(2)?,
                deleted_at: row.get(3)?,
                pushed: row.get(4)?,
            })
        })?;
        let mut result = Vec::new();
        for r in rows {
            result.push(r?);
        }
        Ok(result)
    }

    pub fn list_unpushed_deleted_tracks(&self) -> StorageResult<Vec<DeletedTrack>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT source_name, remote_path, rev, deleted_at, pushed
             FROM deleted_tracks WHERE pushed = 0 ORDER BY rev ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DeletedTrack {
                source_name: row.get(0)?,
                remote_path: row.get(1)?,
                rev: row.get(2)?,
                deleted_at: row.get(3)?,
                pushed: row.get(4)?,
            })
        })?;
        let mut result = Vec::new();
        for r in rows {
            result.push(r?);
        }
        Ok(result)
    }

    pub fn mark_deleted_tracks_pushed(&self, revs: &[i64]) -> StorageResult<()> {
        if revs.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        for rev in revs {
            tx.execute(
                "UPDATE deleted_tracks SET pushed = 1 WHERE rev = ?1",
                params![rev],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn purge_deleted_tracks_up_to(&self, up_to: i64) -> StorageResult<usize> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM deleted_tracks WHERE pushed = 1 AND rev <= ?1",
            params![up_to],
        )?;
        Ok(deleted)
    }

    pub fn tombstoned_remote_paths(&self, source_name: &str) -> StorageResult<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT remote_path FROM deleted_tracks WHERE source_name = ?1",
        )?;
        let rows = stmt.query_map(params![source_name], |row| row.get(0))?;
        let mut result = Vec::new();
        for r in rows {
            result.push(r?);
        }
        Ok(result)
    }

    // ── Sync state (v6 cursors) ───────────────────────────────────────────────

    pub fn get_sync_state(&self, remote_key: &str) -> StorageResult<Option<SyncState>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT remote_key, last_seq, base_up_to, parts, updated_at
             FROM sync_state WHERE remote_key = ?1",
            params![remote_key],
            |row| {
                Ok(SyncState {
                    remote_key: row.get(0)?,
                    last_seq: row.get(1)?,
                    base_up_to: row.get(2)?,
                    parts: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(StorageError::from)
    }

    pub fn save_sync_state(&self, state: &SyncState) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO sync_state
             (remote_key, last_seq, base_up_to, parts, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &state.remote_key,
                state.last_seq,
                state.base_up_to,
                &state.parts,
                &state.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn clear_sync_state(&self, prefix: Option<&str>) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        match prefix {
            None => {
                conn.execute("DELETE FROM sync_state", [])?;
            }
            Some(p) => {
                let pattern = format!("{}%", p);
                conn.execute(
                    "DELETE FROM sync_state WHERE remote_key LIKE ?1",
                    params![pattern],
                )?;
            }
        }
        Ok(())
    }

    // ── Migration / verify tooling ─────────────────────────────────────────────

    /// Compare an Android-schema sqflite DB against this Core DB.
    ///
    /// Reads the old DB at `old_db_path` and compares tables: tracks,
    /// deleted_tracks, sync_state. Does not import or mutate either DB.
    ///
    /// Returns a report showing row counts, missing/extra keys, and field mismatches.
    pub fn compare_with_sqflite(
        old_db_path: &std::path::Path,
        dest: &LibraryDatabase,
    ) -> StorageResult<CompareReport> {
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

        let tracks_diff = compare_tracks(&old, dest)?;
        let deleted_tracks_diff = compare_deleted_tracks(&old, dest)?;
        let sync_state_diff = compare_sync_state(&old, dest)?;

        let ok = tracks_diff.is_ok()
            && deleted_tracks_diff.is_ok()
            && sync_state_diff.is_ok();

        Ok(CompareReport {
            tracks: tracks_diff,
            deleted_tracks: deleted_tracks_diff,
            sync_state: sync_state_diff,
            ok,
        })
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

// ── Comparison helpers ───────────────────────────────────────────────────────

fn compare_tracks(
    old: &Connection,
    dest: &LibraryDatabase,
) -> StorageResult<TableDiff> {
    use std::collections::{HashMap, HashSet};

    // Read sqflite tracks
    let left_map: HashMap<String, (String, String, i64)> = {
        // Check if table exists
        let exists: bool = old
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tracks'",
                [],
                |row| row.get::<_, i64>(0).map(|c| c > 0),
            )
            .unwrap_or(false);

        if !exists {
            HashMap::new()
        } else {
            // Detect columns via PRAGMA table_info
            let mut has_source_name = false;
            let mut has_account_id = false;
            {
                let mut stmt = old.prepare("PRAGMA table_info(tracks)")
                    .map_err(|e| StorageError::Database(format!("PRAGMA table_info: {e}")))?;
                let rows = stmt.query_map([], |row| {
                    let col_name: String = row.get(1)?;
                    Ok(col_name)
                })?;
                for r in rows {
                    if let Ok(name) = r {
                        if name == "source_name" {
                            has_source_name = true;
                        } else if name == "account_id" {
                            has_account_id = true;
                        }
                    }
                }
            }

            // Build SQL based on detected columns
            let sql = if has_source_name {
                "SELECT remote_path, source_name, title, COALESCE(duration_ms, 0) FROM tracks"
            } else if has_account_id {
                "SELECT remote_path, account_id, title, COALESCE(duration_ms, 0) FROM tracks"
            } else {
                // No usable source column; return empty map
                return Ok(TableDiff {
                    left_count: 0,
                    right_count: 0,
                    only_left: vec![],
                    only_right: vec![],
                    field_mismatches: vec![],
                });
            };

            let mut stmt = old.prepare(sql)
                .map_err(|e| StorageError::Database(format!("tracks prepare: {e}")))?;

            let rows = stmt
                .query_map([], |row| {
                    let remote_path: String = row.get(0).unwrap_or_default();
                    let src: String = row.get(1).unwrap_or_default();
                    let title: String = row.get(2).unwrap_or_default();
                    let duration_ms: i64 = row.get(3).unwrap_or(0);
                    let key = format!("{}|{}", src, remote_path);
                    Ok((key, (src, title, duration_ms)))
                })
                .map_err(|e| StorageError::Database(format!("tracks query: {e}")))?;

            let mut map = HashMap::new();
            for r in rows {
                if let Ok((k, v)) = r {
                    map.insert(k, v);
                }
            }
            map
        }
    };

    // Read Core tracks
    let right_map: HashMap<String, (String, String, i64)> = {
        let conn = dest.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT remote_path, account_id, title, COALESCE(duration_ms, 0) FROM tracks"
        )?;
        let rows = stmt.query_map([], |row| {
            let remote_path: String = row.get(0)?;
            let account_id: String = row.get(1)?;
            let title: String = row.get(2)?;
            let duration_ms: i64 = row.get(3)?;
            let key = format!("{}|{}", account_id, remote_path);
            Ok((key, (account_id, title, duration_ms)))
        })?;
        let mut map = HashMap::new();
        for r in rows {
            if let Ok((k, v)) = r {
                map.insert(k, v);
            }
        }
        map
    };

    let left_keys: HashSet<_> = left_map.keys().cloned().collect();
    let right_keys: HashSet<_> = right_map.keys().cloned().collect();

    let only_left: Vec<String> = left_keys.difference(&right_keys).cloned().collect();
    let only_right: Vec<String> = right_keys.difference(&left_keys).cloned().collect();

    let mut field_mismatches = Vec::new();
    for key in left_keys.intersection(&right_keys) {
        let (_, l_title, l_dur) = &left_map[key];
        let (_, r_title, r_dur) = &right_map[key];
        if l_title != r_title {
            field_mismatches.push(format!("{}: title {:?}!={:?}", key, l_title, r_title));
        }
        if l_dur != r_dur {
            field_mismatches.push(format!("{}: duration_ms {}!={}", key, l_dur, r_dur));
        }
    }

    Ok(TableDiff {
        left_count: left_map.len(),
        right_count: right_map.len(),
        only_left,
        only_right,
        field_mismatches,
    })
}

fn compare_deleted_tracks(
    old: &Connection,
    dest: &LibraryDatabase,
) -> StorageResult<TableDiff> {
    use std::collections::{HashMap, HashSet};

    // Read sqflite deleted_tracks
    let left_map: HashMap<String, (i64, i64)> = {
        let exists: bool = old
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='deleted_tracks'",
                [],
                |row| row.get::<_, i64>(0).map(|c| c > 0),
            )
            .unwrap_or(false);

        if !exists {
            HashMap::new()
        } else {
            let mut stmt = old
                .prepare(
                    "SELECT source_name, remote_path, rev, pushed FROM deleted_tracks"
                )
                .map_err(|e| StorageError::Database(format!("deleted_tracks prepare: {e}")))?;

            let rows = stmt
                .query_map([], |row| {
                    let source_name: String = row.get(0).unwrap_or_default();
                    let remote_path: String = row.get(1).unwrap_or_default();
                    let rev: i64 = row.get(2).unwrap_or(0);
                    let pushed: i64 = row.get(3).unwrap_or(0);
                    let key = format!("{}|{}", source_name, remote_path);
                    Ok((key, (rev, pushed)))
                })
                .map_err(|e| StorageError::Database(format!("deleted_tracks query: {e}")))?;

            let mut map = HashMap::new();
            for r in rows {
                if let Ok((k, v)) = r {
                    map.insert(k, v);
                }
            }
            map
        }
    };

    // Read Core deleted_tracks
    let right_map: HashMap<String, (i64, i64)> = {
        let tracks = dest.list_deleted_tracks()?;
        let mut map = HashMap::new();
        for t in tracks {
            let key = format!("{}|{}", t.source_name, t.remote_path);
            map.insert(key, (t.rev, t.pushed));
        }
        map
    };

    let left_keys: HashSet<_> = left_map.keys().cloned().collect();
    let right_keys: HashSet<_> = right_map.keys().cloned().collect();

    let only_left: Vec<String> = left_keys.difference(&right_keys).cloned().collect();
    let only_right: Vec<String> = right_keys.difference(&left_keys).cloned().collect();

    let mut field_mismatches = Vec::new();
    for key in left_keys.intersection(&right_keys) {
        let (l_rev, l_pushed) = left_map[key];
        let (r_rev, r_pushed) = right_map[key];
        if l_rev != r_rev {
            field_mismatches.push(format!("{}: rev {}!={}", key, l_rev, r_rev));
        }
        if l_pushed != r_pushed {
            field_mismatches.push(format!("{}: pushed {}!={}", key, l_pushed, r_pushed));
        }
    }

    Ok(TableDiff {
        left_count: left_map.len(),
        right_count: right_map.len(),
        only_left,
        only_right,
        field_mismatches,
    })
}

fn compare_sync_state(
    old: &Connection,
    dest: &LibraryDatabase,
) -> StorageResult<TableDiff> {
    use std::collections::{HashMap, HashSet};

    // Read sqflite sync_state
    let left_map: HashMap<String, (i64, i64, String)> = {
        let exists: bool = old
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='sync_state'",
                [],
                |row| row.get::<_, i64>(0).map(|c| c > 0),
            )
            .unwrap_or(false);

        if !exists {
            HashMap::new()
        } else {
            let mut stmt = old
                .prepare(
                    "SELECT remote_key, last_seq, base_up_to, parts FROM sync_state"
                )
                .map_err(|e| StorageError::Database(format!("sync_state prepare: {e}")))?;

            let rows = stmt
                .query_map([], |row| {
                    let remote_key: String = row.get(0).unwrap_or_default();
                    let last_seq: i64 = row.get(1).unwrap_or(0);
                    let base_up_to: i64 = row.get(2).unwrap_or(0);
                    let parts: String = row.get(3).unwrap_or_else(|_| "{}".to_string());
                    Ok((remote_key.clone(), (last_seq, base_up_to, parts)))
                })
                .map_err(|e| StorageError::Database(format!("sync_state query: {e}")))?;

            let mut map = HashMap::new();
            for r in rows {
                if let Ok((k, v)) = r {
                    map.insert(k, v);
                }
            }
            map
        }
    };

    // Read Core sync_state
    let right_map: HashMap<String, (i64, i64, String)> = {
        let conn = dest.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT remote_key, last_seq, base_up_to, parts FROM sync_state"
        )?;
        let rows = stmt.query_map([], |row| {
            let remote_key: String = row.get(0)?;
            let last_seq: i64 = row.get(1)?;
            let base_up_to: i64 = row.get(2)?;
            let parts: String = row.get(3)?;
            Ok((remote_key, (last_seq, base_up_to, parts)))
        })?;
        let mut map = HashMap::new();
        for r in rows {
            if let Ok((k, v)) = r {
                map.insert(k, v);
            }
        }
        map
    };

    let left_keys: HashSet<_> = left_map.keys().cloned().collect();
    let right_keys: HashSet<_> = right_map.keys().cloned().collect();

    let only_left: Vec<String> = left_keys.difference(&right_keys).cloned().collect();
    let only_right: Vec<String> = right_keys.difference(&left_keys).cloned().collect();

    let mut field_mismatches = Vec::new();
    for key in left_keys.intersection(&right_keys) {
        let (l_seq, l_up_to, l_parts) = &left_map[key];
        let (r_seq, r_up_to, r_parts) = &right_map[key];
        if l_seq != r_seq {
            field_mismatches.push(format!("{}: last_seq {}!={}", key, l_seq, r_seq));
        }
        if l_up_to != r_up_to {
            field_mismatches.push(format!("{}: base_up_to {}!={}", key, l_up_to, r_up_to));
        }
        if l_parts != r_parts {
            field_mismatches.push(format!("{}: parts {:?}!={:?}", key, l_parts, r_parts));
        }
    }

    Ok(TableDiff {
        left_count: left_map.len(),
        right_count: right_map.len(),
        only_left,
        only_right,
        field_mismatches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_v4() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(db.schema_version().unwrap(), 6);
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

    // ── v5 tests: CUE + clear + delete-by-account ────────────────────────────

    #[test]
    fn cue_crud_and_slice_remaining() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();

        let album = CueAlbum {
            cue_id: "cue-001".into(),
            source_name: "nas".into(),
            cue_remote_path: "/music/album.cue".into(),
            title: Some("Album Title".into()),
            performer: Some("Artist".into()),
            cache_group_id: None,
            created_at: now.clone(),
        };
        db.upsert_cue_album(&album).unwrap();

        let slice1 = CueSlice {
            music_id: "slice-001".into(),
            cue_id: "cue-001".into(),
            audio_music_id: "audio-001".into(),
            source_name: "nas".into(),
            remote_path: "/music/track1.mp3".into(),
            file_name: "track1.mp3".into(),
            track_index: 1,
            title: Some("Track 1".into()),
            artist: Some("Artist".into()),
            album_artist: None,
            album: Some("Album Title".into()),
            duration_ms: Some(180_000),
            track_number: Some(1),
            track_total: Some(3),
            disc_number: Some(1),
            disc_total: Some(1),
            year: Some(2024),
            genre: Some("Rock".into()),
            bitrate: Some(320),
            sample_rate: Some(44100),
            cover_path: None,
            audio_remote_path: Some("/music/album.flac".into()),
            clip_start_ms: Some(0),
            clip_end_ms: Some(180_000),
            cache_group_id: None,
            rev: 0,
            last_downloaded_at: now.clone(),
            last_tag_read_at: now.clone(),
            remote_missing: 0,
        };
        db.upsert_cue_slice(&slice1).unwrap();

        let slice2 = CueSlice {
            music_id: "slice-002".into(),
            remote_path: "/music/track2.mp3".into(),
            file_name: "track2.mp3".into(),
            track_index: 2,
            title: Some("Track 2".into()),
            ..slice1.clone()
        };
        db.upsert_cue_slice(&slice2).unwrap();

        // Remaining slices
        assert_eq!(
            db.remaining_slices_for_cue("nas", "/music/album.cue").unwrap(),
            2
        );

        // Delete one slice
        assert_eq!(db.delete_cue_slice("slice-001").unwrap(), 1);
        assert_eq!(
            db.remaining_slices_for_cue("nas", "/music/album.cue").unwrap(),
            1
        );

        // Delete album (cascades to remaining slice)
        db.delete_cue_album("nas", "/music/album.cue").unwrap();
        assert_eq!(
            db.remaining_slices_for_cue("nas", "/music/album.cue").unwrap(),
            0
        );
    }

    #[test]
    fn cue_json_roundtrip_schema() {
        /// Documents the expected CueAlbum/CueSlice JSON field set and validates
        /// roundtrip: upsert → remaining_slices → delete.
        /// This test serves as a parity harness for Android dual-write — the
        /// JSON shapes built by Android helpers must match this schema.
        use serde_json::json;

        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now();

        // CueAlbum JSON shape (snake_case)
        let album_json = json!({
            "cue_id": "cue-test",
            "source_name": "test-source",
            "cue_remote_path": "/path/album.cue",
            "title": "Test Album",
            "performer": "Test Artist",
            "cache_group_id": "cache-grp-001",
            "created_at": now.to_rfc3339(),
        });

        let album = serde_json::from_value::<CueAlbum>(album_json.clone()).unwrap();
        db.upsert_cue_album(&album).unwrap();

        // CueSlice JSON shape (snake_case, all fields)
        let slice_json = json!({
            "music_id": "slice-test-001",
            "cue_id": "cue-test",
            "audio_music_id": "audio-test-001",
            "source_name": "test-source",
            "remote_path": "/path/track01.mp3",
            "file_name": "track01.mp3",
            "track_index": 1,
            "title": "Track 1",
            "artist": "Track Artist",
            "album_artist": "Album Artist",
            "album": "Test Album",
            "duration_ms": 240_000,
            "track_number": 1,
            "track_total": 10,
            "disc_number": 1,
            "disc_total": 1,
            "year": 2024,
            "genre": "Rock",
            "bitrate": 320,
            "sample_rate": 44100,
            "cover_path": "/path/thumb.jpg",
            "audio_remote_path": "/path/album.flac",
            "clip_start_ms": 0,
            "clip_end_ms": 240_000,
            "cache_group_id": "cache-grp-001",
            "rev": 12345,
            "last_downloaded_at": now.to_rfc3339(),
            "last_tag_read_at": now.to_rfc3339(),
            "remote_missing": 0,
        });

        let slice = serde_json::from_value::<CueSlice>(slice_json.clone()).unwrap();
        db.upsert_cue_slice(&slice).unwrap();

        // Verify fields are present and correct types
        assert_eq!(slice.music_id, "slice-test-001");
        assert_eq!(slice.track_index, 1);
        assert_eq!(slice.audio_music_id, "audio-test-001");
        assert_eq!(slice.title, Some("Track 1".into()));

        // Verify remaining_slices works
        assert_eq!(
            db.remaining_slices_for_cue("test-source", "/path/album.cue").unwrap(),
            1
        );

        // Delete and verify
        db.delete_cue_slice("slice-test-001").unwrap();
        assert_eq!(
            db.remaining_slices_for_cue("test-source", "/path/album.cue").unwrap(),
            0
        );
        db.delete_cue_album("test-source", "/path/album.cue").unwrap();
    }

    #[test]
    fn delete_tracks_by_account() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();

        for (i, acc) in ["acc1", "acc2", "acc1"].iter().enumerate() {
            db.upsert_track(&LibraryTrack {
                id: format!("t{}", i),
                title: format!("Track {}", i),
                artist: None,
                remote_path: format!("/m/{}.mp3", i),
                account_id: acc.to_string(),
                size: 1000,
                modified: None,
                ..Default::default()
            })
            .unwrap();
        }

        assert_eq!(db.list_tracks().unwrap().len(), 3);

        let deleted = db.delete_tracks_by_account("acc1").unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(db.list_tracks().unwrap().len(), 1);
        assert_eq!(db.list_tracks().unwrap()[0].account_id, "acc2");
    }

    #[test]
    fn clear_library_all_keeps_accounts() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now();

        // Insert data across multiple tables
        db.insert_account(&CloudAccount {
            id: "acc1".into(),
            provider_type: "webdav".into(),
            display_name: "Test".into(),
            base_url: Some("http://nas".into()),
            credentials_encrypted: vec![1, 2, 3],
        })
        .unwrap();

        db.upsert_track(&LibraryTrack {
            id: "t1".into(),
            title: "Song".into(),
            artist: None,
            remote_path: "/m/s.mp3".into(),
            account_id: "acc1".into(),
            size: 1000,
            modified: None,
            ..Default::default()
        })
        .unwrap();

        db.upsert_playlist(&Playlist {
            id: "pl1".into(),
            name: "Playlist".into(),
            created_at: now,
            updated_at: now,
        })
        .unwrap();

        db.save_kv("some_key", "some_value").unwrap();

        // Clear library
        db.clear_library_all().unwrap();

        // Accounts and KV survive
        assert!(db.get_account("acc1").unwrap().is_some());
        assert_eq!(db.load_kv("some_key").unwrap(), Some("some_value".into()));

        // Library data gone
        assert!(db.list_tracks().unwrap().is_empty());
        assert!(db.list_playlists().unwrap().is_empty());
    }

    #[test]
    fn clear_library_index_keeps_playlists() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now();

        db.upsert_track(&LibraryTrack {
            id: "t1".into(),
            title: "Song".into(),
            artist: None,
            remote_path: "/m/s.mp3".into(),
            account_id: "acc1".into(),
            size: 1000,
            modified: None,
            ..Default::default()
        })
        .unwrap();

        db.upsert_playlist(&Playlist {
            id: "pl1".into(),
            name: "Playlist".into(),
            created_at: now,
            updated_at: now,
        })
        .unwrap();

        db.clear_library_index().unwrap();

        // Tracks cleared
        assert!(db.list_tracks().unwrap().is_empty());

        // Playlists survive
        assert_eq!(db.list_playlists().unwrap().len(), 1);
    }

    #[test]
    fn schema_v6() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(db.schema_version().unwrap(), 6);

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
        assert!(exists("deleted_tracks"));
        assert!(exists("sync_state"));
    }

    #[test]
    fn deleted_tracks_crud() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();

        let t1 = DeletedTrack {
            source_name: "source1".into(),
            remote_path: "/music/a.flac".into(),
            rev: 1,
            deleted_at: now.clone(),
            pushed: 0,
        };

        db.upsert_deleted_track(&t1).unwrap();
        let all = db.list_deleted_tracks().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].source_name, "source1");
        assert_eq!(all[0].rev, 1);

        let unpushed = db.list_unpushed_deleted_tracks().unwrap();
        assert_eq!(unpushed.len(), 1);

        db.mark_deleted_tracks_pushed(&[1]).unwrap();
        let unpushed = db.list_unpushed_deleted_tracks().unwrap();
        assert_eq!(unpushed.len(), 0);

        let purged = db.purge_deleted_tracks_up_to(1).unwrap();
        assert_eq!(purged, 1);
        assert_eq!(db.list_deleted_tracks().unwrap().len(), 0);
    }

    #[test]
    fn deleted_tracks_clear() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();

        let t1 = DeletedTrack {
            source_name: "source1".into(),
            remote_path: "/music/a.flac".into(),
            rev: 1,
            deleted_at: now.clone(),
            pushed: 0,
        };
        db.upsert_deleted_track(&t1).unwrap();
        assert_eq!(db.list_deleted_tracks().unwrap().len(), 1);

        db.clear_deleted_track("source1", "/music/a.flac").unwrap();
        assert_eq!(db.list_deleted_tracks().unwrap().len(), 0);
    }

    #[test]
    fn deleted_tracks_tombstoned_paths() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();

        db.upsert_deleted_track(&DeletedTrack {
            source_name: "src1".into(),
            remote_path: "/a.mp3".into(),
            rev: 1,
            deleted_at: now.clone(),
            pushed: 0,
        })
        .unwrap();

        db.upsert_deleted_track(&DeletedTrack {
            source_name: "src1".into(),
            remote_path: "/b.mp3".into(),
            rev: 2,
            deleted_at: now.clone(),
            pushed: 0,
        })
        .unwrap();

        let paths = db.tombstoned_remote_paths("src1").unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"/a.mp3".to_string()));
        assert!(paths.contains(&"/b.mp3".to_string()));
    }

    #[test]
    fn clear_library_index_clears_deleted_tracks() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();

        db.upsert_deleted_track(&DeletedTrack {
            source_name: "src1".into(),
            remote_path: "/music/x.flac".into(),
            rev: 1,
            deleted_at: now.clone(),
            pushed: 0,
        })
        .unwrap();
        assert_eq!(db.list_deleted_tracks().unwrap().len(), 1);

        db.clear_library_index().unwrap();
        assert_eq!(db.list_deleted_tracks().unwrap().len(), 0);
    }

    #[test]
    fn sync_state_crud() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();

        let state = SyncState {
            remote_key: "library:source1".into(),
            last_seq: 10,
            base_up_to: 5,
            parts: r#"{"a":1}"#.into(),
            updated_at: now.clone(),
        };

        db.save_sync_state(&state).unwrap();
        let loaded = db.get_sync_state("library:source1").unwrap();
        assert!(loaded.is_some());
        let s = loaded.unwrap();
        assert_eq!(s.last_seq, 10);
        assert_eq!(s.base_up_to, 5);
        assert_eq!(s.parts, r#"{"a":1}"#);

        let missing = db.get_sync_state("missing").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn sync_state_clear() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();

        db.save_sync_state(&SyncState {
            remote_key: "library:s1".into(),
            last_seq: 1,
            base_up_to: 0,
            parts: "{}".into(),
            updated_at: now.clone(),
        })
        .unwrap();

        db.save_sync_state(&SyncState {
            remote_key: "manga:s2".into(),
            last_seq: 2,
            base_up_to: 0,
            parts: "{}".into(),
            updated_at: now.clone(),
        })
        .unwrap();

        // Clear by prefix
        db.clear_sync_state(Some("library:")).unwrap();
        assert!(db.get_sync_state("library:s1").unwrap().is_none());
        assert!(db.get_sync_state("manga:s2").unwrap().is_some());

        // Clear all
        db.clear_sync_state(None).unwrap();
        assert!(db.get_sync_state("manga:s2").unwrap().is_none());
    }

    #[test]
    fn deleted_tracks_json_roundtrip() {
        let now = Utc::now().to_rfc3339();

        let track = DeletedTrack {
            source_name: "src".into(),
            remote_path: "/path.mp3".into(),
            rev: 42,
            deleted_at: now.clone(),
            pushed: 1,
        };

        let json = serde_json::to_string(&track).unwrap();
        let parsed: DeletedTrack = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_name, "src");
        assert_eq!(parsed.rev, 42);
        assert_eq!(parsed.pushed, 1);
    }

    #[test]
    fn sync_state_json_roundtrip() {
        let now = Utc::now().to_rfc3339();

        let state = SyncState {
            remote_key: "library:x".into(),
            last_seq: 100,
            base_up_to: 50,
            parts: r#"{"foo":"bar"}"#.into(),
            updated_at: now.clone(),
        };

        let json = serde_json::to_string(&state).unwrap();
        let parsed: SyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.remote_key, "library:x");
        assert_eq!(parsed.last_seq, 100);
        assert_eq!(parsed.base_up_to, 50);
        assert_eq!(parsed.parts, r#"{"foo":"bar"}"#);
    }

    #[test]
    fn compare_with_sqflite_match() {
        use std::fs;

        // Create temp files
        let temp_dir = std::env::temp_dir();
        let left_path = temp_dir.join(format!("test_left_{}.db", uuid::Uuid::new_v4()));
        let right_path = temp_dir.join(format!("test_right_{}.db", uuid::Uuid::new_v4()));

        // Create left DB with Android-ish schema (source_name)
        let left_conn = Connection::open(&left_path).unwrap();
        left_conn
            .execute_batch(
                r#"
                CREATE TABLE tracks (
                    music_id TEXT PRIMARY KEY,
                    source_name TEXT NOT NULL,
                    remote_path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    duration_ms INTEGER
                );
                CREATE TABLE deleted_tracks (
                    source_name TEXT NOT NULL,
                    remote_path TEXT NOT NULL,
                    rev INTEGER NOT NULL,
                    deleted_at TEXT NOT NULL,
                    pushed INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (source_name, remote_path)
                );
                CREATE TABLE sync_state (
                    remote_key TEXT PRIMARY KEY,
                    last_seq INTEGER NOT NULL DEFAULT 0,
                    base_up_to INTEGER NOT NULL DEFAULT 0,
                    parts TEXT NOT NULL DEFAULT '{}',
                    updated_at TEXT NOT NULL
                );
                "#,
            )
            .unwrap();

        // Insert sample data
        left_conn
            .execute(
                "INSERT INTO tracks (music_id, source_name, remote_path, title, duration_ms) \
                 VALUES ('id1', 'wd-main', '/music/track1.flac', 'Track One', 180000)",
                [],
            )
            .unwrap();
        left_conn
            .execute(
                "INSERT INTO deleted_tracks (source_name, remote_path, rev, deleted_at, pushed) \
                 VALUES ('wd-main', '/music/deleted.mp3', 100, '2024-01-02T03:04:05.000Z', 0)",
                [],
            )
            .unwrap();
        left_conn
            .execute(
                "INSERT INTO sync_state (remote_key, last_seq, base_up_to, parts, updated_at) \
                 VALUES ('library:x', 42, 10, '{\"a\":1}', '2024-01-01T00:00:00.000Z')",
                [],
            )
            .unwrap();
        drop(left_conn);

        // Create Core LibraryDatabase on right path
        let right_db = LibraryDatabase::open(&right_path).unwrap();

        // Upsert matching track (account_id = source_name)
        right_db
            .upsert_track(&LibraryTrack {
                id: "id1".to_string(),
                title: "Track One".to_string(),
                artist: None,
                remote_path: "/music/track1.flac".to_string(),
                account_id: "wd-main".to_string(),
                size: 0,
                modified: None,
                album_id: None,
                artist_id: None,
                duration_ms: 180000,
                genre: None,
                track_number: None,
                disc_number: None,
                year: None,
                mime_type: None,
            })
            .unwrap();

        // Upsert matching DeletedTrack
        right_db
            .upsert_deleted_track(&DeletedTrack {
                source_name: "wd-main".to_string(),
                remote_path: "/music/deleted.mp3".to_string(),
                rev: 100,
                deleted_at: "2024-01-02T03:04:05.000Z".to_string(),
                pushed: 0,
            })
            .unwrap();

        // Upsert matching SyncState
        right_db
            .save_sync_state(&SyncState {
                remote_key: "library:x".to_string(),
                last_seq: 42,
                base_up_to: 10,
                parts: "{\"a\":1}".to_string(),
                updated_at: "2024-01-01T00:00:00.000Z".to_string(),
            })
            .unwrap();

        // Compare
        let report = LibraryDatabase::compare_with_sqflite(&left_path, &right_db).unwrap();

        assert!(report.ok, "report should be ok: {:?}", report);
        assert_eq!(report.tracks.left_count, 1);
        assert_eq!(report.tracks.right_count, 1);
        assert!(report.tracks.only_left.is_empty());
        assert!(report.tracks.only_right.is_empty());
        assert!(report.tracks.field_mismatches.is_empty());

        assert_eq!(report.deleted_tracks.left_count, 1);
        assert_eq!(report.deleted_tracks.right_count, 1);
        assert!(report.deleted_tracks.only_left.is_empty());
        assert!(report.deleted_tracks.only_right.is_empty());
        assert!(report.deleted_tracks.field_mismatches.is_empty());

        assert_eq!(report.sync_state.left_count, 1);
        assert_eq!(report.sync_state.right_count, 1);
        assert!(report.sync_state.only_left.is_empty());
        assert!(report.sync_state.only_right.is_empty());
        assert!(report.sync_state.field_mismatches.is_empty());

        // Clean up
        drop(right_db);
        let _ = fs::remove_file(&left_path);
        let _ = fs::remove_file(&right_path);
    }

    #[test]
    fn compare_with_sqflite_mismatch() {
        use std::fs;

        let temp_dir = std::env::temp_dir();
        let left_path = temp_dir.join(format!("test_left_{}.db", uuid::Uuid::new_v4()));
        let right_path = temp_dir.join(format!("test_right_{}.db", uuid::Uuid::new_v4()));

        // Create left DB
        let left_conn = Connection::open(&left_path).unwrap();
        left_conn
            .execute_batch(
                r#"
                CREATE TABLE tracks (
                    music_id TEXT PRIMARY KEY,
                    source_name TEXT NOT NULL,
                    remote_path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    duration_ms INTEGER
                );
                CREATE TABLE deleted_tracks (
                    source_name TEXT NOT NULL,
                    remote_path TEXT NOT NULL,
                    rev INTEGER NOT NULL,
                    deleted_at TEXT NOT NULL,
                    pushed INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (source_name, remote_path)
                );
                CREATE TABLE sync_state (
                    remote_key TEXT PRIMARY KEY,
                    last_seq INTEGER NOT NULL DEFAULT 0,
                    base_up_to INTEGER NOT NULL DEFAULT 0,
                    parts TEXT NOT NULL DEFAULT '{}',
                    updated_at TEXT NOT NULL
                );
                "#,
            )
            .unwrap();

        // Insert track + extra tombstone
        left_conn
            .execute(
                "INSERT INTO tracks (music_id, source_name, remote_path, title, duration_ms) \
                 VALUES ('id1', 'wd-main', '/music/track1.flac', 'Track One', 180000)",
                [],
            )
            .unwrap();
        left_conn
            .execute(
                "INSERT INTO deleted_tracks (source_name, remote_path, rev, deleted_at, pushed) \
                 VALUES ('wd-main', '/music/deleted.mp3', 100, '2024-01-02T03:04:05.000Z', 0)",
                [],
            )
            .unwrap();
        left_conn
            .execute(
                "INSERT INTO deleted_tracks (source_name, remote_path, rev, deleted_at, pushed) \
                 VALUES ('wd-main', '/music/extra.mp3', 200, '2024-01-03T00:00:00.000Z', 1)",
                [],
            )
            .unwrap();
        left_conn
            .execute(
                "INSERT INTO sync_state (remote_key, last_seq, base_up_to, parts, updated_at) \
                 VALUES ('library:x', 42, 10, '{\"a\":1}', '2024-01-01T00:00:00.000Z')",
                [],
            )
            .unwrap();
        drop(left_conn);

        // Create Core DB with matching track but not the extra tombstone
        let right_db = LibraryDatabase::open(&right_path).unwrap();
        right_db
            .upsert_track(&LibraryTrack {
                id: "id1".to_string(),
                title: "Track One".to_string(),
                artist: None,
                remote_path: "/music/track1.flac".to_string(),
                account_id: "wd-main".to_string(),
                size: 0,
                modified: None,
                album_id: None,
                artist_id: None,
                duration_ms: 180000,
                genre: None,
                track_number: None,
                disc_number: None,
                year: None,
                mime_type: None,
            })
            .unwrap();
        right_db
            .upsert_deleted_track(&DeletedTrack {
                source_name: "wd-main".to_string(),
                remote_path: "/music/deleted.mp3".to_string(),
                rev: 100,
                deleted_at: "2024-01-02T03:04:05.000Z".to_string(),
                pushed: 0,
            })
            .unwrap();
        right_db
            .save_sync_state(&SyncState {
                remote_key: "library:x".to_string(),
                last_seq: 42,
                base_up_to: 10,
                parts: "{\"a\":1}".to_string(),
                updated_at: "2024-01-01T00:00:00.000Z".to_string(),
            })
            .unwrap();

        // Compare
        let report = LibraryDatabase::compare_with_sqflite(&left_path, &right_db).unwrap();

        assert!(!report.ok, "report should be not ok due to extra tombstone");
        assert_eq!(report.deleted_tracks.left_count, 2);
        assert_eq!(report.deleted_tracks.right_count, 1);
        assert_eq!(report.deleted_tracks.only_left.len(), 1);
        assert!(report.deleted_tracks.only_left[0].contains("extra.mp3"));

        // Clean up
        drop(right_db);
        let _ = fs::remove_file(&left_path);
        let _ = fs::remove_file(&right_path);
    }

    #[test]
    fn compare_with_sqflite_missing_table() {
        use std::fs;

        let temp_dir = std::env::temp_dir();
        let left_path = temp_dir.join(format!("test_left_{}.db", uuid::Uuid::new_v4()));
        let right_path = temp_dir.join(format!("test_right_{}.db", uuid::Uuid::new_v4()));

        // Create left DB WITHOUT deleted_tracks table
        let left_conn = Connection::open(&left_path).unwrap();
        left_conn
            .execute_batch(
                r#"
                CREATE TABLE tracks (
                    music_id TEXT PRIMARY KEY,
                    source_name TEXT NOT NULL,
                    remote_path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    duration_ms INTEGER
                );
                CREATE TABLE sync_state (
                    remote_key TEXT PRIMARY KEY,
                    last_seq INTEGER NOT NULL DEFAULT 0,
                    base_up_to INTEGER NOT NULL DEFAULT 0,
                    parts TEXT NOT NULL DEFAULT '{}',
                    updated_at TEXT NOT NULL
                );
                "#,
            )
            .unwrap();
        drop(left_conn);

        // Create Core DB
        let right_db = LibraryDatabase::open(&right_path).unwrap();

        // Compare
        let report = LibraryDatabase::compare_with_sqflite(&left_path, &right_db).unwrap();

        // Should succeed with left_count 0 for deleted_tracks, not an error
        assert_eq!(report.deleted_tracks.left_count, 0);
        assert_eq!(report.deleted_tracks.right_count, 0);

        // Clean up
        drop(right_db);
        let _ = fs::remove_file(&left_path);
        let _ = fs::remove_file(&right_path);
    }
}
