//! PdMount core library.
//!
//! Configuration, profile paths, device identity and the filesystem engine
//! (DESIGN.md §3.1): store, inode table, block cache, staging, the journal
//! executor and the event-driven sync engine ([`events`]).

pub mod access;
pub mod cache;
pub mod config;
pub mod conflicts;
pub mod device;
pub mod engine;
pub mod errors;
pub mod events;
#[cfg(test)]
pub(crate) mod fake_fs;
pub mod gate;
pub mod health;
pub mod journal;
pub mod limits;
pub(crate) mod lock_order;
pub mod metrics;
pub mod paths;
pub mod pdignore;
pub mod photos;
pub mod poller;
pub mod pool;
pub mod readonly;
pub mod rows;
pub mod scan;
pub mod singleflight;
pub mod staging;
pub mod store;
pub mod units;
pub mod vfs;

pub use config::Config;
pub use paths::{Profile, ProfilePaths};
