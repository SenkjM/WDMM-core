# WDMM Front/Back Split — Binding Constraints (Slimmed for Repo)

> Source: `/workspace/saf_work/wdmm-fe-be-split-constraints.md` (full 106-line version)  
> User confirmed 2026-10-21. Implementation by Claude; Grok Bot supervision.

---

## 1. Repository & Ownership

| Repo | Responsibility |
|------|----------------|
| **WDMM-core** (this repo) | All business logic, optional OpenAPI, DLNA, unified local proxy; **publishable as crate collection** |
| **Webdav_Media_Manager** | Flutter Android GUI + platform FFI + Kotlin adapters (SAF/MediaStore/FGS); refactor branch |
| **WDMM-desktop** | WinUI 3 (C#) GUI + platform FFI; separate repo |

**FFI vs OpenAPI**: Platform repos use **source-level coupling** (FFI) as primary path. OpenAPI in core is optional (settings toggle).

---

## 2. Coupling & OpenAPI

- **FFI**: Android uses `flutter_rust_bridge`; Windows uses `csbindgen` / P-Invoke.
- **OpenAPI**: Implementation in Rust `wdmm-openapi` crate; shares same core service interfaces as FFI (only transport differs).
- **IP bind modes** (user setting):
  - Local: `127.0.0.1`
  - Global: `0.0.0.0`
  - Custom: user-supplied IP
- **Auth**:
  - Native FFI clients (same machine): **no barrier** — settings UI can display token directly (meant for external clients).
  - External HTTP clients: **must include token** (Bearer).
- **Schema**: WDMM-native models (not OpenList API-compatible at this stage).

---

## 3. Logic归属

**Into Rust (one-shot)**:
- Library DB (`rusqlite`; **no long-term Dart/Rust dual-write window**; migration = cut DB in one step)
- Cloud drivers
- Task queue
- Sync/backup encoding
- Crypto
- **DLNA**
- **Unified local proxy** (all local HTTP proxy capabilities maintained in Rust)

**留 platform / Kotlin·C# adapters**:
- Playback stack (media_kit etc.)
- System media session
- SAF / MediaStore / `.nomedia`
- Foreground service lifecycle
- Pure UI

**Platform adapter layer**: core defines `platform` traits; path/keychain/notification/FS/background-task hooks injected by each platform; OEM exceptions stay out of pure logic.

---

## 4. Windows

- UI: **WinUI 3** (chosen; not WPF).
- With core: source-level FFI; OpenAPI optional (settings toggle).

---

## 5. Drivers Public-Release Risk (Detailed)

"Should `wdmm-drivers` be published to crates.io / open-sourced?" is **not a technical question** — it's a **product & ToS** question:

1. **Service ToS**: Many cloud platforms prohibit or restrict "unofficial clients", "reverse-engineered protocols", "redistributing access logic". Even drivers using official OpenAPI may violate ToS if published as a generic crate encouraging third-party abuse. CtFile / Baidu / etc. require case-by-case ToS review.
2. **Keys & Quotas**: Public crates risk batch abuse of the same `client_id` / `redirect`; if keys are baked into binaries or examples, risk of ban and quota exhaustion. Keys should belong to **application owner config**, not hard-coded in reusable package defaults.
3. **Attack Surface**: List/direct-link/Range/proxy logic exposed → lower-cost scanning & hotlinking scripts. OpenAPI bound to `0.0.0.0` depends on token + optional network security.
4. **Maintenance Expectation**: Publishing to crates.io = promising semver stability; protocol changes → issue flood toward you, not just serving WDMM.
5. **Pragmatic Default (recommendation for implementation default, can change later)**:
   - Core / drivers **source may be open on GitHub** (private or public per your choice), but **not published to crates.io** (yet);
   - Docs state "officially supported first-class consumer is WDMM Android / Desktop";
   - Third-parties wanting access: use **OpenAPI + token**, not by directly depending on unstable driver APIs;
   - If publishing crates later: `wdmm-core` (no concrete cloud protocols) may go first; driver-containing `wdmm-drivers` defaults `publish = false` or feature-gated, and contains no client secrets.

---

## 6. Implementation & Collaboration

- **Task mode**: **One-shot** (land skeleton + ownership per settled constraints, not long-term dual-track).
- **Execution**: Claude Code writes code; **Grok Bot supervises** — reviews results, switches to higher/peer model if needed, or pauses to wait for upstream API recovery; no manual downgrade on failure.
- **OpenList**: Same maintainer ≠ copy API; can learn layering & mount ideas; **mount compatibility not done now**.

---

## 7. Repository Naming (Final Choice)

**Chosen**: **`WDMM-core`** + **`WDMM-desktop`** + existing **`Webdav_Media_Manager`** (refactor branch).

Short, aligns with spoken "WDMM"; crate-internal naming uses `wdmm-*` package names.

---

## 8. Verification & Scope Addendum (2026-10-21)

- **Drivers**: Except netease-related, implement per **official API**; OAuth/keys **user-applied**, repo embeds no keys. Still **no crates.io** (low risk but preserve no-publish stance).
- **Android (current-repo refactor branch)**: Target = current `main` **full feature parity** achievable (via Rust core + FFI).
- **Windows (`WDMM-desktop`)**: Lower bar — entry points / paths written; **smoke test passing** sufficient; not requiring Android-level polish.
- **Repos already created**: https://github.com/SenkjM/WDMM-core , https://github.com/SenkjM/WDMM-desktop (assistant has ADMIN access confirmed).

---

*This is a slimmed copy for in-repo docs. Full version at `/workspace/saf_work/wdmm-fe-be-split-constraints.md`.*
