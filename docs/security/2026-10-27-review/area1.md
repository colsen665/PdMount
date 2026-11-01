## Area 1 (CLI binary crate `crates/pdmount/src/`): findings

Reviewed read-only at commit 8c5eda9: main.rs, daemon.rs, control.rs, session.rs, credentials.rs, auth.rs, notify.rs, doctor.rs, status.rs, mounts.rs, access.rs, conflicts.rs, trash.rs, probe.rs, plus `crates/pdmount-core/src/{paths,device,config}.rs` and the SDK fork checkout at `~/.cargo/git/checkouts/proton-sdk-rs2-<hash>/c837e03/` (rev c837e03) for what it logs.

### A1-1: The secret log filter misses two SDK modules that log the key passphrase and unlocked private keys at debug
- **Rating**: High
- **Location**: `crates/pdmount/src/main.rs:329-335` (only `proton_sdk_rs2::secret=warn` is forced); `crates/pdmount/src/daemon.rs:163-165` (daemon started with `-v` repeated `verbose.max(1)` times, so `pdmount -vv mount` runs the daemon at debug); fork `crates/proton-sdk-rs2/src/account.rs:226` `log::debug!("Passphrase: {:?}", ...)` in `get_user_keys`; fork `crates/proton-sdk-rs2/src/api/cache.rs:249` `log::debug!("raw: {:?}", raw)` in `try_get_address_keys` (decrypted JSON of unlocked armored address private keys).
- **Actor**: T4, escalating to T1 through A1-3
- **Attack**: `pdmount -vv mount` / `RUST_LOG=debug` / `-vv login`. The log→tracing bridge is active (`tracing-log`), so SDK `log::debug!` records reach the subscriber. `get_user_keys` runs when user keys are not in the secret cache; `try_get_address_keys` on every resume. Passphrase and unlocked keys land in `~/.local/state/pdmount/<profile>.log` or on the terminal. DESIGN §12.4 claims the filter covers every configuration, citing only `rs2/secret.rs:63,:67`.
- **Impact**: A1 key material in plaintext logs; readable by other users with the default 0644 log (A1-3).
- **Fix**: force `proton_sdk_rs2=warn` (or the two modules) after `RUST_LOG` in both branches; remove the logging lines in the fork as a patch; update DESIGN §12.4; test that the filter disables those targets at debug even with `RUST_LOG=trace`.
- **Confidence**: high

### A1-2: The sign-in link, which embeds the session-fork encryption key, is written to the log and passed as a `notify-send` argument
- **Rating**: High (verifier may settle on Medium)
- **Location**: `crates/pdmount/src/auth.rs:252-256` `tracing::warn!(url, code, ...)`; `auth.rs:257` → `notify.rs:56-60` (URL in argv); fork `crates/proton-sdk-rs2/src/session.rs:258-268`, `:669-679` (URL fragment holds `0:<user_code>:<base64 32-byte key>:external-drive`; the SDK zeroizes it).
- **Actor**: T4; T1 via world-readable log (A1-3) and `/proc/<pid>/cmdline` of `notify-send`; notification history.
- **Attack**: daemon in `needs_login` starts web login (D042); another local user reads the log or polls `/proc/*/cmdline`, gets the code and fork key. With the fork payload (needs selector or TLS position) decrypts the key password; can also consume the link (DoS). `adopt` (auth.rs:272-279) blocks account substitution.
- **Impact**: half of the fork-protocol secret persisted world-readable; design promise "link shown only to the user" broken; full A1 compromise needs an extra precondition.
- **Fix**: never log the URL (log a hint to run `pdmount login`/`status`); keep it in memory, hand it out only via the 0600 socket; notification without the URL, or accept explicitly in D042 and make the log 0600. Update DESIGN §12.2.
- **Confidence**: high that it is logged; medium on severity.

### A1-3: Log file and state directory are created with default (umask) modes
- **Rating**: Medium (High combined with A1-1/A1-2)
- **Location**: `crates/pdmount/src/daemon.rs:149-157`; `crates/pdmount-core/src/paths.rs:194-197`
- **Actor**: T1
- **Attack**: umask 022 + traversable home → others read the log: mountpoint, remote paths of conflicts, names, access-rule paths, the sign-in link (A1-2), at `-vv` A1-1 material.
- **Fix**: state dir 0700 (`DirBuilder::mode`), log `.mode(0o600)` plus `set_permissions` on an existing file; document in §13.1.
- **Confidence**: high

