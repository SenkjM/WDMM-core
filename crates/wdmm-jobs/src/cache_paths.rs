//! Cache layout: deterministic paths for cached tracks, manga archives, manga
//! pages and covers. Values must match the Dart `CacheService` layout exactly so
//! files cached by older builds stay usable.
//!
//! Owner: T3/T5 (whichever lands first). Empty until then.

// TODO: pub fn cache_file_for(...), manga_file_for(...), manga_page_dir(...)