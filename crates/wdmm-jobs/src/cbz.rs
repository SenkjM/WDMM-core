//! CBZ (ZIP) helpers: write archives for manga pack/build and read ComicInfo.xml
//! from existing archives. `zip` is already a workspace dependency (read side is
//! used by `wdmm-ffi::ext::zip`); the write side is new.
//!
//! Owner: T5. Empty until the manga jobs land.

// TODO(T5): pub fn write_cbz(...), pub fn read_comic_info(...)