### A1-4: `unmount --force` detaches and aborts any FUSE mount at the mountpoint, not only a pdmount one
- **Rating**: Low (T5)
- **Location**: `crates/pdmount/src/daemon.rs:950-956`, `:970-981`; compare `is_pdmount()` checks at `:824`, `:838`.
- **Attack**: mountpoint holds sshfs/rclone; `unmount --force` lazily unmounts and aborts it.
- **Fix**: act only on `entry.is_pdmount()`, else error.
- **Confidence**: high

### A1-5: Stale pid file plus pid reuse can target another profile's daemon of the same user
- **Rating**: Low
- **Location**: `crates/pdmount/src/daemon.rs:1017-1036` (checks only argv[0] basename), used by `force_unmount` `:957-969`, `preflight` `:222-227`, `refuse_while_running`, `credentials.rs:264`.
- **Attack**: profile A's pid reused by profile B's daemon; `--profile A unmount --force` kills B; `mount A` refused.
- **Fix**: also verify `--profile <name>` and `--foreground` in cmdline, or an flock'ed pid file.
- **Confidence**: high

### A1-6: Profile data directory can exist with the default mode before it is restricted; some files written with default modes
- **Rating**: Low
- **Location**: `crates/pdmount-core/src/device.rs:31-32` (first from `daemon.rs:298`, `status.rs:181`); restriction only in `credentials.rs:493-498` via `session.rs:41-43` or `credentials.rs:475-476`; default-mode files: `proton-root.json` (`session.rs:296-300`), pid file (`daemon.rs:358-362`), `state.db` (`store/mod.rs:108`).
- **Attack**: fake backend with keyring cache key → `create_private_dir` never runs → `state.db` 0644 in 0755 dir. Proton: dir ends 0700 at first resume.
- **Fix**: `ProfilePaths::ensure_dirs()` with 0700 for data and cache dirs before any command; write `proton-root.json` and the pid file via `write_private_file`; document §13.1.
- **Confidence**: high

### A1-7: Minor hardening (Info)
- `credentials.rs:493-498` restricts only the last component; `control.rs:333-341` doesn't verify the mode of a pre-existing `$XDG_RUNTIME_DIR/pdmount`.
- `control.rs:379-381` `read_line` without a length cap; `Drain { timeout_secs }` unbounded (same uid).
- `credentials.rs:478` temp name `cache.tmp`/`credentials.tmp` with `truncate(true)` reuses an existing file's mode; use `create_new`/unique name.
- DESIGN §12.4 names only `rs2/secret.rs` as SDK secret logging.

## Checked and fine (area 1)
- Control socket dir 0700 (`control.rs:333-341`), socket chmod 0600 after bind (`:349-351`), peer uid checked before reading (`:374-378`); serde parsing errors → `ok:false`.
- Profile names restricted to `[a-z0-9][a-z0-9_-]{0,63}` (`paths.rs:33-49`).
- `write_private_file` O_CREAT 0600 + fsync + rename, parent 0700 (`credentials.rs:472-498`); SDK caches chmod 0600 incl. WAL/SHM (`session.rs:60-65`); keyring mode never falls back to files; daemon re-exec carries the mode flag (`daemon.rs:162`).
- No token/passphrase in PdMount's own tracing calls; `StoredCredentials`/`LoggedIn` never Debug-formatted; `proton_sdk_rs2::secret=warn` appended even with `RUST_LOG` (`main.rs:333-335`).
- CLI sign-in link to stdout only (`session.rs:106-124`); `status`/`login` get it over the socket.
- Daemon login refuses another account (`auth.rs:272-279`); unstorable sessions ended.
- Process spawning via argv, no shell: `xdg-open` (hardcoded base URL), `notify-send --`, `fusermount3`.
- `mount_relative` rejects `..` and outside paths (`access.rs:24-53`).
- `unmount --force` abort uses the minor from mountinfo; `kill(pid,0)` EPERM for others' processes.
- `preflight`, `reset`, `trash empty` guards.
- Config errors clean; `~/` only against `$HOME`.
- tracing fields Debug-escape remote names (no log injection).
- `probe.rs` is test-only and `#[ignore]`.
