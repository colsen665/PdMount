//! PdMount core library.
//!
//! Configuration, profile paths, device identity and the filesystem engine
//! (DESIGN.md §3.1): store, inode table, block cache, staging and the
//! journal executor. The event-driven sync engine follows in M3.

pub mod access;
pub mod cache;
pub mod config;
pub mod device;
pub mod engine;
pub mod errors;
pub mod events;
#[cfg(test)]
pub(crate) mod fake_fs;
pub mod gate;
pub mod journal;
pub mod metrics;
pub mod paths;
pub mod pdignore;
pub mod rows;
pub mod singleflight;
pub mod staging;
pub mod store;
pub mod units;
pub mod vfs;

pub use config::Config;
pub use paths::{Profile, ProfilePaths};
