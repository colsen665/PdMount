## Area C3 (session buses, the keyring and credentials, M5 changes): findings

Reviewed read-only at commit 976ea61, `git diff 8b01e65..976ea61` (8b01e65: the M4 release): M5.16, M5.17 and findings 5–9;
decisions D100, D101, D102 with its amendments, D104, D106, D108. Files: `crates/pdmount/src/bus.rs`, `keywait.rs`,
`credentials.rs`, `session.rs`, `auth.rs`, `doctor.rs`, `notify.rs`, `status.rs`, `autostart.rs`, `control.rs`,
`daemon/handler.rs`, `daemon/client.rs`, the `Login` arm of `main.rs`, and `crates/pdmount-core/src/paths.rs`
(`attach_file`). Dependencies read for how PdMount uses them: `zbus` 5.19.0 (`Connection::peer_creds`, unix socket
`get_unix_peer_creds`), `keyring` 3.6.3 (`secret_service.rs`, `error.rs`), `dbus-secret-service` 4.1.0 (`connect`), and the
SDK fork at `caafaea` (`proton-sdk-rs2/src/session.rs` `end_from_session`, `proton-drive-sdk/src/error.rs`
`parse_retry_after`). Threat model: `docs/security/threat-model.md` (D070), including its M5 surfaces paragraph for M5.17.
Known gaps in `TODO.md` (S-10, S-16, S-17, S-21, the P16 decode-helper gap, R-2, R-3) are not repeated except where a
finding goes beyond them.

### C3-1: `login --force` replaces a working session without ending it on the server
- **Rating**: Low
- **Location**: `crates/pdmount/src/auth.rs:341-372` (`adopt`: the new session is stored, and `*self.logged_in.lock().await =
  logged_in` at `:369` drops the old `ProtonAPISession`); `crates/pdmount/src/main.rs:1108-1124` (without a daemon,
  `--force` skips `login_check` and `session::login` stores the new session over the old one); `crates/pdmount/src/session.rs:159-176`
  (`store_login` ends only a session it could *not* store). The fork's `ProtonAPISession` has no `Drop` that ends it; the only
  server-side end is `end_from_session` (fork `crates/proton-sdk-rs2/src/session.rs:576`), called by `logout` and `end_unstored`.
- **Actor**: T4 / A1 hygiene (an earlier copy of the session's tokens in someone else's hands).
- **Attack / failure**: D108 makes `pdmount login --force` the documented way to "sign in anew" while a session works. A user
  who suspects the stored tokens leaked (a copied `credentials.json` from `--allow-file-secrets`, a backup, an old keyring
  export) runs `login --force`. The new session replaces the old one in the keyring and in the daemon, but the old session,
  with its still-valid refresh token, stays open on the server: whoever holds the leaked copy keeps full account access until
  the server expires it, and every `--force` leaves one more live session in the account's session list. Before M5 a daemon
  `login` did the same (D042 `adopt`), but D108 now presents `--force` as the deliberate re-sign-in.
- **Impact**: A1: a session the user meant to replace is not revoked. No local exposure.
- **Fix**: after the new session is stored, end the replaced one on the server (best effort, logged): in `adopt` take the old
  `LoggedIn` out under the same locks and call `end_from_session` on it unless its state was a rejected session; in the
  CLI path, `resume` the stored session first (as `login_check` does) and end it after `store_login` succeeds. Say in DESIGN
  §12 / §14.2 that `login --force` ends the previous session; point users who suspect a leak to `logout` then `login`.
- **Confidence**: high that the old session is not ended; the server-side lifetime of the old refresh token is not verified.

### C3-2: The sign-in link (user code and fork key) goes into `xdg-open`'s and the browser's argv
- **Rating**: Low
- **Location**: `crates/pdmount/src/session.rs:93-101` (`open_in_browser`: `Command::new("xdg-open").arg(url)`),
  `:112-131` (`present_link`), reached from `session::login` and, with a daemon, `crates/pdmount/src/daemon/client.rs:118`
  (`login_via_daemon`, now also for every `login --force` against a working daemon, D108).
