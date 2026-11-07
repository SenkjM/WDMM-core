//! Cover thumbnails. Output path must equal Dart `CoverService.thumbPath`:
//! `<app_docs>/covers/<identityHashStem>.jpg`; sizing/quality must match
//! `resizeCoverToThumb` in the old Dart code.
//!
//! Owner: T4. Empty until the scan executor lands.

// TODO(T4): pub fn write_thumbnail(bytes: &[u8], dest: &Path, size: u32) -> Result<(), ThumbError>