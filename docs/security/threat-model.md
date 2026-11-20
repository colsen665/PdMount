# PdMount threat model (D070)

Written for the first security review (M4.0.1, 2026-10-27); every later review reuses and updates it.

PdMount is a single-user Linux FUSE daemon (Rust, fuser 0.18) that mounts Proton Drive "My Files" read-write, using
the community Proton SDK through a patched fork. A CLI (`pdmount`) talks to the per-profile daemon over a Unix socket.
Reviews: 2026-10-27 (M4.0.1) at commit 8c5eda9 (`m4-start`), SDK fork `pdmount` c837e03 (diff from `base/0.4.0`);
reports in `docs/security/2026-10-27-review/`. 2026-10-31 (M4.16), the M4 changes: `m4-start..686ded8`, SDK fork `pdmount`
`c837e03..caafaea`; reports in `docs/security/2026-10-31-review/`. 2026-11-15 (M5.8), the M5 changes:
`8b01e65..976ea61`, SDK fork `pdmount` `caafaea`; reports in `docs/security/2026-11-15-review/`. 2026-11-18 (M6.0.3), PdMount `976ea61..9818aa1` and,
for the first time, the release pipeline; SDK fork `pdmount-0.5` `70f10ff` on upstream v0.5.1; reports in
`docs/security/2026-11-18-review/`. 2026-11-20 (M6.5), the M6 notifications: `6456097..7faae80`; reports in
`docs/security/2026-11-20-review/`.

## Assets
- A1 Proton session: access/refresh tokens, key passphrase, SDK secret cache and key store (`sdk-secrets.db`, `sdk-keys.db`, encrypted with a key
  from the keyring or `cache.key`), `credentials.json` (0600 file modes per D029). Compromise = full account access.
- A2 Cache master key and the encrypted block cache (`~/.cache/pdmount/<p>/blocks/`, XChaCha20-Poly1305 per block).
- A3 Plaintext local data: staging files and snapshots (`staging/`, 0700), the SQLite store (names, tree), logs; the
  further trees' stores and staging under `trees/<key>/`, exported changes under `orphaned/`; the Photos tree's store
  (names, capture times, tags, related links); and the decrypted thumbnails PdMount writes to
  `$XDG_CACHE_HOME/thumbnails/{normal,large,x-large}/` (PNG, 0600), which `logout` and `reset` do not remove (V-13).
- A4 The user's files on the drive: integrity (no silent corruption, no data written to the wrong file or path,
  no unintended deletion/trash) and confidentiality (nothing uploaded that the user did not write there, e.g. data of
  another file, local-only `.pdignore` files).
- A5 The local system of the user: files outside the mount and outside PdMount's own directories must never be
  read, written, deleted or executed because of remote input. Designed exception (M5.13b, D115): the Photos root
  writes into the freedesktop thumbnail cache, shared with every program of the user; remote input chooses neither
  the file name (MD5 of a URI under the mountpoint) nor its bytes beyond pixels (a re-encoded PNG), and writes never
  follow a link or leave the size folders.
- A6 The published Releases: the archive and checksum of every version, and their binding to a tag and commit.
- A7 The repository: `master`, the tags and the workflow files, and the SDK fork repository at the pinned commit.
- A8 The development machine as far as the project's scripts touch it: the push credentials (kept outside the
  repository, D090) and the files that `scripts/ci.sh`, `scripts/build-doctor.sh` and the scenario scripts create or
  remove.

## Actors and entry points
- T1 Other local users (different uid, not root): the control socket (`$XDG_RUNTIME_DIR/pdmount/<p>.sock` 0600 with a
  peer-uid check, fallback in the data dir), pid file, file and directory modes of everything PdMount creates
  (config, data, cache, state, runtime dirs, temp files, logs), the mount itself (no `allow_other`, so only the
  mounting user reaches it; `/etc/fuse.conf` lacks `user_allow_other`), `/tmp` or other shared paths, races (TOCTOU,
  symlinks planted in shared or predictable locations).