- **Actor**: T1 (another local user reading `/proc/<pid>/cmdline`, world-readable unless `/proc` is mounted with
  `hidepid`, which is not the default on this system's distribution).
- **Attack**: another user polls `/proc/*/cmdline` while the victim runs `pdmount login` at a graphical terminal. `xdg-open`
  passes the URL on as an argument to the desktop's opener and then to the browser (`firefox <url>`, `chromium <url>`);
  when no browser instance was running, the browser's main process keeps the URL in its argv for its whole lifetime.
  The URL's fragment holds the user code and the 32-byte fork key (2026-10-27 review, A1-2). With it the other user can
  consume the link first (a denial of the sign-in) and holds half of the fork secret; decrypting the forked key password
  still needs the selector, which stays in PdMount's memory, so this is less than A1-1-style key exposure.
- **Impact**: the threat model's promise "the sign-in link shown only to the user" (T4) is broken toward T1, by the same
  channel for which A1-2 (Medium) removed the link from `notify-send`'s argv. The 2026-10-27 reviews listed "xdg-open argv
  only" as fine (area 1, area 4); that judged shell injection, not the argv exposure. Predates M5.
- **Fix**: open the link without putting it in an argument: write a 0600 HTML file in `$XDG_RUNTIME_DIR/pdmount/` (0700) that
  redirects to the URL (`<meta http-equiv="refresh">` or a script that reads it from its own fragment-free body) and
  `xdg-open` that file's path, removing it after the sign-in; or ask the desktop's portal (`org.freedesktop.portal.OpenURI`,
  the URL goes over D-Bus, not argv) and fall back to printing. Otherwise document the exposure in DESIGN §12.2 and the
  user guide and recommend `--no-browser` on shared machines.
- **Confidence**: high for `xdg-open`'s argv; medium for the browser's (depends on the browser and whether it was running).

### C3-3: The peer-uid check before a keyring call can be seconds or 30 s old, not "a few milliseconds"
- **Rating**: Info
- **Location**: `crates/pdmount/src/credentials.rs:285` (`WRITE_CHECK_PAUSE` 500 ms), `:308-333` (`checked_write`: after a
  `NoReply` on the first `set`, a `get`, then a second `set`), `:351-373` (`raw_set_checked`, run inside the keyring helper);
  `keyring` 3.6.3 `src/secret_service.rs:139-166, 337` (each `set_password`/`get_password` opens new libdbus connections
  through `SecretService::connect`); `crates/pdmount/src/bus.rs:388, 428-469` (`name_state` answers from a 30 s cache without
  connecting), `:775` (`Route::Own`), `crates/pdmount/src/credentials.rs:563-567` (`own_gate` uses that cache).
- **Actor**: T1, after the desktop's bus has exited (logout).
- **Scenario**: the threat model (M5 surfaces, M5.17) says the residual window between the peer-uid check and the child's
  own connection is "a few milliseconds". Two paths make it longer: (1) a timed-out write (D106 amended, rule 3): the
  helper's first `set` gets `NoReply` after 2 s, sleeps 500 ms, reads back and writes the session JSON again, each on a
  fresh connection to the same address made 2.5–5 s after `attached_verified` checked it; a slow keyring at the moment
  the desktop session ends is exactly when `NoReply` is likely. (2) When the keyring is routed to the daemon's own bus,
  the in-process call relies on a `name_state` answer up to 30 s old; for a daemon started in a desktop terminal or by the
  autostart entry, the own bus is the desktop's `dbus-launch` bus, which also ends at logout.
- **Why only Info**: in both cases the connection is made by libdbus, which refuses a server whose GUID differs from the
  address's `guid=`; `dbus-launch` addresses carry one and other users cannot read it (it is only in the environment, the
  0600 log, the 0600 control socket and the 0600 attach file). An exploitable case needs a guid-less address in `/tmp` or
  the abstract namespace. D102 also says the own bus is used "unchecked, as before".
