//! `wdmm-core` — pure data models, track identity, revision clock, codecs.
//!
//! **Zero platform dependencies**: this crate must never import `std::fs` or
//! any OS-specific API directly. All I/O is expressed through the
//! `wdmm-platform` adapter traits.

#![forbid(unsafe_code)]
#![allow(dead_code)]

pub mod error;
pub mod identity;
pub mod models;
pub mod rev_clock;

pub use error::WdmmError;
pub type Result<T> = std::result::Result<T, WdmmError>;
