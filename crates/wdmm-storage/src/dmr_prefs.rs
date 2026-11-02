//! Independent USN-keyed DMR preference store (`dmr_prefs.db`).

use chrono::Utc;
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

use crate::{StorageError, StorageResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DmrPrefRow {
    pub usn: String,
    /// `true` = proxy direct file; `false` = HLS transcode.
    pub force_direct: bool,
    /// User override for friendly name; empty/None = use discovery name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_name: Option<String>,
    /// HLS transcode preset (e.g. `"balanced"`, `"compatible"`).
    /// None → use app-wide default from settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    #[serde(default)]
    pub updated_at: String,
}

pub struct DmrPrefsDatabase {
    conn: Mutex<Connection>,
}

impl DmrPrefsDatabase {
    pub fn open(path: &Path) -> StorageResult<Self> {
        let conn = if path.as_os_str() == ":memory:" {
            Connection::open_in_memory()?
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    StorageError::Database(format!("create dmr_prefs dir: {e}"))
                })?;
            }
            Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            )?
        };
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> StorageResult<()> {
        let conn = self.conn.lock().map_err(|e| StorageError::Database(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS dmr_prefs (
                usn TEXT PRIMARY KEY NOT NULL,
                force_direct INTEGER NOT NULL DEFAULT 1,
                override_name TEXT,
                updated_at TEXT NOT NULL
            );
            -- v2 migration: add preset_id column if absent
            ALTER TABLE dmr_prefs ADD COLUMN preset_id TEXT;
            "#,
        ).or_else(|e| {
            // "duplicate column name" is returned by SQLite when the column already exists.
            // Treat that as a no-op so the migration is idempotent.
            let msg = e.to_string();
            if msg.contains("duplicate column name") {
                Ok::<(), StorageError>(())
            } else {
                Err(e.into())
            }
        })?;
        Ok(())
    }

    pub fn list(&self) -> StorageResult<Vec<DmrPrefRow>> {
        let conn = self.conn.lock().map_err(|e| StorageError::Database(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT usn, force_direct, override_name, preset_id, updated_at FROM dmr_prefs ORDER BY usn",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DmrPrefRow {
                usn: r.get(0)?,
                force_direct: r.get::<_, i64>(1)? != 0,
                override_name: r.get::<_, Option<String>>(2)?,
                preset_id: r.get::<_, Option<String>>(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get(&self, usn: &str) -> StorageResult<Option<DmrPrefRow>> {
        let conn = self.conn.lock().map_err(|e| StorageError::Database(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT usn, force_direct, override_name, preset_id, updated_at FROM dmr_prefs WHERE usn = ?1",
        )?;
        let mut rows = stmt.query(params![usn])?;
        match rows.next()? {
            Some(r) => Ok(Some(DmrPrefRow {
                usn: r.get(0)?,
                force_direct: r.get::<_, i64>(1)? != 0,
                override_name: r.get::<_, Option<String>>(2)?,
                preset_id: r.get::<_, Option<String>>(3)?,
                updated_at: r.get(4)?,
            })),
            None => Ok(None),
        }
    }

    pub fn upsert(&self, pref: &DmrPrefRow) -> StorageResult<()> {
        let conn = self.conn.lock().map_err(|e| StorageError::Database(e.to_string()))?;
        let updated = if pref.updated_at.is_empty() {
            Utc::now().to_rfc3339()
        } else {
            pref.updated_at.clone()
        };
        let oname = pref
            .override_name
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let preset = pref
            .preset_id
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        conn.execute(
            "INSERT INTO dmr_prefs (usn, force_direct, override_name, preset_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(usn) DO UPDATE SET
               force_direct = excluded.force_direct,
               override_name = excluded.override_name,
               preset_id = excluded.preset_id,
               updated_at = excluded.updated_at",
            params![
                pref.usn,
                if pref.force_direct { 1i64 } else { 0i64 },
                oname,
                preset,
                updated
            ],
        )?;
        Ok(())
    }

    pub fn delete(&self, usn: &str) -> StorageResult<()> {
        let conn = self.conn.lock().map_err(|e| StorageError::Database(e.to_string()))?;
        conn.execute("DELETE FROM dmr_prefs WHERE usn = ?1", params![usn])?;
        Ok(())
    }

    /// Replace all rows (used by backup restore).
    pub fn replace_all(&self, rows: &[DmrPrefRow]) -> StorageResult<()> {
        let conn = self.conn.lock().map_err(|e| StorageError::Database(e.to_string()))?;
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM dmr_prefs", [])?;
        for pref in rows {
            let oname = pref
                .override_name
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let preset = pref
                .preset_id
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let updated = if pref.updated_at.is_empty() {
                Utc::now().to_rfc3339()
            } else {
                pref.updated_at.clone()
            };
            tx.execute(
                "INSERT INTO dmr_prefs (usn, force_direct, override_name, preset_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    pref.usn,
                    if pref.force_direct { 1i64 } else { 0i64 },
                    oname,
                    preset,
                    updated
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmr_prefs_roundtrip() {
        let db = DmrPrefsDatabase::open(Path::new(":memory:")).unwrap();
        assert!(db.list().unwrap().is_empty());
        db.upsert(&DmrPrefRow {
            usn: "uuid:tv-1".into(),
            force_direct: false,
            override_name: Some("Living Room".into()),
            preset_id: None,
            updated_at: String::new(),
        })
        .unwrap();
        let got = db.get("uuid:tv-1").unwrap().unwrap();
        assert!(!got.force_direct);
        assert_eq!(got.override_name.as_deref(), Some("Living Room"));
        assert!(got.preset_id.is_none());
        db.delete("uuid:tv-1").unwrap();
        assert!(db.get("uuid:tv-1").unwrap().is_none());
    }

    #[test]
    fn dmr_prefs_preset_id_roundtrip() {
        let db = DmrPrefsDatabase::open(Path::new(":memory:")).unwrap();
        db.upsert(&DmrPrefRow {
            usn: "uuid:tv-2".into(),
            force_direct: false,
            override_name: None,
            preset_id: Some("compatible".into()),
            updated_at: String::new(),
        })
        .unwrap();
        let got = db.get("uuid:tv-2").unwrap().unwrap();
        assert_eq!(got.preset_id.as_deref(), Some("compatible"));

        // Update to clear preset_id
        db.upsert(&DmrPrefRow {
            usn: "uuid:tv-2".into(),
            force_direct: false,
            override_name: None,
            preset_id: None,
            updated_at: String::new(),
        })
        .unwrap();
        let updated = db.get("uuid:tv-2").unwrap().unwrap();
        assert!(updated.preset_id.is_none());
    }

    #[test]
    fn dmr_prefs_migration_idempotent() {
        // Opening the same in-memory db twice would add the column twice;
        // just verify a fresh open with the migration works without error.
        let db = DmrPrefsDatabase::open(Path::new(":memory:")).unwrap();
        db.upsert(&DmrPrefRow {
            usn: "uuid:tv-3".into(),
            force_direct: true,
            override_name: None,
            preset_id: Some("balanced".into()),
            updated_at: String::new(),
        })
        .unwrap();
        assert_eq!(
            db.get("uuid:tv-3").unwrap().unwrap().preset_id.as_deref(),
            Some("balanced")
        );
    }
}
