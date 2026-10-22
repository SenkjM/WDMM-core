# wdmm-openapi — Public API

Toggleable axum HTTP server exposing core services as OpenAPI 3.1.
Bind modes: `127.0.0.1` (local) / `0.0.0.0` (LAN) / custom IP. Token-based auth
for external clients; native FFI clients read the token without a network round-trip.

---

## Error types

### `enum ApiError`
`Server(String)`, `Bind(String)`, `NotRunning`.

### `type ApiResult<T> = std::result::Result<T, ApiError>`

---

## Configuration

### `struct ServerConfig` (Serialize, Deserialize, Default)
Fields: `bind_mode: BindMode`, `port: u16`, `token: String` (bearer token).

`Default`: `Local`, port `0`, a random UUID token.

### `enum BindMode` (Serialize, Deserialize)
`Local` → `127.0.0.1`, `Global` → `0.0.0.0`, `Custom` → user IP in `custom_ip`.

### `fn BindMode::to_addr(&self, custom: Option<&str>) -> &'static str`
Return the bind address for `Local`/`Global`. `Custom` returns a sentinel;
callers use `custom_ip` directly in that mode.

### `const TOKEN_KV_KEY: &str = "openapi_token"`
KV-store key under which the bearer token is persisted in `LibraryDatabase`.

---

## Server handle

### `struct ApiServer`
A running server. `Drop` / `.stop()` shuts it down.

### `async fn ApiServer::start(config: ServerConfig, db: Arc<LibraryDatabase>, queue: Arc<TaskQueue>) -> ApiResult<Self>`
Create and start the server. `port = 0` assigns an ephemeral port (read back via `port()`).

Token resolution: reuse the persisted DB token if present; else seed from
`config.token` if non-empty; else generate and persist one.

Errors: `Bind` on `TcpListener::bind` failure.

### `fn ApiServer::port(&self) -> u16`
The bound port.

### `fn ApiServer::token(&self) -> &str`
The token the server started with. For the live (post-rotate) token use `current_token()`.

### `async fn ApiServer::current_token(&self) -> String`
The live bearer token (reflects rotations). Async — token is behind a `RwLock`.

### `fn ApiServer::live_token_arc(&self) -> Arc<RwLock<String>>`
Raw token `Arc<RwLock>` so FFI can update it directly without HTTP.

### `async fn ApiServer::stop(&self)`
Graceful shutdown. Idempotent.

---

## HTTP routes

All routes **except** `/health` require `Authorization: Bearer <token>`.

### `GET /health` (no auth)
Returns `{"status":"ok","service":"wdmm-openapi","version":"…","port":…}`.

### Accounts
- `GET /v1/accounts` — list all accounts (credentials omitted).
- `POST /v1/accounts` — create account. Body: `provider_type`, `display_name`,
  `base_url?`, `credentials_encrypted?`. Returns `{"id":"…"}` (201).
- `DELETE /v1/accounts/:id` — delete. 204.

### Tasks
- `GET /v1/tasks` — list queue tasks.
- `GET /v1/tasks/:id` — single task (404 if absent).
- `POST /v1/tasks/:id/cancel` — cancel task (204 / 404 / 500).

### Token
- `GET /v1/token` — return current token (behind auth).
- `POST /v1/token/rotate` — rotate token, persist it, return `{"token":"…"}`.
  Invalidates the old token immediately.

### Library — tracks
- `GET /v1/library/tracks?q=<query>` — list all, or search if `q` set.
- `GET /v1/library/tracks/:id` — single track (404 if absent).
- `PUT /v1/library/tracks/:id` — upsert track (id taken from path).
- `DELETE /v1/library/tracks/:id` — delete (204).

### Library — playlists
- `GET /v1/playlists` — list all playlists.
- `POST /v1/playlists` — create. Body: `id?`, `name`. Returns `{"id":"…"}` (201).
- `GET /v1/playlists/:id` — single playlist (404 if absent).
- `PUT /v1/playlists/:id` — rename (body `{"name":"…"}`). Preserves `created_at`.
- `DELETE /v1/playlists/:id` — delete; cascades entries (204).
- `GET /v1/playlists/:id/entries` — list entries.
- `PUT /v1/playlists/:id/entries` — replace all entries in one shot. Body: `Vec<PlaylistEntry>`. Returns `{"count":N}`.