- **Fix**: correct the threat model's sentence (the window covers a timed-out write's retry; the own bus is checked through a
  30 s cache), or make the helper refuse to retry after a `NoReply` unless the parent re-verifies (return a "timed out"
  reply and let the parent run a second helper after `attached_verified`), and bypass the name cache in `own_gate` when the
  operation is a write.
- **Confidence**: high on the timing; not tested.

### C3-4: Minor (Info)
- `crates/pdmount/src/keywait.rs:459` puts `format!("{error:#}")` of a failed backend start into `Wait::Backend`, which
  `status` prints unescaped in its Session line (`crates/pdmount/src/status.rs:250`) and `login` prints as an error. The chain
  can hold the server's error text, and with the fork's decode helpers (known gap, P16) a whole response body: control
  characters from the server reach the terminal. Only a compromised server can write them (out of scope), but this goes
  beyond S-17 (names) and the P16 gap (log only). Pass the reason through `status::printable` (already used for the root lines,
  `status.rs:100`) or truncate it.
- `crates/pdmount/src/keywait.rs:456-460`: `until: now + wait` with `wait = backoff.max(retry_after)`; the fork's
  `parse_retry_after` takes any `u64` (`proton-drive-sdk/src/error.rs:191-194`), so `Retry-After: 18446744073709551615`
  overflows `Instant + Duration` and panics the waiting daemon's task, which leaves it waiting for good. An extension of S-10
  (server actor, out of scope); not traced whether the fork's retry loop sleeps first. Clamp with `checked_add` or a 1 h cap.
- `crates/pdmount/src/credentials.rs:739`: `Instant::now() + timeout` in `run_helper` panics when
  `PDMOUNT_KEYRING_TIMEOUT_SECS` (a test knob read in shipped builds, `:57-63`) is near `u64::MAX`. T5, the S-16 family.
- `keyring` 3.6.3 is built without `crypto-rust`/`crypto-openssl` (workspace `Cargo.toml:36`), so the Secret Service session
  is `EncryptionType::Plain` (`secret_service.rs:141-143`) and the session JSON and cache key cross the bus in clear. Only
  connections of the bus's own uid (a `BecomeMonitor` of the same user) can see them, which is T4 and pre-M5; DH would not
  stop a hostile bus either. Note it in DESIGN §12.4 or enable `crypto-rust`.

## Checked and fine (area C3)
- **Who can attach.** `AttachBus` arrives only over the control socket: 0600 after bind, peer uid checked with
  `SO_PEERCRED` before the first read (`control.rs:689-731`), in `$XDG_RUNTIME_DIR/pdmount/` (0700). The saved attach
  (`paths.rs:272-276`) exists only with a runtime directory; it is written 0600 through `write_private_file`, whose parent is
  made 0700 (`credentials.rs:1278-1304`), and read with `O_NOFOLLOW|O_NONBLOCK`, `fstat` for a regular file of this uid and
  ≤ 16 KiB (`bus.rs:1030-1070`). Other local users can neither send an attach nor plant the file.
- **Address validation** (`bus.rs:107-158`): exactly one `unix:` address, only the keys `path` (absolute), `abstract`
  (non-empty) and `guid`, no `;`, whitespace or control characters, ≤ 1024 bytes, then zbus's parser. `tcp:`, `nonce-tcp:`,
  `unixexec:`, `launchd:`, `autolaunch:`, `systemd:`, `unix:tmpdir=`/`dir=`/`runtime=` are refused. The daemon re-validates the
  socket's address and the saved file's (`keywait.rs:710, 750, 787`).
