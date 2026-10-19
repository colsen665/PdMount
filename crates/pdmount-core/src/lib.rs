//! PdMount core library.
//!
//! M0 provides configuration, profile paths and device identity. The
//! filesystem engine (inode table, store, cache, staging, journal, sync) is
//! added from M1 on; see DESIGN.md §3.1.

pub mod config;
pub mod device;
pub mod paths;
pub mod units;

pub use config::Config;
pub use paths::{Profile, ProfilePaths};
