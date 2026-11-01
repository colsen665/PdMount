# PdMount user guide

PdMount mounts Proton Drive "My Files" as a normal folder on Linux, read-write. Changes are saved locally at once and uploaded
in the background.

> PdMount is a third-party application, not officially supported by Proton.

The examples use the default profile. `--profile <name>` before the command selects another one; each profile is one account and one
mount. `pdmount <command> --help` lists the options of a command. Paths given to `pdmount` are absolute inside the mountpoint
or relative to the mount root: with the mount at `~/ProtonDrive`, `~/ProtonDrive/Archive` and `Archive` mean the same folder, while
`/Archive` is refused as outside the mount.

## Build and install

You need a recent Rust toolchain (edition 2024), Go, clang, protoc, pkg-config and `fusermount3` (FUSE 3).

```sh
cargo install --path crates/pdmount
pdmount doctor
```

The build takes a patched Proton Drive SDK from the local repository named in `Cargo.toml` under `[patch.crates-io]`. If you build
on another machine, that repository has to exist there, or the URL has to point to a copy (`docs/sdk-patches.md`).

`doctor` checks FUSE, the configuration, the keyring, the login, disk space and the clock. A warning that `user_allow_other` is not
set in `/etc/fuse.conf` is harmless: it only means that a crashed daemon's mount has to be cleared by hand (see
[Mount and unmount](#mount-and-unmount)).

## Sign in and secret storage

```sh
pdmount login
```

This prints a sign-in link and a code, and opens the link in your browser when you are at a desktop. `--no-browser` only prints it,
`--browser` always opens it, and `auth.browser` in the configuration sets the default. The session and keys are stored in the
keyring. `pdmount logout` ends the session and removes them; unmount first.

Two options choose where secrets go. Use the same one on every command of the profile:

| Option | Secrets are stored in |
|---|---|
| none | the keyring only; if it is unavailable, the command fails and says why |
| `--allow-file-secrets` | the keyring; 0600 files only when the keyring is unavailable |
| `--force-file-secrets` | 0600 files only; the keyring is never used |

Files are the choice for a machine without a keyring, such as a server over SSH. They protect your session only as far as your
user account's file permissions do.

A locked keyring shows an unlock dialog, which may open behind other windows; a note on the terminal says so after 2 seconds.

## Mount and unmount

```sh
pdmount mount               # at ~/ProtonDrive, or `mountpoint` from the configuration
pdmount mount ~/Drive       # somewhere else; the folder is created if missing
pdmount status
pdmount unmount
```

`mount` starts a background daemon and returns when it is ready. Without a session it signs in first; `--no-login` makes it
fail instead, for scripts. `--foreground` keeps it in the terminal.

`mount -o ro` makes every write answer "Read-only file system". The access rules stay as they are, and work already pending
still uploads.

`unmount` first uploads what is pending, for up to 10 minutes, and lists anything it had to leave. Nothing is lost by that: what
remains continues at the next mount. A busy mount stays mounted and the error is reported; close what is using it, or use
`--force`.

**After a crash** or `kill -9`, or when the mount hangs, the folder answers "Transport endpoint is not connected" and `mount`
reports a stale mount. Clear it, then mount again; no reboot is needed:

```sh
pdmount unmount --force
```

`--force` also stops a daemon that is still running. Pending changes are safe: a change that was acknowledged survives a crash of the
daemon (a power loss or kernel crash, only what was fsync'd). If the kernel's connection cannot be ended without root, the command prints the root command to run.

## Autostart

```sh
pdmount autostart enable    # mount this profile at desktop login
pdmount autostart           # show whether it is enabled
pdmount autostart disable
```

`enable` writes `~/.config/autostart/pdmount-<profile>.desktop`, which runs the binary you enabled it with, with the same profile and
secret storage option. If you move the binary, enable it again.

Nothing restarts a daemon that crashes; use `unmount --force` and `mount`. When the desktop session needs a sign-in, you get a
notification; run `pdmount login` or read the link from `pdmount status`.

## Access rules

A rule makes a path and everything below it read-only or read-write. The most specific rule decides.

```console
$ pdmount access set ro Archive
ro /Archive
$ pdmount access set rw Archive/inbox
rw /Archive/inbox
$ pdmount access list
MODE  PATH                      SET AS                    SOURCE  STATUS
ro    /Archive                  /Archive                  cli     active
rw    /Archive/inbox            /Archive/inbox            cli     active
(default: rw)
$ pdmount access effective Archive/old.txt
ro  (rule: /Archive)
$ pdmount access unset Archive/inbox
```

Writes under a `ro` path answer "Read-only file system". A rule follows its folder when it is renamed; when the folder is deleted,
the rule shows as `orphaned`, and `unset` removes it. Rules belong to this device and profile; they don't sync. `access` works while mounted. To set rules in advance, put them in the
configuration under `[access]`; they are imported at mount.

## Local-only files

`.pdignore` lists files that stay on this device and are never uploaded. It uses gitignore syntax and is read from two places:
`<mount>/.pdignore`, which syncs like any other file, and `~/.config/pdmount/<profile>.pdignore`, which does not. The list is empty by default.

Such a file survives remounts but exists nowhere else, so it has no copy on the server.

## Working with git and rsync

git and rsync work on the mount. `fsync` makes a change safe on this machine without waiting for the upload, which
keeps `git commit` fast. To be sure that the server has everything, for instance before you switch devices, run `pdmount sync`.

- **Use one writer per git working tree at a time.** Reading from other devices is fine. No sync filesystem can make two devices
  committing to the same working tree safe.
- **A bare repository on the mount** works as a shared remote as long as pushes come one after another.
- **After a conflict notice under `.git`,** run `git fsck`. Objects and packs don't conflict. A conflict copy of a ref is not a valid ref
  name, so git warns and ignores it. A conflict over the index is fixed by `git status` or `git reset`.
- **rsync:** use one writer per destination tree. Running it again is safe: files with identical content don't conflict.

If specific paths must be on the server when `fsync` returns, set `fsync.remote_paths` or `fsync.mode` in the configuration. Each such
`fsync` then waits for an upload, so don't use it for git repositories.

## Conflicts

When two devices change the same file, nothing is overwritten and both versions are kept. The version that reached the server first
keeps the name; the other becomes a copy beside it, named after the device whose content it holds:

```text
report (conflict laptop 2026-10-19 14-03-27 UTC).odt
```

`status` shows `Conflicts:  N open`, and a desktop notification appears when a session bus is available.

```console
$ pdmount conflicts list
ID   STATE    KIND                         WHEN                    PATH
1    open     edit_edit                    2026-10-31 06:34:03 UTC /docs/c (conflict laptop 2026-10-31 06-34-03 UTC).txt
                                                                   other: /docs/c.txt
$ pdmount conflicts show 1              # both copies, revisions and devices
$ pdmount conflicts resolve keep-both 1
conflict 1 (edit_edit) resolved: both copies kept
```

| Choice | Result |
|---|---|
| `keep-both` | Both files stay; the conflict is marked resolved |
| `keep-local` | The conflict copy takes the name; the other version goes to the trash, or stays as an older revision |
| `keep-remote` | The conflict copy is removed and goes to the trash |

You can also just edit, rename or delete the copies yourself, and then use `keep-both` to close the entry. `conflicts list --all`
includes resolved ones. A conflict is named by its id or by the path of either copy. Write `./2024` for a file named `2024`, since a
number alone is taken as an id.

Some kinds, such as a folder renamed on two devices, are settled automatically and only reported. For those, `keep-both` dismisses
the notice and the other two choices are refused.

The state of a file is also visible as an extended attribute: `getfattr -d <file>` shows `user.pdmount.state` (such as `clean`, `pending`,
`uploading`, `parked`, `conflict`), `user.pdmount.access` and, in a conflict, `user.pdmount.conflict`.

## The upload queue and `sync`

`pdmount sync [<path>]` uploads what is pending now, under the path or everywhere, and waits for it. It exits 1 if something is parked or
after `--timeout` seconds (default 600).

`status` shows `Queue: N pending (oldest 3m), M parked`. Transient errors are retried by themselves. An entry is *parked* when
retrying won't help soon: quota, file size, permissions, or five failed attempts. A new change to the file or the next mount retries it.

```console
$ pdmount queue list
ID         KIND          ATTEMPTS     AGE  STATE                    PATH
6          upload               0      0s  parked: parked by user   /docs/new.txt
$ pdmount queue show docs/new.txt      # adds the last error, base revision and what it waits for
$ pdmount queue retry --all            # upload now; clears parks and back-offs
$ pdmount queue park docs/new.txt      # hold it until `queue retry` or the next mount
$ pdmount queue drop docs/new.txt      # only prints what it would discard
6 /docs/new.txt: would remove it: never uploaded, no other copy exists
Nothing was dropped. Run again with --yes to discard these local changes.
```

`retry`, `park` and `drop` take one of a path, `--id <id>` or `--all`; a folder selects every entry under it.

**`drop --yes` discards your local changes.** A file that exists on the server goes back to the server's version. A file that never
reached the server is removed, with everything in it if it is a folder, and no other copy exists. A new folder may already be on the
server, in which case only what is pending inside it goes.

## Pins and the cache

Files you read are kept in an encrypted block cache of up to `cache.max_bytes` (10 GiB by default); the oldest blocks are evicted first.

A *pin* keeps a file or folder available offline, including new revisions as they appear:

```console
$ pdmount pin docs
Pinned /docs; its files are fetched in the background (pdmount pin list).
$ pdmount pin list
/docs: 2 files, 293.0 KiB of 293.0 KiB, complete
$ pdmount unpin docs
Unpinned /docs.
```

The state is `fetching`, `complete`, `waiting-offline` or `waiting-space`. Pinned data doesn't count against the cache limit, so a pin
is refused if what is known of the path doesn't fit in the free disk space less 1 GiB, and fetching stops below 1 GiB free. `unpin`
takes the path of the pin itself and names that path when you give one inside it. Once unpinned, the blocks become normal cache
blocks.

```console
$ pdmount cache stats
Disk:       293.0 KiB of 10.0 GiB, 1 blocks
Pinned:     0 B (not counted against the cap)
Protected:  0 revisions
Pool slots: 64KiB 0/79, 1MiB 1/7, 4MiB 0/5, scratch 0/4
$ pdmount cache gc --target 2GiB      # evict down to a size; default 80 % of the cap
$ pdmount cache wipe [<path>]         # evict everything allowed, or only under a path
```

Pinned blocks and the base versions of files you have changed are never evicted by these commands.

`pdmount reset --keep-login` rebuilds all local state and the cache, keeping the login and device identity. It is refused while
mounted or with changes pending.

## Offline and sign-in needed

**Offline.** After three failed connections in a row, `status` shows `Connection: offline since <time> (N pending)`. Then:

- cached files and folders you have listed before keep working;
- anything else answers an I/O error after about 2 seconds;
- writes are accepted and queued, and upload when the connection returns, which is checked every 30 seconds.

A profile that has been mounted online once can also be mounted offline.

**Sign-in needed.** When the session can't be refreshed, the mount stays up in the state `needs login`: cached reads and all local
writes go on, and what needs the network answers an I/O error. Run `pdmount login`; the running daemon takes over the
sign-in, keeps the mount and drains the queue. Signing in as a different account than the mounted one is refused.

## `stats` and logs

```sh
pdmount stats                 # counters, gauges and latencies (p50, p99, max)
pdmount stats --json
pdmount stats --reset         # clear the latency histograms after reading
```

Useful lines: `queue_pending`, `queue_parked` and `queue_oldest_secs` for the upload backlog, and `rate_limited` for HTTP 429
answers from Proton. To feed Prometheus, set `stats.prometheus_textfile`.

The log is `~/.local/state/pdmount/<profile>.log`, readable only by you. A background `mount` moves a log over 16 MiB to `<profile>.log.1`.
For more detail, mount with `-v` (info) or `-vv` (debug); `RUST_LOG` overrides these. Secrets are never logged, and the log has
only the sign-in code, not the link.

## Troubleshooting

Start with `pdmount doctor` and `pdmount status`, then the log.

| Symptom | Cause and fix |
|---|---|
| "Transport endpoint is not connected" | The daemon is gone. Run `pdmount unmount --force`, then `mount` |
| The same message only inside a Flatpak app or other sandbox | The sandbox holds a copy of the mount from when it started. The copy stays broken until the sandbox exits; restart the app. Outside it, the mount is unaffected |
| `mount`: "already mounted" or "stale mount" | See `status`; for a stale mount, `unmount --force` |
| `unmount` reports that the mount is busy | Find the user with `fuser -m <mount>`, close it, try again, or use `--force` |
| "Read-only file system" | A `ro` rule (`access effective <path>`) or `mount -o ro` (`status`) |
| "No space left on device" | The local disk or the account's quota is full. `status` shows storage; a parked entry tells which (`queue list`) |
| "Input/output error" on reading | Offline or sign-in needed, and the data is not cached (`status`) |
| A change does not reach other devices | See `queue list` for parked entries and their reasons, then `queue retry` |
| Hard links or symlinks fail with "Operation not permitted" | PdMount does not support them. git copes by itself |
| Keyring errors or hanging commands | Look for an unlock dialog behind other windows, or use the secret storage options above |

You don't need to raise `ulimit -n`: the number of pending files is not limited by the open-file limit.

## Configuration

The configuration is `~/.config/pdmount/<profile>.toml`; without one, the defaults apply. Every key and its default is in
[DESIGN.md, Appendix B](../DESIGN.md#appendix-b-configuration-reference).
