# WDMM OpenAPI Server

Runnable HTTP server exposing WDMM core services.

## Build & Run

```bash
cargo run -p wdmm-openapi --bin wdmm-server
```

Server starts on `http://127.0.0.1:3000`.

## Endpoints

### Health Check
```bash
curl http://127.0.0.1:3000/health
```
Response:
```json
{
  "status": "ok",
  "service": "wdmm-openapi",
  "version": "0.1.0"
}
```

### List Accounts (stub)
```bash
curl http://127.0.0.1:3000/v1/accounts
```
Response:
```json
{
  "accounts": []
}
```

### List Tracks (stub)
```bash
curl http://127.0.0.1:3000/v1/tracks
```
Response:
```json
{
  "tracks": []
}
```

## Configuration

Default: 127.0.0.1:3000 (local only).

For production, implement:
- Token-based auth (see `ServerConfig` in `lib.rs`)
- Bind mode selection (Local/Global/Custom)
- TLS support
