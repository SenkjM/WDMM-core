//! `wdmm-storage` — rusqlite library DB (schema v1, minimal tables skeleton).
//!
//! Aligns with WDMM `music_library.db` direction (schema v18 from architecture
//! report). This is a **minimal bootstrap** — full table set per brief: tracks,
//! cue, manga, user_folders, deleted. Schema version comments document alignment.

#![forbid(unsafe_code)]

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use thiserror::Error as ThisError;

use wdmm_core::models::LibraryTrack;

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
    conn: Connection,
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
            "#,
        )?;

        Ok(Self { conn })
    }

    pub fn insert_track(&self, track: &LibraryTrack) -> StorageResult<()> {
        let now = Utc::now().timestamp();
        let modified_ts = track.modified.map(|dt| dt.timestamp());
        self.conn.execute(
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
        let mut stmt = self.conn.prepare(
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
        Ok(self.conn.query_row("SELECT version FROM _schema_version", [], |row| row.get(0))?)
    }
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
}
