## Area C2 (M6 notifications: D-Bus, processes and the dependency; M6.5): findings

Reviewed read-only at `m6x-fix` = `7faae80` (scope `git diff 6456097..m6x-fix`). Read in full: `crates/pdmount/src/notify.rs`,
`notify_watch.rs`, `conflict_notify.rs`, `downloads.rs`, the changed parts of `bus.rs`, `keywait.rs` (`handle_attach`,
`restore_*_attach`), `daemon/run.rs`, `main.rs` (module and `fake` gating), `incoming.rs`, `roots.rs`, `roots/shared.rs`,
`doctor.rs`; `crates/pdmount-core/src/slow.rs`, `store/0006.sql`, `store/mod.rs` (`migrate`), the conflict registry and
accessor changes (`conflicts.rs`, `engine/accessors.rs`, `engine/io.rs`); `notify_stub.rs` and `tests/scenarios/lib.sh` only for
their gating and set-up. Other notification callers checked for names: `signatures.rs`, `auth.rs`, `flush.rs`, `resume.rs`.
Dependencies read at the `Cargo.lock` versions: `zbus` 5.19.0 (`src/proxy/mod.rs` `SignalStream`, `src/match_rule/mod.rs`,
`src/connection/handshake/client.rs`, `src/connection/mod.rs` queues, `src/message/header.rs`), `fuser` 0.18.0
(`src/mnt/fuse_pure.rs`, the `/dev/fuse` descriptor's flags). `cargo tree -p pdmount -e features -i zbus` was run in a separate
checkout (offline): zbus is built with `default-features = false, features = ["tokio"]` only. Nothing else was built or run.

Not covered: the behaviour of real notification servers, file managers and `xdg-utils` versions (stated from their documented
behaviour, not tested); the engine's conflict detection itself; the keyring path (unchanged by the diff). Not repeated: V-16
(forged unicast `PrepareForShutdown`), V-28 (age of the peer check before a child), V-10/S-17 (remote names in log fields), V-31
(readiness descriptor inherited), except where M6 adds a new site.

### C2-1: **Show**'s `xdg-open` fallback also runs when the bus did not answer (connection lost, 10 s timeout), with an address that may no longer be this user's bus
- **Rating**: Low
- **Location**: `crates/pdmount/src/conflict_notify.rs:367-386` (`show`: any `ShowError::Call` runs `open_folder`),
  `:399-421` (`show_items`: `Ok(Err(error))` at `:413` — every `zbus::Error`, an I/O error of a closed connection included —
  and the timeout at `:414` both become `ShowError::Call`), `:426-452` (`open_folder`: `DBUS_SESSION_BUS_ADDRESS` = that
  address); DESIGN.md §10.8 (line 1465: "When the call fails on a bus that passed the check (no file manager answers)").
- **Actor**: T1 (another local user) listening at the address of a desktop bus that has just exited; trigger: the user's
  click on **Show** (D102's scenario, now reached by a program the daemon starts).
- **Scenario**: The desktop's bus is a `dbus-launch` bus (`unix:abstract=/tmp/dbus-…` or a socket in `/tmp`). The user clicks
  **Show** and logs out (or the session ends) while `ShowItems` is in flight: the call fails with a closed connection at once,
  or after 10.5 s when a slow activation is cut off by the logout. Both are `ShowError::Call`, so `xdg-open <folder>` starts with
  the dead address; nothing checks it again (`attached_verified`/`bus::connect` are not called), and the address is not dropped.
  Another user who binds the freed abstract name or `/tmp` path first (they are listed in `/proc/net/unix`; a loop can take one
  within milliseconds) receives the connections of `xdg-open`'s children: `gio open`, or the file manager (GDBus, which does not
  check `guid=`, threat model). A GApplication file manager finds its own name "owned" there and hands the open request to the
  other user's process and exits; GVfs calls go to it as well.
- **Impact**: The click is delivered to another user's process, which learns the folder (also in argv) and answers the file
  manager's bus calls (what it shows, mount prompts). No secret is on that path; the window needs a click within a logout.
  Separately, a timeout on a slow but working activation opens the folder twice (the file manager shows it when it starts).
- **Fix**: Run the fallback only on a D-Bus error reply (`zbus::Error::MethodError`/`FDO` such as `ServiceUnknown`, i.e. the bus
  answered), and call `bus::connect` once more immediately before the spawn; on an I/O error or timeout, call
  `bus::drop_if_gone_async` and open nothing.
- **Confidence**: high for the code path; medium for the race being winnable in practice.

### C2-2: The `xdg-open` child gets the daemon's working directory, `$PATH` lookup and environment, and lives in its service
- **Rating**: Info
- **Location**: `crates/pdmount/src/conflict_notify.rs:517` (`PathBuf::from("xdg-open")`), `:426-452` (no `current_dir`, no
  `env_clear`); `crates/pdmount/src/daemon/launch.rs:72-92` (the background daemon inherits the launching shell's working
  directory; only `NOTIFY_SOCKET` is removed there); no `chdir` anywhere in `crates/pdmount/src`;
  `crates/pdmount/src/supervise.rs:119-125` (`NOTIFY_SOCKET` is read, and stays in the daemon's environment under systemd).
- **Actor**: T1 only with a misconfigured `$PATH`; otherwise the user's own environment (T4/T5).
- **Scenario**: (a) `pdmount mount` run from a directory other users can write (`/tmp/x`), with an empty or `.` element in `$PATH`
  (a trailing `:` is enough): glibc's `execvp` resolves `xdg-open` in the daemon's working directory, so a planted `xdg-open`
  runs as the user at the next **Show**. (b) Under systemd the file manager that `xdg-open` starts stays in the unit's cgroup:
  a daemon restart (`Restart=on-failure`) or `service stop` kills the user's window, a copy in progress in it included (D139
  notes the cgroup); it also inherits `NOTIFY_SOCKET` (ignored by systemd with the default `NotifyAccess=main`).
- **Impact**: (a) code execution as the user, conditional on the `$PATH` mistake; (b) a lost window or interrupted copy.
- **Fix**: `chdir("/")` in the daemon at start, resolve the opener once to an absolute path from absolute `$PATH` entries, and
  `env_remove("NOTIFY_SOCKET")` (and, under systemd, start it through `systemd-run --user --scope` or leave the start to D-Bus).
- **Confidence**: high (code read; execvp semantics are glibc's documented ones).

### C2-3: zbus accepts a forged `NameOwnerChanged` from any peer, so `ActionInvoked` can come from a non-owner; **Show** has no rate limit
- **Rating**: Info
- **Location**: zbus 5.19.0 `src/proxy/mod.rs:1268-1282` (`SignalStream::filter`: a message from another sender that parses as
  `NameOwnerChanged` replaces `src_unique_name`, whoever sent it), `src/match_rule/mod.rs:219-228` (a well-known `sender` in a
  rule is not matched locally); `crates/pdmount/src/notify.rs:523-550` (`listen`), `crates/pdmount/src/conflict_notify.rs:870-875`
  (each `show` action spawns `show`, without a limit); `crates/pdmount/src/notify_watch.rs:122-129` (each new owner runs `up`).
- **Actor**: T4 (any process of the user's session bus; dbus-daemon admits only the user's uid, and the bus is peer-checked).
- **Scenario**: A peer unicasts to the daemon's unique name a `NameOwnerChanged("org.freedesktop.Notifications", "", ":1.66")`
  with its own sender; zbus then takes `:1.66`'s `ActionInvoked(id, "show")` as the server's, for ids it reads from the bus
  (`Notify` replies are unicast, but ids are small integers). A loop of such signals starts one `ShowItems` and possibly one
  `xdg-open` per signal; forged owner changes on the watcher's stream make every `up()` re-run the catch-up.
- **Impact**: Same-uid nuisance (windows, processes); no data or secret. Recorded because the D092 sender check of `logind.rs`
  is not repeated for this bus.
- **Fix**: Act on **Show** once per notification (remove the `Open` entry after the first action) and ignore further signals.
- **Confidence**: high (zbus source read).

### C2-4: The unverified-signatures notification still uses `status::printable` for the tree name, not `notify::clean_name`
- **Rating**: Info
- **Location**: `crates/pdmount/src/signatures.rs:417-421` (`" in {}", crate::status::printable(name)` in the summary);
  rule: DESIGN.md §14.4 ("Every name in a notification … is first cleaned by one rule, `notify::clean_name`").
- **Actor**: T2s (a share's owner names the share; the tree name is the shown share name).
- **Scenario**: A share named `Q3\u{202E}txt.exe` makes "PdMount: unverified signatures in Q3exe.txt" read reversed; `printable`
  replaces only `Cc`. Every other caller was moved to `clean_name` in this diff.
- **Impact**: Display spoofing in one summary (the case `clean_name` exists for); the documented rule does not hold.
- **Fix**: Use `crate::notify::clean_name(name)` there (and widen `clean_name` to the zero-width and tag characters V-10 listed).
- **Confidence**: high.

### C2-5: Minor and hardening (Info)
- **Stale notification connections are never pruned** (`notify.rs:450-507`): `ENTRIES` keeps one entry per bus address ever
  used (a new `dbus-launch` address per desktop login), each with a closed zbus connection, its finished listener and up to 512
  ids; only a failed call on that address removes it. Fix: on `bus::attach_changes()`, drop entries whose address is neither
  attached nor the own bus.
- **The attach's display can be cleared by a late drop of the previous bus** (`bus.rs:105-118`, `keywait.rs:722-727`):
  `handle_attach` calls `remember_display(new)` before `keep(new)`; a `forget(old)` in between (a failing call on the old bus)
  runs `detach(old)`, which sets `DESKTOP_DISPLAY = None`, so **Show** then starts `xdg-open` with the daemon's own display
  variables. Fix: set the display inside `keep`/`set_attached` under the `ATTACHED` lock.
- **A new log site writes a remote path raw** (`conflict_notify.rs:375`, `folder = %folder.display()` at INFO): a name with a
  newline forges a log line (V-10/S-17 class). Fix: `?folder` or the cleaning function.
- **A hung notification server costs every conflict a full attempt** (`conflict_notify.rs:524`, `:675-700`, `:661-669`): the
  item channel is unbounded and a failed send retracts its burst slot, so each new conflict tries again (route lookups,
  connect, `GetCapabilities`, `Notify`: up to tens of seconds) while the retry is pending; items are small, so this is delay,
  not memory. Fix: while `retry` is set, hold new conflicts instead of sending.
- **Conflicts counted without ids are counted again** (`conflict_notify.rs:644-650`, `:779-820`, `:859-862`): beyond `MAX_HELD`
  and after a `Lagged`, conflicts go into `missed`; a later catch-up with the window full holds the same unshown conflicts
  again, so the "N more conflicts" count grows with each `Up`/retry, and those never get `notified_at`. Fix: rebuild the
  summary's count from `waiting()` at flush time rather than accumulating it.
- **Threat model out of date for M6** (`docs/security/threat-model.md:69`, `:96-111`): it still describes `notify-send` with the
  kept address; the M6 surfaces are not listed: the daemon's own zbus connection to the notification server (peer uid and,
  with `guid=`, the GUID checked by zbus, `handshake/client.rs:45-53`), `NameOwnerChanged` watching, `ShowItems`, and
  `xdg-open` with the kept address and the attach's display (GDBus children do not check `guid=`, so C2-1's window matters).

## Checked and fine (area C2)
- Every zbus connection to a session bus goes through `bus::connect`/`connect_with` (`bus.rs:188-217`; the only
  `Builder::address` call): notification entries, the watcher, `ShowItems`, name and ID lookups. Before the peer check the
  daemon sends only the EXTERNAL handshake and `Hello`; zbus also refuses a server whose GUID differs from `guid=`.
- A cached connection stays bound to the socket it was checked on: a later listener at the same address cannot take it over;
  a call on a dead entry fails and the retry reconnects with a fresh check (`notify.rs:597-611`, `close` `:649-676`). A
  handle on a bus that is gone or now another user's gets nothing sent (`close` returns false).
- Attach changes: a `watch` generation reaches every watcher (`bus.rs:42-57`, `notify_watch.rs:52-68`, `:138-144`); the name
  and ID caches are invalidated; a dropped bus takes its display with it.
- `ShowItems` URI: built from the path's bytes, so non-UTF-8, `%`, `?`, `#`, space and `<&` are percent-encoded
  (`conflict_notify.rs:281-293`, test at `:1104-1106`); it is ASCII, as `as` requires.
- `xdg-open` argv: one argument, an absolute folder (mountpoint made absolute, `daemon.rs:172-179`; names never `.`/`..`/`/`,
  `vfs.rs:578`), so it cannot be read as an option; no shell; stdio null; own process group; reaped by a task
  (`conflict_notify.rs:443-451`), so no zombie; display variables checked at attach (control characters, length) and removed
  when the attach did not send them. Rust's `Command` resets the signal mask and `SIGPIPE` in the child; `/dev/fuse` is
  `FD_CLOEXEC` (`fuser` `fuse_pure.rs:343`).
- Hostile or slow server (same uid): every call is bounded (2 s method timeout plus a 2.5 s outer deadline, `notify.rs:384-395`;
  connect 2 s plus 2 s for the credentials; `ShowItems` 10 s; `desktop_sent` 10 s; the stop's closes 3 s, `daemon/run.rs:369-377`);
  queues and maps are bounded (`desktop` queue 64 with `try_send`, `Sent` 512, `EVENTS` 64 lossy, `open` 64, `held` 256; the
  download maps by concurrent waits and a 10-minute window); every `Lagged` is handled; capabilities and action keys are only
  compared with constants, and an action must match a handle this process sent; the listener drains its streams without
  awaiting, so zbus's 64-slot match queues do not block replies (V-16's pattern is not repeated).
- Texts: summaries are constants or carry cleaned names; bodies carry cleaned names and are escaped for `body-markup`; actions
  only with `actions`; sizes and counts formatted without overflow.
- `notify_watch.rs` loop: one connection at a time, dropped on return; a missing bus is tried every 60 s with debug-level lines
  only; INFO lines come only from a route change (`bus.rs` `record_choice`) or a drop (once per attach).
- Store schema 6: applied in one transaction with `user_version`; a newer or negative version is refused (`StoreError::Downgrade`,
  `store/mod.rs:339-363`), so a 0.5 binary does not open a 0.6 database; old rows are marked notified; the upsert keeps
  `notified_at`; marks are queued under the registry's lock after their row; no SQL list of ids is built.
- Dependency: the diff does not change `Cargo.lock`; zbus 5.19.0 is the version the 2026-11-15 review read, with only the `tokio`
  feature (no `p2p`, `bus-impl`, `url` or blocking API). Its `unsafe` is in `connection/socket/unix.rs` (descriptor passing,
  credentials), `message/`, `fdo/peer.rs`, `object_server/interface/`, `address/` and the Windows-only `win32.rs`; the new
  calls (`peer_creds`, signal streams, `call_method`) add none. No advisory against zbus 5, zvariant 5, async-broadcast 0.7,
  ordered-stream 0.2 or event-listener 5 is known to this reviewer (no audit tool was run).
- Test-only code: `notify_stub` is `cfg(any(test, feature = "fake-backend"))`, `fake notifications` and the `fake create
  --root` change are inside the `fake-backend`-only `Fake` command, `Bus::Fixed`, `Target::Fixed` and `SlowReads::set_after` are
  `cfg(test)`; production passes only `xdg-open` as the opener; the release build uses default features; the scenario stubs and
  private buses stay under `tests/` and the 0700 scenario directory.