- **Another user's bus.** Every zbus connection to an attached, saved, service or own address goes through `bus::connect`,
  which checks `SO_PEERCRED` of the listening socket (zbus 5.19.0 `connection/socket/unix.rs:361-380`, cached per
  connection) against `geteuid` before any call; zbus sends only `AUTH EXTERNAL` and `Hello` before that. Another user's
  `dbus-launch` bus on an abstract name, `/run/user/<other>/bus` (0700, `EACCES`) and the system bus (uid 0) all fail. The
  attached address is re-checked by `attached_verified` (`bus.rs:874-883`, fresh `GetId`, no cache) before every keyring
  helper and `notify-send` run; the service's bus by `verify_blocking` (`credentials.rs:377`). A failure drops the address and
  removes the saved file.
- **D101, owner of `org.freedesktop.secrets`.** `GetConnectionUnixUser` is not called, and need not be: on a session bus
  whose listener has this user's uid, dbus-daemon (and dbus-broker) admit only connections of the bus owner's uid, so
  every name owner there is this user (or root). `ListActivatableNames` does not activate; an activatable Secret Service is
  started only by the keyring call itself, on a checked bus of this user. The check-to-call race is same-uid only.
- **Display variables** (`bus.rs:901-997`): the daemon keeps only `DISPLAY`, `XAUTHORITY`, `WAYLAND_DISPLAY`, values
  non-empty, ≤ 4096 bytes, without any `char::is_control` (NUL, newline, CR, tab, C1); other names are refused and logged.
  The same check runs on the client, on the socket request and on the saved file. They go only to the daemon's own bus
  (`UpdateActivationEnvironment` on a peer-checked connection); the daemon's own environment is never changed. After a
  logout a stale `DISPLAY`/`XAUTHORITY` cannot open a dialog on another user's display (X authority cookie; a Wayland socket
  name resolves in this user's runtime directory).
- **Secrets never in argv, logs, notifications.** The keyring helper is `/proc/self/exe keyring-helper` with the bus only
  in its environment; key and value go over stdin, the reply over stdout (1 MiB caps), stderr is `/dev/null`; the child is
  killed and reaped at the timeout (`credentials.rs:731-801`). `HelperRequest`/`HelperReply` `Debug` hide values;
  `keyring::Error`'s `Display` hides `BadEncoding` bytes (`keyring` 3.6.3 `error.rs:69`). The read-back of rule 3 compares
  values and logs only the key name. `notify-send` argv is fixed text plus the profile name (`auth.rs:443-458`,
  `keywait.rs:689-695`). The sign-in link stays out of the log and the notification (`auth.rs:413-440`, test), is kept in
  memory and served only over the 0600 socket (`handler.rs:106-107`).
- **D029 modes with D106.** A failed write in keyring mode returns an error and never falls back to a file
  (`credentials.rs:1106-1121`); unsaved tokens live only in memory and are written by the token saver, which in keyring and
  allow mode first probes without a prompt (`auth.rs:151-170`, `write_now` at `:400`); allow mode's fallback file is 0600; force mode
  never calls the keyring. `Tokens:` shows the keyring's reason only (`session.rs:195-197`). No secret file is created by
  M5 besides these; the attach file and its `.tmp` hold an address and display values.
- **D106 waiting daemon.** The stand-in backend answers `Unauthorized` (no remote writes); a cache key is never made once
  caches are open (`credentials.rs:976-984`, set at `daemon/run.rs:128`); a session that reaches another root is refused
  before its root is stored (`keywait.rs:618-623`), so a keyring of another account cannot redirect the mount.
- **D108.** `login` against a waiting daemon retries the keyring first; `not_needed` replies carry only the account name;
  `adopt` still refuses another account and ends that session (`auth.rs:345-352`). `login_check` refuses while a daemon's pid
  is alive, so a CLI resume cannot rotate tokens under a stopping daemon.
- **`doctor`/`status` output.** Bus IDs, addresses and D-Bus errors come from buses of this user; activation `Exec` lines from
  this user's or the system's service directories; share names and errors in root lines go through `printable`. Only the
  server text of C3-4 is unescaped.
- **Attach autostart entry** (`autostart.rs`): `Exec=<binary> --profile <p> session attach`, quoted as the autostart entry,
  no secret flag; it never overwrites or removes another profile's entry of the same file name.
