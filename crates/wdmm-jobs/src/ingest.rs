//! Library ingest composition: tag payload → `LibraryTrack` row → artist/album
//! upsert → aggregate/FTS rebuild. Ported from `LibraryService.ingestFromTags`
//! and friends; the identity-hash rules must stay byte-for-byte compatible so
//! existing rows keep matching.
//!
//! Owner: T4. Empty until the scan executor lands.

// TODO(T4): pub fn ingest_from_tags(...)