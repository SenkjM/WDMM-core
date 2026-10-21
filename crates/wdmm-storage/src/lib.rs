//! `wdmm-storage` — rusqlite library DB (schema v1, minimal tables skeleton).
//!
//! Aligns with WDMM `music_library.db` direction (schema v18 from architecture
//! report). This is a **minimal bootstrap** — full table set per brief: tracks,
//! cue, manga, user_folders, deleted. Schema version comments document alignment.

#![forbid(unsafe_code)]

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::sync::Mutex;
use thiserror::Error as ThisError;

use wdmm_core::models::{CloudAccount, LibraryTrack};

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
    /// Open or create the library database. Schema v1 (skeleton).
    pub fn open(path: &std::path::Path) -> StorageResult<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;

            -- Schema version tracking (aligns WDMM library schema v18 direction)
            CREATE TABLE IF NOT EXISTS _schema_version (
                version INTEGER PRIMARY KEY
            );
            INSERT OR IGNORE INTO _schema_version (version) VALUES (1);

            -- Tracks table (minimal subset)
            CREATE TABLE IF NOT EXISTS tracks (
                id           TEXT PRIMARY KEY,  -- SHA-1 identity
                title        TEXT NOT NULL,
                artist       TEXT,
                remote_path  TEXT NOT NULL,
                account_id   TEXT NOT NULL,
                size         INTEGER NOT NULL DEFAULT 0,
                modified     INTEGER,           -- Unix timestamp (seconds)
                created_at   INTEGER NOT NULL   -- Unix timestamp
            );

            -- User folders (stub)
            CREATE TABLE IF NOT EXISTS user_folders (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                parent_id  TEXT,
                created    INTEGER NOT NULL
            );

            -- Deleted items tombstone (stub)
            CREATE TABLE IF NOT EXISTS deleted (
                id         TEXT PRIMARY KEY,
                deleted_at INTEGER NOT NULL
            );

            -- Queue tasks (stub)
            CREATE TABLE IF NOT EXISTS queue_tasks (
                id         TEXT PRIMARY KEY,
                kind       TEXT NOT NULL,
                status     TEXT NOT NULL,
                created    INTEGER NOT NULL
            );

            -- Cloud accounts
            CREATE TABLE IF NOT EXISTS accounts (
                id                    TEXT PRIMARY KEY,
                provider_type         TEXT NOT NULL,
                display_name          TEXT NOT NULL,
                base_url              TEXT,
                credentials_encrypted BLOB NOT NULL,
                created_at            INTEGER NOT NULL,
                updated_at            INTEGER NOT NULL
            );

            -- Playlists (Round 4: full CRUD per docs/10-PLAYLIST-FORMAT.md essentials)
            CREATE TABLE IF NOT EXISTS playlists (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );

            -- Playlist entries (references to tracks, per §4.2 ENTRIES semantics)
            CREATE TABLE IF NOT EXISTS playlist_entries (
                playlist_id  TEXT NOT NULL,
                position     INTEGER NOT NULL,
                music_id     TEXT NOT NULL,
                source_name  TEXT NOT NULL,
                remote_path  TEXT NOT NULL,
                title        TEXT NOT NULL,
                duration_ms  INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (playlist_id, position),
                FOREIGN KEY (playlist_id) REFERENCES playlists(id) ON DELETE CASCADE
            );

            -- Key-value store (Round 4: OpenAPI token persistence + misc settings)
            CREATE TABLE IF NOT EXISTS kv_store (
                key        TEXT PRIMARY KEY,
                value      TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            "#,
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert_track(&self, track: &LibraryTrack) -> StorageResult<()> {
        let now = Utc::now().timestamp();
        let modified_ts = track.modified.map(|dt| dt.timestamp());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tracks (id, title, artist, remote_path, account_id, size, modified, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &track.id,
                &track.title,
                &track.artist,
                &track.remote_path,
                &track.account_id,
                track.size as i64,
                modified_ts,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn get_track(&self, id: &str) -> StorageResult<Option<LibraryTrack>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, artist, remote_path, account_id, size, modified FROM tracks WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            let modified_ts: Option<i64> = row.get(6)?;
            let modified = modified_ts.map(|ts| DateTime::from_timestamp(ts, 0).unwrap().with_timezone(&Utc));

            Ok(Some(LibraryTrack {
                id: row.get(0)?,
                title: row.get(1)?,
                artist: row.get(2)?,
                remote_path: row.get(3)?,
                account_id: row.get(4)?,
                size: row.get::<_, i64>(5)? as u64,
                modified,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn schema_version(&self) -> StorageResult<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT version FROM _schema_version", [], |row| row.get(0))?)
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

    // ── Playlists ─────────────────────────────────────────────────────────────

    pub fn list_tracks(&self) -> StorageResult<Vec<LibraryTrack>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, artist, remote_path, account_id, size, modified FROM tracks",
        )?;

        let rows = stmt.query_map([], |row| {
            let modified_ts: Option<i64> = row.get(6)?;
            let modified = modified_ts.map(|ts| DateTime::from_timestamp(ts, 0).unwrap().with_timezone(&Utc));

            Ok(LibraryTrack {
                id: row.get(0)?,
                title: row.get(1)?,
                artist: row.get(2)?,
                remote_path: row.get(3)?,
                account_id: row.get(4)?,
                size: row.get::<_, i64>(5)? as u64,
                modified,
            })
        })?;

        let mut tracks = Vec::new();
        for track in rows {
            tracks.push(track?);
        }
        Ok(tracks)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_query() {
        let db = LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);

        let track = LibraryTrack {
            id: "test-id".into(),
            title: "Test Track".into(),
            artist: Some("Artist".into()),
            remote_path: "/music/test.mp3".into(),
            account_id: "acc1".into(),
            size: 12345,
            modified: None,
        };
        db.insert_track(&track).unwrap();

        let loaded = db.get_track("test-id").unwrap().unwrap();
        assert_eq!(loaded.title, "Test Track");
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
