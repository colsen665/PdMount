//! `pdmount trash empty` (DESIGN.md §14.2, D036): deletes everything in the
//! account's trash for good. Only while no daemon runs for the profile: a
//! second session resume could rotate the daemon's refresh token.

use crate::control::{self, Request};
use crate::session::{self, Resumed};
use anyhow::bail;
use pdmount_backend::Backend;
use pdmount_core::units::ByteSize;
use pdmount_core::{Config, ProfilePaths};
use std::process::ExitCode;
use std::time::Duration;

pub async fn empty(paths: &ProfilePaths, config: &Config) -> anyhow::Result<ExitCode> {
    let daemon = control::call(
        &paths.control_socket(),
        Request::Status,
        Duration::from_secs(5),
    )
    .await?;
    if daemon.is_some() {
        bail!(
            "profile {} is mounted; unmount it first (the daemon holds the session)",
            paths.profile
        );
    }
    let logged_in = match session::resume(paths).await? {
        Resumed::Active(logged_in) => logged_in,
        Resumed::NotLoggedIn => bail!("profile {} is not logged in", paths.profile),
        Resumed::NeedsLogin(error) => {
            bail!("the session expired, run `pdmount login` ({error:#})")
        }
    };
    let device = session::device_identity(paths, config)?;
    let backend = session::proton_backend(&logged_in, &device).await?;
    let before = backend.storage_info().await?;
    backend.empty_trash().await?;
    // The server may account for the deletion a little later.
    let after = backend.storage_info().await?;
    println!(
        "Emptied the trash of {}. Storage used: {} before, {} now, of {}.",
        logged_in.session.username,
        ByteSize(before.used).human(),
        ByteSize(after.used).human(),
        ByteSize(after.max).human()
    );
    Ok(ExitCode::SUCCESS)
}
