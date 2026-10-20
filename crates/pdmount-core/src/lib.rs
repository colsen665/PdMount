//! PdMount core library.
//!
//! Configuration, profile paths, device identity and the filesystem engine
//! (DESIGN.md §3.1). M1 adds the read side: store, inode table and block
//! cache. Staging, journal and sync follow in M2 and M3.

pub mod cache;
pub mod config;
pub mod device;
pub mod engine;
pub mod errors;
pub mod gate;
pub mod metrics;
pub mod paths;
pub mod singleflight;
pub mod store;
pub mod units;
pub mod vfs;

pub use config::Config;
pub use paths::{Profile, ProfilePaths};
