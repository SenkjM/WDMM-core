# WDMM-core — Agent Instructions

## Repository role

WDMM-core is the Rust business-logic layer shared by all WDMM platform clients.
It owns: cloud drivers, sync/backup, task queue, crypto, DLNA, unified local proxy, OpenAPI.
Platform repos (Android, Desktop) couple to it via FFI; OpenAPI is optional.

## WDMM-VFS is the single authority for cloud file ops

All in-app cloud file operations go through **`WdmmVfs`** (the unified driver trait in `wdmm-drivers`; product name **WDMM-VFS** — formerly `CloudDriver`).

- Sync, backup, and any file I/O goes through `WdmmVfs` methods: `list` / `mkdir` / `remove` / `put_content` / `get_content`.
- **Do not** introduce a `VirtualFs`, `ObjectFs`, or `RemoteStore`-as-public-VFS that duplicates `WdmmVfs`. Private shims that only rename `WdmmVfs` are deleted/inlined; engines take `Arc<dyn WdmmVfs>` directly.
- Account-scoped factory at FFI/outer edge only: `account_id` → `DriverRegistry::create_driver` → `WdmmVfs`.
- Sync FFI takes `account_id + remote_dir (+ with_covers)` — never `url/user/pass`. Backup FFI retains `url/user/pass` until explicitly cut.
- Missing put/get on a driver → driver error. No silent WebDAV-only fallback.
- Wrapper drivers (Crypt) expose `LocalStream` (MustProxy), never a plaintext `raw_url`.
- Content-delivery kinds (`NetworkDirect` / `LocalStream` / `LocalFile`) stay labeled on `CloudFileItem` / driver `get`. Enum is `#[non_exhaustive]`.

## One-shot coding rule (mandatory)

Finish the new authority completely, then delete the old path and wire callers. No feature flags, no dual-write/dual-read, no soft fallbacks, no dead code kept for "mid-step usability". See `/workspace/saf_work/wdmm-fe-be-split-constraints.md` for the full constraints.

## FFI coupling

- Android uses `flutter_rust_bridge` (generated bindings in `Webdav_Media_Manager`).
- Windows uses `csbindgen` / P-Invoke (`WDMM-desktop`).
- OpenAPI in `wdmm-openapi` crate; same service interfaces as FFI; settings toggle.

## Logic ownership

**Into Rust**: Library DB (rusqlite), cloud drivers, task queue, sync/backup encoding, crypto, DLNA, unified local proxy.

**Stay platform (Kotlin/C#)**: Playback stack, system media session, SAF/MediaStore/.nomedia, foreground service lifecycle, pure UI.

## Publishing stance

Core source may be open on GitHub; **not published to crates.io** (yet). Driver crates default `publish = false`. No client secrets in repo.

## Verification

```bash
cargo test -p wdmm-sync && cargo test -p wdmm-drivers
rg -n 'VirtualFs|RemoteStore|WebDavDriver::new' crates src || true
# must be empty in new code
```
