//! Text handling that must not live in Dart: CUE sheet parsing and subtitle
//! charset detection/decoding (`docs/ARCHITECTURE.md`: Rust owns the execution
//! layer).
//!
//! Both capabilities used to be Dart-only (585 lines of sniffing plus a 222-line
//! CUE state machine). The public API here is what `wdmm-ffi` exposes; the Dart
//! side keeps only models and UI.

pub mod cue;
pub mod subtitle;
