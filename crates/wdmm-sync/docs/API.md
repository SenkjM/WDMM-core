# wdmm-sync — Public API

Incremental sync engine for credentials, library tracks, and playlists.
Uses a `CloudDriver` as the remote store; merge strategy is last-write-wins
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

### `fn CredentialSyncEngine::new(db: Arc<LibraryDatabase>, driver: Arc<dyn CloudDriver>, remote_dir: impl Into<String>) -> Self`
`remote_dir`: base directory on the remote (e.g. `"/wdmm_sync"`).

Implements `SyncEngine`. Strategy: full-replace push; accounts on remote not
found locally are pulled (inserted). `credentials_encrypted` is base64-encoded
in the wire format so it survives text round-trips.

---

## Library sync engine

### `struct LibrarySyncEngine`
Syncs `LibraryTrack` records via Android-compatible shard+tombstone under `{remote_dir}/library/music/` (`index.json`, `lib-`/`seg-`/`del-*.wdmm`).

### `fn LibrarySyncEngine::new(db: Arc<LibraryDatabase>, driver: Arc<dyn CloudDriver>, remote_dir: impl Into<String>) -> Self`

Implements `SyncEngine`. Strategy: full-replace push; remote tracks not found
locally are pulled.

---

## Playlist sync engine

### `struct PlaylistSyncEngine`
Syncs `Playlist` + `PlaylistEntry` sets to/from `{remote_dir}/playlists/{id}.json`.
One file per playlist. Tombstones/deletion packages are out of scope (Round 4).

### `fn PlaylistSyncEngine::new(db: Arc<LibraryDatabase>, driver: Arc<dyn CloudDriver>, remote_dir: impl Into<String>) -> Self`

Implements `SyncEngine`. Strategy:
- Pull: remote playlists not present locally → insert.
- Remote `updated_at` strictly newer than local → pull (overwrite local).
- Equal timestamps → `NoChange`.
- Local newer or no remote → push.

---

## Aggregate trigger

### `async fn run_sync_all(db: Arc<LibraryDatabase>, driver: Arc<dyn CloudDriver>, remote_dir: &str) -> SyncResult<serde_json::Value>`
Run credential + library + playlist sync in sequence. Entry point called by
`wdmm_ffi::wdmm_sync_trigger`.

Returns a JSON summary:
```json
{
  "credentials": {"total": N, "pushed": N, "pulled": N, "conflict": N},
  "library":     {"total": N, "pushed": N, "pulled": N, "conflict": N},
  "playlists":   {"total": N, "pushed": N, "pulled": N, "conflict": N}
}
```
