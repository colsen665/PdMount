//! A stand-in for logind on a private bus, for scenario X11 (test builds,
//! DESIGN.md §15.3): `pdmount fake logind serve` owns
//! `org.freedesktop.login1` on the bus that `DBUS_SYSTEM_BUS_ADDRESS` names,
//! hands out delay locks, and writes what happens to a log, one line per
//! event with Unix milliseconds:
//!
//! - `<ms> ready`
//! - `<ms> inhibit <n> <what> <mode>`: lock `n` was taken
//! - `<ms> refused`: an `Inhibit` was refused (`--refuse`)
//! - `<ms> released <n>`: every copy of lock `n` was closed
//! - `<ms> prepare <true|false>`: `PrepareForShutdown` was sent
//!
//! `pdmount fake logind prepare true|false` asks it to send
//! `PrepareForShutdown`.

use anyhow::Context;
use futures::StreamExt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use zbus::message::Type;
use zbus::{Connection, MessageStream};

const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
/// The stub's own interface, for `prepare`.
const TEST: &str = "org.pdmount.TestLogind";

#[derive(Clone)]
struct Log(Arc<Mutex<std::fs::File>>);

impl Log {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        Ok(Self(Arc::new(Mutex::new(file))))
    }

    fn line(&self, text: &str) {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        if let Ok(mut file) = self.0.lock() {
            let _ = writeln!(file, "{ms} {text}");
        }
    }
}

/// Serves until killed.
pub async fn serve(log: PathBuf, refuse: bool) -> anyhow::Result<()> {
    let log = Log::open(&log)?;
    // Callers start after `ready`, so the stream misses nothing.
    let connection = zbus::connection::Builder::system()?
        .name(LOGIND)?
        .build()
        .await
        .context("cannot connect to DBUS_SYSTEM_BUS_ADDRESS")?;
    let mut messages = MessageStream::from(&connection);
    log.line("ready");
    let mut locks = 0u32;
    while let Some(message) = messages.next().await {
        let message = message?;
        if message.message_type() != Type::MethodCall {
            continue;
        }
        let header = message.header();
        let interface = header.interface().map(|name| name.as_str().to_owned());
        let member = header.member().map(|name| name.as_str().to_owned());
        match (interface.as_deref(), member.as_deref()) {
            (Some(MANAGER), Some("Inhibit")) => {
                let (what, _who, _why, mode): (String, String, String, String) =
                    message.body().deserialize()?;
                if refuse {
                    connection
                        .reply_error(
                            &header,
                            "org.freedesktop.DBus.Error.AccessDenied",
                            &"refused by the test",
                        )
                        .await?;
                    log.line("refused");
                    continue;
                }
                locks += 1;
                let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
                connection
                    .reply(&header, &zbus::zvariant::Fd::from(&write))
                    .await?;
                drop(write);
                log.line(&format!("inhibit {locks} {what} {mode}"));
                // The read end sees EOF once every copy of the write end is
                // closed, as logind notices a released lock.
                let log = log.clone();
                let id = locks;
                std::thread::spawn(move || {
                    let mut read = std::fs::File::from(read);
                    let mut buffer = [0u8; 16];
                    while matches!(read.read(&mut buffer), Ok(n) if n > 0) {}
                    log.line(&format!("released {id}"));
                });
            }
            (Some(TEST), Some("Prepare")) => {
                let active: bool = message.body().deserialize()?;
                connection
                    .emit_signal(
                        None::<&str>,
                        LOGIND_PATH,
                        MANAGER,
                        "PrepareForShutdown",
                        &active,
                    )
                    .await?;
                log.line(&format!("prepare {active}"));
                connection.reply(&header, &()).await?;
            }
            _ => {
                connection
                    .reply_error(
                        &header,
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        &"not in the stub",
                    )
                    .await?;
            }
        }
    }
    Ok(())
}

/// Asks the stub to send `PrepareForShutdown(active)`.
pub async fn prepare(active: bool) -> anyhow::Result<()> {
    let connection = Connection::system()
        .await
        .context("cannot connect to DBUS_SYSTEM_BUS_ADDRESS")?;
    connection
        .call_method(Some(LOGIND), LOGIND_PATH, Some(TEST), "Prepare", &active)
        .await?;
    Ok(())
}
