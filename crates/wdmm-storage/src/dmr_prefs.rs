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
            "#,
        )?;
        Ok(())
    }

    pub fn list(&self) -> StorageResult<Vec<DmrPrefRow>> {
        let conn = self.conn.lock().map_err(|e| StorageError::Database(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT usn, force_direct, override_name, updated_at FROM dmr_prefs ORDER BY usn",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DmrPrefRow {
                usn: r.get(0)?,
                force_direct: r.get::<_, i64>(1)? != 0,
                override_name: r.get::<_, Option<String>>(2)?,
                updated_at: r.get(3)?,
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
            "SELECT usn, force_direct, override_name, updated_at FROM dmr_prefs WHERE usn = ?1",
        )?;
        let mut rows = stmt.query(params![usn])?;
        match rows.next()? {
            Some(r) => Ok(Some(DmrPrefRow {
                usn: r.get(0)?,
                force_direct: r.get::<_, i64>(1)? != 0,
                override_name: r.get::<_, Option<String>>(2)?,
                updated_at: r.get(3)?,
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
        conn.execute(
            "INSERT INTO dmr_prefs (usn, force_direct, override_name, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(usn) DO UPDATE SET
               force_direct = excluded.force_direct,
               override_name = excluded.override_name,
               updated_at = excluded.updated_at",
            params![
                pref.usn,
                if pref.force_direct { 1i64 } else { 0i64 },
                oname,
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
            let updated = if pref.updated_at.is_empty() {
                Utc::now().to_rfc3339()
            } else {
                pref.updated_at.clone()
            };
            tx.execute(
                "INSERT INTO dmr_prefs (usn, force_direct, override_name, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    pref.usn,
                    if pref.force_direct { 1i64 } else { 0i64 },
                    oname,
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
            updated_at: String::new(),
        })
        .unwrap();
        let got = db.get("uuid:tv-1").unwrap().unwrap();
        assert!(!got.force_direct);
        assert_eq!(got.override_name.as_deref(), Some("Living Room"));
        db.delete("uuid:tv-1").unwrap();
        assert!(db.get("uuid:tv-1").unwrap().is_none());
    }
}
