# PdMount user guide

PdMount mounts Proton Drive "My Files" as a normal folder on Linux, read-write. Changes are saved locally at once and uploaded
in the background.

> PdMount is a third-party application, not officially supported by Proton.

The examples use the default profile. `--profile <name>` before the command selects another one; each profile is one account and one
mount. `pdmount <command> --help` lists the options of a command. Paths given to `pdmount` are absolute inside the mountpoint
or relative to the mount root: with the mount at `~/ProtonDrive`, `~/ProtonDrive/Archive` and `Archive` mean the same folder, while
`/Archive` is refused as outside the mount.

## Build and install

### From a release archive

Each release on the project's GitHub Releases page has `pdmount-x86_64-unknown-linux-gnu.tar.gz` and a `.sha256` file with its
checksum. The binary is for x86-64 Linux with glibc 2.35 or newer (Ubuntu 22.04, Debian 12 and newer). It needs `fusermount3`
(the `fuse3` package) and `libdbus-1` (usually installed already) at run time.

```sh
sha256sum -c pdmount-x86_64-unknown-linux-gnu.tar.gz.sha256
tar -xzf pdmount-x86_64-unknown-linux-gnu.tar.gz
install -m 0755 pdmount-x86_64-unknown-linux-gnu/pdmount ~/.local/bin/
pdmount doctor
```

