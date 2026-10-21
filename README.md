# WDMM-core — Rust Business Logic Layer

**WebDAV Media Manager** front/back split: all business logic, optional OpenAPI server, DLNA/UPnP cast, unified local proxy.

This is a **Cargo workspace** with 10 crates. Source-level coupling to platform repos (Android Flutter via `flutter_rust_bridge`, Windows C# via `csbindgen`). **Not published to crates.io** — see `docs/CONSTRAINTS.md §5` for rationale.

---

## Workspace Structure

| Crate | Role |
|-------|------|
| `wdmm-core` | Pure data models, track identity (SHA-1), `RevClock`, zero platform deps |
| `wdmm-crypto` | AES-256-GCM credential vault + backup envelope (PBKDF2 600k iterations) |
| `wdmm-drivers` | `CloudDriver` trait; **WebDAV list working**, others stubbed |
| `wdmm-storage` | rusqlite library DB (schema v1 skeleton, aligns WDMM library direction) |
| `wdmm-queue` | Task-queue FIFO state machine + resumable download stub |
| `wdmm-sync` | Incremental sync engine (credentials / playlists / library / manga) stub |
| `wdmm-dlna` | DLNA/UPnP SSDP discovery + AVTransport SOAP stub; owns local proxy |
| `wdmm-platform` | Adapter traits (keychain / notification / FS / wakelock) + std default impl |
| `wdmm-openapi` | Toggleable axum HTTP server (/v1/ routes, bind modes, token auth) |
| `wdmm-ffi` | C ABI surface (`cdylib`/`staticlib`) for FRB (Android) / csbindgen (Windows) |

---

## Quick Start

### Build & Test

```bash
cargo check --workspace
cargo test --workspace
cargo build --release
```

### Run OpenAPI Server Locally

```rust
use wdmm_openapi::{ApiServer, BindMode, ServerConfig};

#[tokio::main]
async fn main() {
    let config = ServerConfig {
        bind_mode: BindMode::Local,
        port: 8080,
        token: "dev-token-123".into(),
    };
    let server = ApiServer::new(config);
    server.start().await.unwrap();
    println!("OpenAPI: http://127.0.0.1:8080/health");
    // Keep running...
    tokio::signal::ctrl_c().await.unwrap();
}
```

Access:
- **Health**: `GET http://127.0.0.1:8080/health` (public)
- **Accounts**: `GET http://127.0.0.1:8080/v1/accounts` (stub, returns `[]`)
- **Tracks**: `GET http://127.0.0.1:8080/v1/tracks` (stub, returns `[]`)

**Bind modes**:
- `Local` → `127.0.0.1` (same-machine clients only)
- `Global` → `0.0.0.0` (LAN accessible; requires token for external clients)
- `Custom` → user-supplied IP

**Auth**: Native FFI clients (same machine) skip token. External HTTP clients must include `Authorization: Bearer <token>`.

---

## FFI Hookup

### Android (flutter_rust_bridge)

1. Add `flutter_rust_bridge` to `pubspec.yaml` + `build.gradle.kts` integration with `cargo-ndk`.
2. Generate Dart bindings:
   ```bash
   flutter_rust_bridge_codegen generate \
     --rust-input crates/wdmm-ffi/src/lib.rs \
     --dart-output lib/generated/ffi_bindings.dart
   ```
3. Call from Dart:
   ```dart
   import 'package:flutter_rust_bridge/flutter_rust_bridge.dart';
   import 'generated/ffi_bindings.dart';

   final api = WdmmCore(dylib: loadDylib());
   final version = await api.wdmmVersion();
   print('Core version: $version');
   ```

### Windows (csbindgen)

1. Build `wdmm-ffi` as `cdylib`:
   ```bash
   cargo build --release --package wdmm-ffi
   ```
2. Generate C# bindings with `csbindgen`:
   ```bash
   csbindgen --input target/release/wdmm_ffi.dll --output NativeMethods.g.cs
   ```
3. Call from C#:
   ```csharp
   using System.Runtime.InteropServices;

   var version = NativeMethods.wdmm_version();
   Console.WriteLine($"Core version: {Marshal.PtrToStringAnsi(version.ptr)}");
   NativeMethods.wdmm_ffi_free_string(version);
   ```

---

## Architecture Notes

- **Pure logic / zero platform deps**: `wdmm-core` never imports `std::fs`. All I/O goes through `wdmm-platform` adapter traits.
- **Drivers — official API only**: WebDAV works. baidu / aliyundrive / 115 / ctfile / terabox / netease are stubbed (TODO per architecture report §4.3 phasing). OAuth keys are **user-supplied** (not embedded).
- **DB migration**: `wdmm-storage` uses rusqlite; migration from Dart `sqflite` is **one-shot** (no long-term dual-write window).
- **DLNA + local proxy**: owned by Rust; Android/Windows delegates cast operations via FFI.
- **Publish = false**: Workspace default. See `docs/CONSTRAINTS.md §5` (service ToS, key leakage risk, attack surface).

---

## Known Gaps (First Bootstrap)

- Most drivers are stubs (WebDAV only working driver)
- OpenAPI auth middleware not enforcing token yet
- Queue task persistence goes to in-memory Vec (not yet backed by wdmm-storage)
- Sync engines are interface-only stubs
- DLNA discovery/cast not implemented

Full implementation continues in subsequent rounds per `saf_work/wdmm-split-impl-brief.md` phasing.

---

## References

- **Constraints**: `docs/CONSTRAINTS.md`
- **Architecture Report**: `/workspace/saf_work/wdmm-fe-be-split-architecture-report.md`
- **Brief**: `/workspace/saf_work/wdmm-split-impl-brief.md`
- **Platform Repos**:
  - Android: `Webdav_Media_Manager` (branch `refactor/rust-core`)
  - Windows: `WDMM-desktop` (separate repo)

---

License: **Apache-2.0** (per existing LICENSE file)