- T2 Untrusted remote content: the drive is written by other devices of the account, official apps, and possibly
  shared folders. Treat every name, size, mtime, mode xattr, block list, block content, revision metadata and event
  as attacker-controlled input (within what the server permits). Concerns: names with `/`, `\0`, `.`/`..`, over-long
  or invalid UTF-8; path traversal into local paths built from remote names (staging, cache, conflict copies, logs);
  integer overflows in sizes/offsets/block math; huge listings/allocations (memory DoS); integrity of downloaded
  blocks (hash and manifest signature checks, P2); a malicious mode xattr (setuid/setgid bits, device files);
  conflict-name generation; log injection. Pending markers of other devices (`.pdmount/<id>.pending`, D076): capped at
  4 KiB and one block before parsing, fields checked (a `time` out of range refused), device names shown only without
  control and formatting characters; read only in trees the user can write, at most 16 per tree and pass, one
  notification per tree and pass (D126).
- T2s Share members (M5.12, M5.14a): the owner and every editor of a share write into it, and any user can share
  with the user. On top of T2: share names and owner addresses (shown names, D107/D125: a newcomer cannot take an
  existing share's shown name), the share's own `/.pdignore` (not applied to a member's tree, D124), pending markers
  (D126), roles that change at any time (D114), revocation (local changes and local-only files exported to
  `orphaned/`, never deleted, D124). What a member's PdMount uploads into a share is signed by the member's keys.
- T2p Photos (M5.13, D113): the user's own Photos volume, written by the account's devices and apps or any client
  with a session of the account. Every timeline field (capture times of any magnitude, tags of any value, related links
  pointing anywhere), names (equal names too), sizes, revisions, events, and thumbnails: the encrypted size, the WebP
  container and its frame headers (sizes checked before decoding, V-5), the signer address. A valid signature binds a
  thumbnail to its uploader, not to the photo's content. Shared albums are not in the timeline and stay out of scope.
- T3 The network: TLS for API and block storage (reqwest; check the TLS backend and that certificate validation is
  never disabled), redirects, responses that the SDK passes through unchecked, retry/backoff abuse.
- T4 Local processes of the same uid are NOT generally in scope (they can read the user's files anyway), EXCEPT where
  the design promises more: secrets never printed or logged (tokens, passphrase: the SDK logs the passphrase at debug,
  PdMount filters `proton_sdk_rs2::secret`; the 2026-10-27 review found this incomplete, S-1, fixed in M4.0.2.1; and the
  session-fork selector of a web sign-in, which alone authorises collecting the new session's tokens: it reaches the log
  and `status` when a sign-in poll fails, M6.0.3 V-2), secrets only in files when a flag allows it (D029), the sign-in link
  shown only to the user, notifications sent over D-Bus (no program since M6, D138) and `xdg-open` invoked without
  shell interpretation of remote-controlled text.
- T1 for Photos: the modes of the thumbnail cache's folders (the specification asks for 0700; an existing looser folder
  is kept, V-13, and `doctor` warns) and the guessability of its file names.
- T5 Configuration and CLI input from the user: mostly trusted, but parsing must not panic into undefined state, and
  paths from config (mountpoint, globs) must not cause writes outside intended places.
- Release pipeline actors (M6.0.3):
  - S1 A contributor without write access (a pull request from a fork): runs their own code in `ci.yml` and in the
    `plan` job of `release.yml`, with a read-only token and no secrets. Controls: no `pull_request_target`, no cache
    shared with tag builds, the `publishing` condition of the build and host jobs, approval for pull requests from
    all external contributors (a GitHub setting, set 2026-11-19).
  - S2 A compromised build input: the runner image and its apt packages, an action, the dist installer, a crate's build
    script or proc-macro, the Go toolchain, the fork repository. It can alter the archive it builds; the controls limit
    what else it reaches (A6's earlier Releases, A7).
  - S3 A holder of push rights: the maintainer's account, the push key on the development machine, a mistaken tag.
    Any tag matching `**[0-9]+.[0-9]+.[0-9]+*` starts a release, not only `v*`; the release does not wait for CI, and the
    tagged commit need not be on `master`.
  - S4 Someone between the Release and the user: a replaced asset, a mirror.

- M4 surfaces, under the actors above: the control requests `queue drop`, `queue retry`/`park`, `cache wipe`/`gc`, `pin`/`unpin`, `sync` and
  `stats --reset` (T1 through the socket; T5); the autostart entry (`~/.config/autostart`: Exec quoting, file mode, the secret-storage
  flag it carries; T5); the Prometheus textfile and the rotated logs (file modes, paths, names in them; T1, T2); the staging descriptor
  LRU and pinned blocks (A4: no descriptor reused for another inode, nothing pinned or evicted that breaks a read; T2); the key store
  `sdk-keys.db` and its move out of `sdk-secrets.db` (D085; A1); the fake's `profile.json` and other test switches staying out of shipped builds.

- M5 surfaces (M5.1): the system bus connection (`crates/pdmount/src/logind.rs`): logind's delay lock and PrepareForShutdown (T1:
  any local user may send a signal or a message with descriptors to the daemon's connection; only the owner of
  `org.freedesktop.login1` may start a flush, D092); the stop flush (`flush.rs`: `syncfs` on the mount, bounded waits, no new
  path into staging or the store); the shutdown notification (`notify-send` argv: a count and the grace, nothing remote, T4);
  the test-only `pdmount fake logind` and `tests/scenarios/stubs/` staying out of shipped builds and runs.
- M5 surfaces (M5.17, D102, D106): the control request `AttachBus` (T1 through the socket: the address is checked to be
  `unix:path=`/`unix:abstract=` only, so no `unixexec:` or `tcp:`, then connected to with a 2 s limit); the session bus it
  names, on which the daemon calls `GetId` and runs `notify-send` and the keyring helper (a bus the user's own desktop
  hands over; a hostile bus could answer keyring reads with wrong secrets, or log what the daemon writes there, which is
  what any process of the user's session can do already). T1 by another local user: once the desktop's bus has exited
  (logout), its abstract name or `/tmp` socket path is free, and a bus another user listens on there would get the
  session JSON of the next token save and could answer the cache key read with a key of its choosing. Every connection
  to a kept address therefore checks the socket's peer uid (`SO_PEERCRED`, through zbus) against the daemon's, at the
  attach and just before each helper or `notify-send` run; a bus that fails is dropped (`bus.rs` `connect`,
  `attached_verified`, D102 amended). Left: the time between that check and the child's own connection: usually milliseconds, but 2.5–5 s when a
  timed-out token write is read back and retried, and up to 30 s with the keyring on the daemon's own bus (V-28).
  The helper's libdbus refuses a server whose GUID differs from the address's `guid=` (dbus-launch's addresses have
  one; the GUID is not visible to other users), so for the keyring that window needs a GUID the attacker cannot read;
  GDBus in `notify-send` does not check `guid=` (verified with `gdbus call --address`), but its argv and the
  notification carry no secret (D071). Further surfaces: the hidden `keyring-helper` child (secrets over pipes only, never
  argv (D071); replies' `Debug` hides values; killed at the keyring timeout); the attach entry in `~/.config/autostart`
  (T5, Exec quoting as for autostart); a daemon running with the disk cache closed and a stand-in backend until the keyring
  answers (A1: nothing cached is deleted for being unreadable without the key, and a running daemon never makes a
  new cache key, which would delete its caches; D106).

- M6 surfaces (M6.1–M6.4, D137–D140; reviewed 2026-11-20): `notify-send` is gone; the daemon is a client of
  `org.freedesktop.Notifications` itself, over zbus connections made only through `bus::connect` (the peer uid, and
  zbus refuses a server whose GUID differs from `guid=`), one per bus address, kept and reused. Remote text reaches
  the notification server as summaries and bodies: every name is cleaned by `notify::clean_name` (control, separator,
  bidirectional, invisible and blank characters and slash look-alikes become U+FFFD) and escaped when the server has
  `body-markup`; the capabilities are asked again when the service changes owner (M6.5 V-4). What the server does
  with plain text is its own: GNOME Shell and dunst make links of address-like text in a body (M6.5 V-5). Inbound:
  `ActionInvoked` and `NotificationClosed` (passed on only for ids this process sent; any process of the session can
  forge them, T4, so **Show** acts once per notification), and `NameOwnerChanged` of the service (the announcer's
  catch-up of conflicts shown while no service ran). **Show** calls `org.freedesktop.FileManager1.ShowItems` on the
  notification's bus; only when that bus answers with an error does it start `xdg-open <folder>` (from an absolute
  `PATH` directory, in `/`, the bus address and the attach's display in its environment); a bus that does not answer
  gets nothing (M6.5 V-6). The child's GDBus does not check `guid=`, so V-28's window applies to it. The store's
  `conflicts.notified_at` (schema 6) and the slow-download report (`pdmount_core::slow`: file names, debug log only)
  carry no secret. Test-only: `pdmount fake notifications` and `notify_stub.rs` (built with `fake-backend` only).

## Dependencies

Reviewed when a dependency is added; advisories are out of scope (below).

- **`zbus` 5.19 (M5.1, D091, D092).** Built without default features, with `tokio`: no `async-io`, `async-process` or blocking
  API, no object server in use. It adds 18 crates to `Cargo.lock` (`zbus`, `zbus_macros`, `zbus_names`, `zvariant`,
  `zvariant_derive`, `zvariant_utils`, `zcheapstr`, `endi`, `enumflags2` and its derive, `async-broadcast`, `async-recursion`,
  `event-listener`, `event-listener-strategy`, `futures-lite`, `parking`, `ordered-stream`, and `uds_windows`, which is not built
  on Linux); the rest (`rustix`, `serde`, `tokio`, `tracing`, `winnow`) was in the tree already. Four are proc-macros that run
  at build time only (`zbus_macros`, `zvariant_derive`, `enumflags2_derive`, `async-recursion`). The others are zbus's own
  crates and small, widely used async utilities (smol-rs's `event-listener`, `futures-lite`, `parking`, `async-broadcast`).
  Against this model: the peer is the system bus, but its messages come from any local user (T1), so the D-Bus wire parser
  (`zvariant`) is the exposed code; it limits message size and container nesting (32 structs, 32 arrays, 64 in all) as the
  specification does, and the daemon reads only PrepareForShutdown's boolean and two method replies. Descriptors that arrive with any message are received without close-on-exec (`recvmsg` without
  `MSG_CMSG_CLOEXEC`) and closed when the message is dropped, so a child spawned in that moment (`notify-send`, `fusermount3`)
  could inherit one: Low, the sender gave it away already. The lock descriptor itself is duplicated with close-on-exec, so a
  lingering `notify-send` does not hold up a shutdown. Connecting reads `DBUS_SYSTEM_BUS_ADDRESS` (T5). No network, no files
  written, no secrets pass through it. Rated: no finding above Low.

- **WebP and PNG (M5.13b, D115).** `image-webp` 0.2.4 (the exposed parser: every frame's size is read from its header
  and bounded before it decodes, V-5), `image` 0.25.10, asked for without formats and used for scaling only; the SDK's
  default `thumbnail-generation` feature builds its decoders into the binary, and no PdMount code calls them (M6.0.3
  V-19), `png` 0.18.1 (encoder, and the decoder for text chunks of local thumbnails), `md-5` 0.10.6 (cache file names,
  not a security use). Decoding runs in a blocking task under `catch_unwind`; an allocation failure would still abort.
  Reviewed in M5.8: one Medium (V-5), fixed.

## Build-time supply chain (D097, M5.15)

The release archive is built on a GitHub-hosted runner, not on the maintainer's machine. What goes into that build, and
how much it is trusted:

- **Runner image.** `ubuntu-22.04` (GitHub-hosted, updated by GitHub): the C toolchain, glibc 2.35 and the apt packages
  `clang`, `libdbus-1-dev`, `pkg-config` and `protobuf-compiler` come from Ubuntu's archive without version pins. Trusted as
  the OS vendor's; a compromise would change the binary without a change in the repository.
- **dist (cargo-dist 0.32.0).** `release.yml` is generated by dist and checked against `dist-workspace.toml`, so it is not
  hand-edited. Its actions (`actions/checkout`, `upload-artifact`, `download-artifact`, `attest`) are pinned by commit through
  `github-action-commits` in `dist-workspace.toml` (M6.0.4.20, M6.0.3 V-6), updated together by hand. It installs the `dist`
  binary at the pinned version with the installer script from dist's GitHub release, with no hash check (dist offers none).
  Reviewed when dist is upgraded (`dist init`, then read the diff of `release.yml`). The workflow grants `contents: write`
  workflow-wide and uses only `GITHUB_TOKEN`; no other secrets exist. Since M6.0.4.4 (`github-attestations = true`) the
  build job, where every build script and proc-macro runs, has a permissions block of its own: `contents: read`, and
  `attestations: write` and `id-token: write` for the attestation, so a malicious build input can at most attest the
  archive it already controls (M6.0.3 V-4). Residue: the `plan` job, which pipes the dist installer into `sh` and creates
  the draft Release, and the `build-global-artifacts` (checksums only, no cargo build), `host` and `announce` jobs keep the write token. Pull requests run `dist plan`
  only (`pr-run-mode = "plan"`). A pull request from a fork runs the workflow of its own merge commit, so what keeps it
  from a release is the read-only token GitHub gives such runs and the `publishing` condition, not the run mode.
- **Hand-written workflows and steps** (`ci.yml`, `.github/build-setup.yml`): actions pinned by full commit (`actions/checkout`,
  `actions/setup-go`), `permissions: contents: read`, `persist-credentials: false`, no `pull_request_target`, no secrets.
  The Rust toolchain comes from rustup at a pinned version, in `ci.yml` and, through `.github/build-setup.yml`, in the
  release build, which `ci.yml` checks to agree (M6.0.4.21, M6.0.3 V-7); Go from `actions/setup-go` at `1.25.x` (the newest patch release of a pinned minor, so that the
  Go standard library in the binary has its security fixes).
- **Rust and Go dependencies.** `Cargo.lock` is checked in. `scripts/ci.sh` builds with `--locked`; dist's build does not
  pass it, so the release job first runs `cargo metadata --locked`, which fails when the lock file would change
  (M6.0.4.21, M6.0.3 V-7); the SDK fork at a pinned commit. The Go library of proton-crypto is built from modules vendored in the `gopenpgp-sys` crate (no download
  at build time).
- **cargo-about (0.9.3).** Generates `THIRD-PARTY-LICENSES.html` from `Cargo.lock`, locally; the result is checked in and
  `ci.yml` regenerates it and fails on a difference. It runs no part of the release build. A licence outside the accepted
  list in `about.toml` fails the generation.
- **What a tag push does.** A pushed tag matching `**[0-9]+.[0-9]+.[0-9]+*` starts the release build (`v0.6.0`, but also
  `0.6.0` or `x-0.6.0`), from whatever commit it names. Whoever can push tags can publish a Release, and the tag name is
  expanded in the workflow's shell commands. The controls are GitHub settings: a ruleset restricting the creation,
  update and deletion of all tags to the maintainer, a ruleset for `master`, immutable releases, and read-only default
  workflow permissions, with no bypass for GitHub Actions; Actions are disabled on the SDK fork. The user set or checked
  all of them on 2026-11-19 (M6.0.3 V-4, V-5), so the write token of the remaining release jobs reaches neither published
  Releases nor tags nor `master`. The archive's `.sha256` is produced in the same
  run, so it proves integrity of the download, not provenance. Since M6.0.4.4 the build job attests the archive (GitHub's
  build provenance, `gh attestation verify`), which binds it to the repository, the workflow and the commit; it does not
  prove that the build inputs were sound (M6.0.3 V-15).
- **Local archive check.** `dist build` on the maintainer's machine embeds local build paths in the binary and is never
  published. The hosted build embeds only paths under the runner's home, among them the directories OpenSSL would load
  engines and providers from, which nothing in PdMount asks for (M6.0.3 V-16).
- **Not done:** reproducible builds, signed archives, hash-pinned apt packages, a hash check of the
  dist installer.

## Out of scope
- root; a compromised Proton server or Proton's crypto design; the upstream SDK and third-party crates except how
  PdMount uses them (the fork's own patches ARE in scope); dependency CVE scanning (`cargo audit` not run, D023);
  physical access / power loss.
- Access rules (`pdmount access set ro`) are a safety feature against the user's own tools, not a security boundary
  (the same user can change them); report bypasses as Low unless they cause data loss.

## Ratings
- Critical: remote or other-user compromise of A1, or arbitrary file write/exec outside PdMount's dirs (A5).
- High: other-user read/write of A1–A3; remote-triggered write/delete outside the mount; silent corruption or loss
  of A4 triggered by untrusted input; secrets in logs or world-readable files.
- Medium: DoS of the daemon or memory exhaustion from remote input; integrity check gaps without a shown exploit;
  weaker-than-designed file modes on non-secret data.
- Low / Info: hardening, defence in depth, unclear but plausible issues.
- Release pipeline (A6–A8). Critical: someone without write access publishes or changes a Release, or changes
  `master` or a tag. High: the same by a holder of a narrower right than the maintainer's (a restricted tag or branch
  right, a pull request).
  Medium: a compromised build input (S2) reaches beyond the build it runs in (earlier Releases, `master`, tags) where a
  stated or cheap control would have contained it. Low / Info: hardening, a stated control that a normal release does
  not depend on.