`~/.local/bin` has to be on your `PATH`. The archive also holds this guide, the licence and `THIRD-PARTY-LICENSES.html`. To start
PdMount automatically, run `pdmount autostart enable` or `pdmount service install` (see [Mount and unmount](#mount-and-unmount)).

### Build from source

You need Rust 1.89 or newer (rustup), Go 1.23 or newer, a C compiler, clang with libclang, protoc, pkg-config, the
`libdbus-1` development files and `fusermount3` (FUSE 3). On Debian or Ubuntu: `apt install build-essential git clang
libclang-dev protobuf-compiler pkg-config libdbus-1-dev fuse3`, plus Rust from rustup and a Go that is new enough.
`scripts/build-doctor.sh` checks each of them and says what is missing.

```sh
cargo install --locked --path crates/pdmount
pdmount doctor
```

The build downloads a patched Proton Drive SDK over HTTPS from the fork named in `Cargo.toml` under `[patch.crates-io]`, at the
pinned commit (`docs/sdk-patches.md`).

### Check the installation

`doctor` checks FUSE, the configuration, the keyring, the login, disk space and the clock. Its keyring line says whether a Secret
Service runs on the session bus, is only started by D-Bus on demand, or is missing, and whether the default collection is locked
(it never opens an unlock dialog). With a user service installed, a `desktop bus` line compares your terminal's session bus with
the service's: it says "same as the service's", "attached" (the service has your desktop's bus, see "Running as a service"),
or warns "not attached" (log in again or run `pdmount session attach`). The `mountpoint` line warns when another
profile's config uses one of this profile's mountpoints. With further mounts (`[[roots]]`), a `root` line per mount says
whether its mountpoint can be used: an empty folder, PdMount's mount, or a folder it can create, not inside My Files' or
another root's mountpoint. For Photos with thumbnails on, the same line checks that `~/.cache/thumbnails` is writable and
private: thumbnails are decrypted pictures, so a warning names any of its folders that group or others can open (`chmod 700`
them). A warning that `user_allow_other` is not
set in `/etc/fuse.conf` is harmless: it only means that a crashed daemon's mount has to be cleared by hand (see
[Mount and unmount](#mount-and-unmount)).

## Sign in and secret storage

```sh
pdmount login
```

This prints a sign-in link and a code, and opens the link in your browser when you are at a desktop. `--no-browser` only prints it,
`--browser` always opens it, and `auth.browser` in the configuration sets the default. The session and keys are stored in the
keyring. `pdmount logout` ends the session and removes them; unmount first.

When the profile is already signed in, `login` says so (`Profile <p> is already signed in as <account>`) and keeps the session;
reading it may show the keyring's unlock dialog. A mounted profile counts as signed in while it is online or offline; one that
is not mounted is checked with Proton first, and if Proton cannot be reached, `login` says why and exits 1. `pdmount login
--force` signs in anew all the same and replaces the stored session, also in a running mount.

Two options choose where secrets go. Use the same one on every command of the profile:

| Option | Secrets are stored in |
|---|---|
| none | the keyring only; if it is unavailable, the command fails and says why |
| `--allow-file-secrets` | the keyring; 0600 files only when the keyring is unavailable |
| `--force-file-secrets` | 0600 files only; the keyring is never used |

Files are the choice for a machine without a keyring, such as a server over SSH. They protect your session only as far as your
user account's file permissions do.

A locked keyring shows an unlock dialog, which may open behind other windows; a note on the terminal says so after 2 seconds.
Before it calls the keyring, PdMount asks the session bus whether a Secret Service runs there. With none, the command fails at
once ("no Secret Service on the session bus"); when D-Bus has to start one first, the note says so, and a start that fails is
reported as "the Secret Service did not start", with the bus and the activation file's command. When a user service's bus
(`$XDG_RUNTIME_DIR/bus`) is not your terminal's, a command also looks there: a keyring running on the service's bus is used
before one that D-Bus would have to start on yours, and `doctor`'s keyring line then says "on the service's session bus".

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

**At logout and shutdown** the daemon gets SIGTERM (SIGINT and SIGHUP do the same) and first uploads what was pending at that
moment, without the usual wait after a change, for up to `timeouts.shutdown_grace` (90 s). It does not wait while offline or signed
out. For a shutdown started from the desktop or with `loginctl`/`systemctl`, the daemon holds logind's shutdown back while it
uploads and shows "PdMount: uploading N changes before shutdown"; logind waits at most its `InhibitDelayMaxSec` (5 s unless root
raised it). The log says how many entries were flushed, or how many are left for the next mount. That mount warns in the log and
with "PdMount: N changes from the last session are still uploading", and says when they are uploaded; the same after a crash with
changes queued. Details: DESIGN.md §14.4.

**After a crash** or `kill -9`, or when the mount hangs, the folder answers "Transport endpoint is not connected" and `mount`
reports a stale mount. Clear it, then mount again; no reboot is needed:

```sh
pdmount unmount --force
```

**Two profiles need two mountpoints.** Every profile mounts at `~/ProtonDrive` unless its config sets `mountpoint`. PdMount
never takes over another profile's live mount: `mount`, a service's start, `unmount` and `unmount --if-stale` of the second
profile refuse with "another PdMount mount (profile …) is at …". Give one of them its own mountpoint, for example
`mountpoint = "~/ProtonDrive-work"` in `~/.config/pdmount/work.toml`. `df` and `/proc/mounts` show whose a mount is as its
source, `pdmount:<profile>`. A mount whose daemon is gone counts as stale for every profile and is cleared as above.

**Further mounts** (shared folders, Photos; see below) start and stop with the daemon. One of them can be unmounted and
mounted again on its own while My Files goes on:

```sh
pdmount unmount ~/ProtonDrive-Shared      # uploads its pending changes first, as above
pdmount mount ~/ProtonDrive-Shared        # again, through the running daemon; -o ro for read-only until the next mount
```

`status` shows a `Root:` line per further mount with its state, and a line per share. Path commands (`access`, `pin`,
`queue`, `conflicts`, `sync`, `cache wipe`) work in My Files only.

`--force` also stops a daemon that is still running. Pending changes are safe: a change that was acknowledged survives a crash of the
daemon (a power loss or kernel crash, only what was fsync'd). If the kernel's connection cannot be ended without root, the command prints the root command to run.

## Files others shared with you

Shares from other Proton users can be mounted on a mountpoint of their own, next to My Files, by the same daemon:

```toml
[[roots]]
kind = "shared"
mountpoint = "~/ProtonDrive-Shared"
```

- Each shared folder is a folder there, under its own name. Files shared on their own are in its folder `Shared files`.
- A share keeps the name it shows under for as long as it is shared with you, also across remounts, so paths to it
  stay valid. A share that arrives later with a name already shown gets its owner added, as
  `Team (bob@example.com)`, and the share that had the name keeps it; this holds for a name that looks like another
  share's, such as `Team (alice@example.com)`, too. Shares with the same name that arrive together each show with their
  owner; two of the same owner get a number, `Team (alice@example.com, 2)`, in the order they were shared. A name
  changes only when its owner renames the share.
- A shared folder where your role is editor or admin is writable (a folder shared with "can edit" in Proton's web
  app shows as `admin` in `pdmount status`: that is the role the server reports for it), the same way as My Files: rsync, git and editors
  work there, and changes upload in the background. A share where you are a viewer, and a file shared on its own,
  answer "Read-only file system". The top level of the mount is read-only too.
- Moving something from one share to another, or between a share and My Files, copies it (each share counts as a
  file system of its own), as `mv` does between two disks.
- Uploads to a share count against its owner's storage. `df` on the mount shows your own quota, so a full share shows
  only as "No space left on device" when a file is closed or synced.
- Conflict copies (`… (conflict …)`) made in a share are visible to all its members and its owner. A conflict there is
  written to the daemon's log and announced on the desktop; `pdmount conflicts` covers My Files only.
- Only your own local-only rules (`~/.config/pdmount/<profile>.pdignore`, see "Local-only files") apply in a share. A
  `.pdignore` in the share's folder is an ordinary file for you: what the owner or other members write into it never
  keeps your files from uploading, nor uploads files your own rules keep local.
- When the owner makes you a viewer while changes are still waiting, or Proton refuses one of them, the share turns
  read-only and the waiting changes are kept: they stay readable in the mount, `pdmount status` lists them as parked
  (with "the server refused a change" when that is why), and a notification says so. They upload when you are an
  editor again, or are tried again at the next mount.
- A new share shows within about 30 s of Proton announcing it, as does a share that was taken back. One taken back also
  goes as soon as PdMount finds its folder gone. A file of it that is still open then answers "Input/output error", and
  its local copy and cache are deleted. Changes that were still waiting, and files your rules kept local-only there,
  are not deleted: they go to `~/.local/share/pdmount/<profile>/orphaned/<share> (<date-time>)/`, the files under
  `files/` at their paths in the share, with `pdmount-orphaned.json` listing every change (renames and deletions
  included; a local-only file as `local-only`). A notification says so,
  and `pdmount status` shows an `Orphaned:` line for each such folder until you delete it. A file whose earlier
  content could no longer be read is listed with the reason, and the share's raw local state is kept beside it in
  `state/`.
- With `pending_marker = true` in the `[[roots]]` entry, a stop whose grace runs out with changes left for a share
  leaves a small marker in the share's `.pdmount/` folder, so that the other members' PdMount shows them as
  `Incoming:`. In a share it names no device. The folder is visible to every member and to the owner. Markers are
  read only in shares you can write to (editor or admin), at most 16 per share, with one notification per share for
  all that arrived together; in a share where you are a viewer none are read or shown, since any member could leave
  them there.
- Files the owner adds or changes show within 30 s in the share you are using, and within 10 minutes in the others, as
  in Proton's own apps.
- `pdmount status` lists each share with your role, when its owner's drive was last polled, and how often it is polled
  now. `pdmount unmount ~/ProtonDrive-Shared` and `pdmount mount ~/ProtonDrive-Shared` work as for any further root.
- At most 32 shares are mounted, the oldest first; the others are listed in `status` as not opened. Each share takes
  about 16 MiB of memory. Started without a connection, the folder stays empty until the list of shares can be read.

## Photos

Your Proton Photos can be mounted read-only on a mountpoint of their own, by year and month:

```toml
[[roots]]
kind = "photos"
mountpoint = "~/ProtonPhotos"
```

- The folders are `<year>/<month>/`, for instance `2024/06/IMG_0001.jpg`, by the time each photo was taken, in your
  local time zone. A photo's modification time is that time.
- The years and months come first, from Proton's photo timeline. A month's file names and sizes are fetched when you
  open it, in batches of 50; a large month fills in over a few seconds, and listing it again shows the rest. Once fetched,
  they are kept: a later mount reads neither the timeline nor the names again.
- A Live Photo shows as two files side by side: the photo and its short video (for instance `IMG_0003.HEIC` and
  `IMG_0003.MOV`). Deleting the photo in Proton's apps removes both.
- New photos show within about 30 s of their upload; deleted ones go as fast.
- Everything is read-only: a write answers "Read-only file system". Upload and delete photos with Proton's apps.
- Opened photos count against `cache.max_bytes` like any further root's files.
- **Thumbnails:** when you open a month, PdMount fetches Proton's own thumbnails of its photos in the background and
  puts them where file managers look for them, `~/.cache/thumbnails` (`$XDG_CACHE_HOME/thumbnails`), in the sizes
  `normal`, `large` and `x-large`. Your file manager then shows the pictures without downloading the photos. They come at
  about two a second, the month you opened last first; ones already there are not fetched again. Each is checked against
  the signature of the photo's upload before it is written; a photo whose thumbnail fails the check, or has none, is
  logged and left to the file manager. Thumbnails match the mount's path, so browse the photos through the mountpoint
  as configured, not through a symlink to it. To turn this off:

  ```toml
  [photos]
  thumbnail_cache = false
  ```

  The thumbnails are decrypted pictures of your photos, readable only by you, as any thumbnail your file manager makes
  (`pdmount doctor` warns when the thumbnail folders are open to others).
  They stay when a photo is deleted, until your file manager or you clear the cache.
- `pdmount status` shows how many photos the mount knows, how many have their names, and whether the timeline has
  been read. `pdmount unmount ~/ProtonPhotos` and `pdmount mount ~/ProtonPhotos` work as for any further root.

## Autostart

```sh
pdmount autostart enable    # mount this profile at desktop login
pdmount autostart           # show whether it is enabled
pdmount autostart disable
```

`enable` writes `~/.config/autostart/pdmount-<profile>.desktop`, which runs the binary you enabled it with, with the same profile and
secret storage option. If you move the binary, enable it again.

A profile uses either autostart or a user service, not both: `autostart enable` refuses while a service is installed for the profile, and `doctor` warns when both exist.

Nothing restarts a daemon that crashes; use `unmount --force` and `mount`. When the desktop session needs a sign-in, you get a
notification; run `pdmount login` or read the link from `pdmount status`.

## Running as a service

A user service starts the mount at login, restarts it after a crash, and stops it with SIGTERM at logout or shutdown, so that the
changes still queued are uploaded first (`timeouts.shutdown_grace`). It is more reliable than autostart; use one of the two.
A start that fails before the mount is ready is not retried: fix the cause the log names and start the service again
(see Troubleshooting).

```sh
pdmount service                 # the manager found, and whether a service is installed for the profile
pdmount service install         # write the files, enable the service and start it
pdmount service uninstall       # stop it (this uploads what is pending), disable it, remove the files
```

`service` takes the global options like any command: `--profile <name>`, and the secret storage option the service should run with
(`--allow-file-secrets` or `--force-file-secrets`; none means the keyring). `--manager openrc|systemd` overrides the manager, which is
otherwise the one found running. A profile uses a service or autostart, not both: `install` refuses while autostart is enabled.
If you move the binary or update PdMount, run `install` again: it rewrites the service files.

**OpenRC.** The files are `~/.config/rc/init.d/pdmount` (shared), the link `pdmount.<profile>` next to it and
`~/.config/rc/conf.d/pdmount.<profile>`; the service is `pdmount.<profile>` in the user's `default` runlevel
(`rc-service --user pdmount.<profile> status|stop|start`). Its output goes to `~/.local/state/pdmount/<profile>.log`, which this way is
not rotated. A shutdown through logind waits for the upload only as long as elogind's `InhibitDelayMaxSec` (5 s by default), so
`install` prints a file for root to create, `/etc/elogind/logind.conf.d/pdmount.conf`, that raises it to the grace; it takes effect at
the next boot, and `doctor` warns while the limit is lower. A `poweroff` or `reboot` run as root does not ask logind and gives
the service about 16 s.

The service uses the session bus of the OpenRC user `dbus` service, `$XDG_RUNTIME_DIR/bus`. Some display managers start the
desktop with a bus of its own (`dbus-launch` from `/etc/X11/xinit/xinitrc.d/80-dbus`, as SDDM's session script does); then the
keyring, the notification server or both run on the desktop's bus. So `install` also writes the desktop entry
`~/.config/autostart/pdmount-<profile>-attach.desktop`, which runs `pdmount --profile <profile> session attach` at each desktop
login: it hands the desktop's bus to the service's daemon (`uninstall` removes the entry; `pdmount service` shows it). From then
on the daemon picks a bus for the keyring and one for notifications, each where that service is running: the desktop's bus if
it runs there, else the service's own, and only when it runs on neither, a bus where D-Bus could start it. So a keyring that
your login started on the service's bus is still used while notifications go to the desktop. `session attach` prints which bus
each one uses, `pdmount status` shows it on its `Bus:` line, and the log has a line each time it changes. Where the desktop uses
the service's bus, as usual with systemd, the attach changes nothing. You can run `pdmount session attach` yourself in a
terminal of the desktop session. A restart of the service (or a respawn after a crash) keeps the desktop's bus: the daemon saves
it in `$XDG_RUNTIME_DIR/pdmount/<profile>.attach` and takes it up again at its start while that bus still runs, so you need not
attach again until the next login. If the attach runs before the service's daemon is up, as can happen at login, it saves the
desktop's bus for the daemon's start and says "its next start takes this session's bus". This replaces the old workaround of installing the service with `--allow-file-secrets` on such
systems.

The attach also passes the desktop's display (`DISPLAY`, `XAUTHORITY`, `WAYLAND_DISPLAY`) to the service's bus when the keyring
or notifications run there, so that programs that bus starts, such as gnome-keyring's unlock dialog, open on your desktop;
`session attach` then prints a second line. Without it, an unlock on that bus failed with "unlock prompt was dismissed". You do
not need to run `dbus-update-activation-environment` for this yourself.

The attach entry is an XDG autostart entry. Desktop environments run those at login, but i3, sway and other plain window
managers do not (unless you run `dex -a` or the like from their configuration). There, add the attach to the window manager's
configuration yourself; `pdmount service install` and `pdmount service` print the line when `XDG_CURRENT_DESKTOP` is `i3` or
`sway` (or not set). For i3, in `~/.config/i3/config`:

```
exec --no-startup-id pdmount --profile <profile> session attach
```

For sway, in `~/.config/sway/config`, the same without `--no-startup-id`. The printed line names the binary by its full path.
Log in again afterwards.

Do not move the desktop onto the service's bus (for example by setting `DBUS_SESSION_BUS_ADDRESS` to
`unix:path=$XDG_RUNTIME_DIR/bus` in `~/.xprofile`): the services that D-Bus then starts for the desktop run outside its login
session, where polkit can refuse them: on Gentoo with SDDM, USB drives no longer mounted in Thunar ("not permitted").

**Before the keyring answers.** A service starts before you log in to the desktop, so its keyring may not be reachable yet, or
still locked. A profile that has been mounted before does not give up then: it mounts at once in the state `needs login`
(`status` names the keyring as the reason), lists what it knows, keeps files you were editing and accepts new writes, which upload
later. Files that are not open for editing answer an I/O error until then, since the local cache is encrypted with a key in the
keyring. A service never shows the unlock dialog by itself at its start: with the keyring locked it mounts this way within
seconds. The daemon tries again at each desktop login (the attach entry) and at `pdmount login`, which may both show the unlock
dialog, and every minute while the keyring is unlocked. One desktop notification tells you, once one can be shown.

If the keyring answers but something else is missing, `status` says what to do, and the daemon tries again only at a desktop
login or a `pdmount login`:
- *no cache key for this profile*: the keyring is likely not the one that holds it (another desktop's). Log in to the desktop
  whose keyring has it, or run `pdmount session attach` there. If the key is lost, restart the mount: its start makes a new key
  and deletes the encrypted caches (the files are downloaded again; a new `pdmount login` may follow). The waiting daemon itself
  never makes a key, so it never deletes the caches;
- *no session*: stop the service, run `pdmount login`, and start it again;
- *a session of another account*: attach the desktop whose keyring holds this profile's session, or stop the service and log
  in with the mount's account.

When Proton or the network fails after the keyring has answered, the daemon waits longer after each failure (from a minute up
to 30 minutes, and as long as Proton asks).

The daemon uses a desktop's bus only while that bus belongs to you: after you log out, another user could take over its
address, so every use checks the user that runs the bus.

**systemd.** The files are `~/.config/systemd/user/pdmount@.service` (shared) and
`pdmount@<profile>.service.d/pdmount.conf` next to it; the unit is `pdmount@<profile>`
(`systemctl --user status|stop|start pdmount@<profile>`, `journalctl --user -u pdmount@<profile>`). systemd stops a user's units
within the stop timeout of `user@.service` (120 s by default), whatever the unit says, so `doctor` warns when
`timeouts.shutdown_grace` + 30 s is more than that; raise `TimeoutStopSec` in a drop-in of `user@.service` as root, or lower the grace.
`install` keeps a running daemon on its old unit; restart it with `systemctl --user restart pdmount@<profile>` to use a new binary.
Whether a logout stops the user manager depends on the system (on the Debian machine of the M5.0 probe it did not, so the daemon kept running); a shutdown always stops it.

Under a service a plain `pdmount unmount` refuses and names the stop command; `unmount --force` still works for a hung daemon.
After it, OpenRC restarts the daemon (`rc-service --user pdmount.<profile> stop` keeps it stopped), while the systemd unit stays
stopped until `systemctl --user start pdmount@<profile>` and stays enabled for the next login. If a daemon you started by hand
(`pdmount mount`, autostart) runs when you `install`, the service is enabled but not started, since the hand-started daemon holds
the mount; `pdmount unmount`, then start the service. `install` refuses while another profile's config uses the same
mountpoint (see "Two profiles need two mountpoints" above), since one of the two services would fail at every login. The
systemd unit clears a mount the daemon left behind when it stops (`pdmount unmount --if-stale`), never one that a daemon still
serves, nor another profile's. `pdmount doctor` checks that the service's stop timeout still matches the config
(it is written at install, so run `install` again after changing `timeouts.shutdown_grace`).

## Telling other devices that changes are coming

When a stop's grace runs out with changes still queued, those changes reach the drive only at the next mount. With

```toml
[sync]
pending_marker = true
```

the daemon then leaves a small file, `.pdmount/<device id>.pending`, in the root of the drive, in the last 10 s of the grace.
It holds the device name, the time and how many changes and bytes are left, but no file names. The next time this device's
queue is empty, the file goes to the trash. It is off by default because it adds a `.pdmount` folder to the drive, which the
official apps show as an ordinary folder.

Reading is always on in My Files and in shares you can write to: when another device has left such a file,
`pdmount status` shows it, and you get a notification:

```
Incoming:   laptop: 12 changes (48.0 MiB), since 18:02 (5m ago)
```

When several devices left one at the same time, one notification names how many (and the first three), not one each.
At most 16 markers are read per drive or share; `status` shows at most 5 lines and then `+N more`. A marker older than
7 days is shown as `stale`; one whose content is not a valid marker (wrong format, a time no clock can hold) is ignored
and logged. It is best effort: nothing is written when the machine is offline, loses
power or crashes, so the absence of a marker proves nothing. In the mount `.pdmount` is read-only ("Read-only file system"),
so `rsync --delete` on the root cannot remove it. Details: DESIGN.md §14.4.

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
In a share from another user only the second applies (see "Files others shared with you").

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
With further roots (`[[roots]]`) the cap covers My Files and all of them together; a root can use free room, but never pushes My
Files below half of the cap. `status` and `cache stats` then show the profile's total against the cap (DESIGN.md §14.5).

**Many files at once.** When programs open many different files in quick succession (a file manager making thumbnails, a
recursive `grep`, a cold copy out of the mount), PdMount treats their first megabytes as a *scan*: they download one at a time,
after whatever you open yourself, without readahead, and fill at most 10 % of the cache. A file you open on its own, and the rest of
a large file being copied, are read at full speed. The thresholds are in `[scan]` (DESIGN.md §6.4, Appendix B); `scan.opens = 0`
turns this off, for instance for a cold copy of many small files that should run unthrottled. `pdmount stats` counts `scans` and
`scan_reads`.

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
mounted or with changes pending. Changes left in a further root's tree name its directory in the refusal; when they cannot be
uploaded any more (a share that is gone), `--orphan-trees` moves that tree's state to `orphaned/` instead (see `pdmount status`).

## Offline and sign-in needed

**Offline.** After three failed connections in a row, `status` shows `Connection: offline since <time> (N pending)`. Then:

- cached files and folders you have listed before keep working;
- anything else answers an I/O error after about 2 seconds;
- writes are accepted and queued, and upload when the connection returns, which is checked every 30 seconds.

A profile that has been mounted online once can also be mounted offline.

**Sign-in needed.** When the session can't be refreshed, the mount stays up in the state `needs login`: cached reads and all local
writes go on, and what needs the network answers an I/O error, and a notification says "PdMount: sign in needed". Run
`pdmount login`; the running daemon takes over the sign-in, keeps the mount and drains the queue. Signing in as a different
account than the mounted one is refused. A sign-in you start yourself with `pdmount login --force` is announced only as
"PdMount: sign-in link ready".

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
| "another PdMount mount (profile …) is at …" (from `mount`, the service's log, `unmount`) | Two profiles use one mountpoint; the other profile's mount is live and is left alone. Set `mountpoint` in one profile's config (`~/.config/pdmount/<profile>.toml`), then mount it, or `pdmount service install` it again. `doctor`'s `mountpoint` line names the profiles that share one |
| `unmount` reports that the mount is busy | Find the user with `fuser -m <mount>`, close it, try again, or use `--force` |
| "Read-only file system" | A `ro` rule (`access effective <path>`) or `mount -o ro` (`status`) |
| "No space left on device" | The local disk or the account's quota is full. `status` shows storage; a parked entry tells which (`queue list`) |
| "Input/output error" on reading | Offline or sign-in needed, and the data is not cached (`status`) |
| A change does not reach other devices | See `queue list` for parked entries and their reasons, then `queue retry` |
| A shared folder answers "Read-only file system" | Your role is viewer, the share is a file shared on its own, or the owner lowered your role or Proton refused a change: `status` shows the role and "parked"; the changes wait and upload when you are an editor again |
| A share disappeared from the mount | The owner stopped sharing it. Changes that were still waiting are in `~/.local/share/pdmount/<profile>/orphaned/` (`status` shows an `Orphaned:` line), never uploaded from there |
| Photos show without thumbnails in the file manager | They come in the background, about two a second, once a month is opened; browse through the mountpoint as configured, not a symlink to it. A photo whose thumbnail failed the signature check is logged and left to the file manager. `[photos] thumbnail_cache = false` turns them off |
| `mount`: "roots: kind "computers" is not available yet" | Computers are not supported in this version; remove that `[[roots]]` entry |
| Hard links or symlinks fail with "Operation not permitted" | PdMount does not support them. git copes by itself |
| Keyring errors or hanging commands | Look for an unlock dialog behind other windows, or use the secret storage options above |
| "no Secret Service on the session bus" (a command, or `doctor`'s keyring line) | Nothing owns or can start `org.freedesktop.secrets` on this process's session bus: start your keyring (gnome-keyring, KWallet's Secret Service) in the desktop session, or use the secret storage options above |
| "the Secret Service did not start" | The bus could start a Secret Service by activation, but it did not answer within 2 s; the message names the bus and the activation file's command. Often the desktop and the user services run separate buses (see "Running as a service", OpenRC) |
| `doctor`: "desktop bus: not attached" | The service runs on another session bus than your terminal and does not have yours: log in again (the attach entry runs at login) or run `pdmount session attach` in a terminal of the desktop session |
| A service shows no notifications, or its keyring is unavailable while commands in a terminal work | The service runs on another session bus than the desktop and has not been given the desktop's: check that `~/.config/autostart/pdmount-<profile>-attach.desktop` exists (`pdmount service install` writes it), or run `pdmount session attach` in the desktop. Do not move the desktop onto the service's bus (see "Running as a service", OpenRC) |
| `status`: "the daemon waits for the keyring" | The service started before the keyring could be read: log in to the desktop, unlock the keyring, or run `pdmount login` (it asks the daemon to try again and may show the unlock dialog). If the reason is a missing cache key or session, do what `status` says (see "Before the keyring answers") |
| A service shows no notifications on i3, sway or another window manager, and `status` has no `Bus:` line | The window manager does not run the attach entry (an XDG autostart entry). Add the line that `pdmount service` prints to its configuration, for i3 `exec --no-startup-id pdmount --profile <profile> session attach`, and log in again (see "Running as a service") |
| `status`: "Tokens: not yet saved to the keyring" | The session works, but the keyring did not take its refreshed tokens. The daemon writes them once the keyring is unlocked (it checks every minute, without a dialog) and at the latest when it stops; unlock the keyring. If the daemon is killed before that, the next start may need `pdmount login` |
| `session attach` or `status` says "keyring: none found" | Neither the desktop's bus nor the service's has a Secret Service running or able to start: unlock or start your keyring (for gnome-keyring, usually at login through PAM), then run `pdmount session attach` again |
| `pdmount login` with a locked keyring: "unlock prompt was dismissed", and no dialog appeared | The keyring runs on the service's bus, which could not open a dialog on your display. Run `pdmount session attach` in the desktop session (or log in again, so the attach entry or the window manager's line runs it); it should print that the service's bus now starts programs on this display. Then run `pdmount login` again |
| `session attach`: "cannot use that bus" | The address in `DBUS_SESSION_BUS_ADDRESS` does not answer; run it inside the desktop session |
| A service start failed and is not retried (OpenRC: "failed to start"; systemd: `status=78/CONFIG`) | A start that fails before the mount is ready is final: the cause (a broken config, a profile never signed in or never mounted, no network on a first mount, a FUSE error) would fail the next try too. Read the error in the log (`~/.local/state/pdmount/<profile>.log`, or `journalctl --user -u pdmount@<profile>`), fix it, and start the service again (`rc-service --user pdmount.<profile> start` or `systemctl --user start pdmount@<profile>`). A daemon that stops after it was ready is restarted as before |
| OpenRC: the service shows stopped, but PdMount still runs (`rc-service … stop` says "already stopped", `start` says "already running"; `status` and `doctor` warn "the daemon runs outside OpenRC's state") | A start that failed under an older service script left its supervisor running, and it started the daemon again behind OpenRC's back. Stop it with `RC_USER_SERVICES=yes RC_SVCNAME=pdmount.<profile> supervise-daemon pdmount.<profile> -U --stop` (it flushes and unmounts as at any stop), run `pdmount service install` so the current script stops the supervisor after a failed start, then `rc-service --user pdmount.<profile> start` if `install` did not start it |

You don't need to raise `ulimit -n`: the number of pending files is not limited by the open-file limit.

## Configuration

The configuration is `~/.config/pdmount/<profile>.toml`; without one, the defaults apply. Every key and its default is in
[DESIGN.md, Appendix B](../DESIGN.md#appendix-b-configuration-reference).
