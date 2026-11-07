# wdmm-sync — Public API

Incremental sync engine for credentials, library tracks, and playlists.
Uses a `WdmmVfs` as the remote store; merge strategy is last-write-wins
at the entity level. Payload format: JSON (binary `WDMMPL01` containers are
a future milestone).

---

## Error types

### `enum SyncError`
`Driver(String)`, `Storage(#[from] StorageError)`, `Serde(String)`,
`Conflict { local: u64, remote: u64 }`, `Other(String)`.

### `type SyncResult<T> = std::result::Result<T, SyncError>`

---

## Outcome types

### `struct SyncOutcome`
Fields: `entity_id: String`, `action: SyncAction`, `rev: RevClock`.

### `enum SyncAction`
`NoChange | Pushed | Pulled | Conflict`

---

## Sync engine trait

### `trait SyncEngine: Send + Sync`

### `async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>>`
Sync only changed entities.

### `async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>>`
Full sync (currently delegates to `sync_incremental`).

---

## Credential sync engine

### `struct CredentialSyncEngine`
Syncs `CloudAccount` records to/from `{remote_dir}/credentials.json`.

### `fn CredentialSyncEngine::new(db: Arc<LibraryDatabase>, driver: Arc<dyn WdmmVfs>, remote_dir: impl Into<String>) -> Self`
`remote_dir`: base directory on the remote (e.g. `"/wdmm_sync"`).

Implements `SyncEngine`. Strategy: full-replace push; accounts on remote not
found locally are pulled (inserted). `credentials_encrypted` is base64-encoded
in the wire format so it survives text round-trips.

---

## Library sync engine

### `struct LibrarySyncEngine`
Syncs `LibraryTrack` records via Android-compatible shard+tombstone under `{remote_dir}/library/music/` (`index.json`, `lib-`/`seg-`/`del-*.wdmm`).

### `fn LibrarySyncEngine::new(db: Arc<LibraryDatabase>, driver: Arc<dyn WdmmVfs>, remote_dir: impl Into<String>) -> Self`

Implements `SyncEngine`. Strategy: full-replace push; remote tracks not found
locally are pulled.

---

## Playlist sync engine

### `struct PlaylistSyncEngine`
Syncs `Playlist` + `PlaylistEntry` sets to/from `{remote_dir}/playlists/{id}.json`
(one `WDMMPL01` document per playlist) plus `WDMMPD01` deletion packs. Each id
is reconciled by revision: a tombstone newer than the live document wins, a peer
edit newer than the tombstone brings the playlist back, and tombstones survive
180 days before they are garbage collected.

### `fn PlaylistSyncEngine::new(db: Arc<LibraryDatabase>, driver: Arc<dyn WdmmVfs>, remote_dir: impl Into<String>) -> Self`

Implements `SyncEngine`. Strategy:
- Pull: remote playlists not present locally → insert.
- Remote `updated_at` strictly newer than local → pull (overwrite local).
- Equal timestamps → `NoChange`.
- Local newer or no remote → push.

---

## Aggregate trigger

### `pub enum SyncMode { Incremental, Full }`

`SyncMode::parse` / `as_str` use the wire values `"incremental"` / `"full"`.

### `async fn run_sync(db, driver, remote_dir, mode: SyncMode, cover_roots: Option<CoverRoots>) -> SyncResult<serde_json::Value>`

Run credential + library + manga + playlist sync and return the contract summary.
Entry point called by `wdmm_ffi::wdmm_sync_run`.

```json
{
  "ok": true,
  "mode": "incremental",
  "conflicts": N,
  "entities": {
    "credentials": {"total": N, "pushed": N, "pulled": N, "conflicts": N},
    "library":     {"total": N, "pushed": N, "pulled": N, "conflicts": N},
    "manga":       {"total": N, "pushed": N, "pulled": N, "conflicts": N},
    "playlists":   {"total": N, "pushed": N, "pulled": N, "conflicts": N}
  }
}
```

### `async fn run_sync_all(...)` / `async fn run_sync_full(...)`

Engine-level passes kept for tests and one-mode callers; `run_sync` dispatches to
them and wraps their summaries.
