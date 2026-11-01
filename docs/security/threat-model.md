# PdMount threat model (D070)

Written for the first security review (M4.0.1, 2026-10-27); every later review reuses and updates it.

PdMount is a single-user Linux FUSE daemon (Rust, fuser 0.18) that mounts Proton Drive "My Files" read-write, using
the community Proton SDK through a patched fork. A CLI (`pdmount`) talks to the per-profile daemon over a Unix socket.
Reviews: 2026-10-27 (M4.0.1) at commit e55ed49 (`m4-start`), SDK fork `pdmount` d1c8994 (diff from `base/0.4.0`);
reports in `docs/security/2026-10-27-review/`. 2026-10-31 (M4.16), the M4 changes: `m4-start..4903f03`, SDK fork `pdmount`
`d1c8994..2ae4aa3`; reports in `docs/security/2026-10-31-review/`.

## Assets
- A1 Proton session: access/refresh tokens, key passphrase, SDK secret cache and key store (`sdk-secrets.db`, `sdk-keys.db`, encrypted with a key
  from the keyring or `cache.key`), `credentials.json` (0600 file modes per D029). Compromise = full account access.
- A2 Cache master key and the encrypted block cache (`~/.cache/pdmount/<p>/blocks/`, XChaCha20-Poly1305 per block).
- A3 Plaintext local data: staging files and snapshots (`staging/`, 0700), the SQLite store (names, tree), logs.
- A4 The user's files on the drive: integrity (no silent corruption, no data written to the wrong file or path,
  no unintended deletion/trash) and confidentiality (nothing uploaded that the user did not write there, e.g. data of
  another file, local-only `.pdignore` files).
- A5 The local system of the user: files outside the mount and outside PdMount's own directories must never be
  read, written, deleted or executed because of remote input.

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
  conflict-name generation; log injection.
- T3 The network: TLS for API and block storage (reqwest; check the TLS backend and that certificate validation is
  never disabled), redirects, responses that the SDK passes through unchecked, retry/backoff abuse.
- T4 Local processes of the same uid are NOT generally in scope (they can read the user's files anyway), EXCEPT where
  the design promises more: secrets never printed or logged (tokens, passphrase: the SDK logs the passphrase at debug,
  PdMount filters `proton_sdk_rs2::secret`; the 2026-10-27 review found this incomplete, S-1, fixed in M4.0.2.1), secrets only in files when a flag allows it (D029), the sign-in link
  shown only to the user, `notify-send`/`xdg-open` invoked without shell interpretation of remote-controlled text.
- T5 Configuration and CLI input from the user: mostly trusted, but parsing must not panic into undefined state, and
  paths from config (mountpoint, globs) must not cause writes outside intended places.

- M4 surfaces, under the actors above: the control requests `queue drop`, `queue retry`/`park`, `cache wipe`/`gc`, `pin`/`unpin`, `sync` and
  `stats --reset` (T1 through the socket; T5); the autostart entry (`~/.config/autostart`: Exec quoting, file mode, the secret-storage
  flag it carries; T5); the Prometheus textfile and the rotated logs (file modes, paths, names in them; T1, T2); the staging descriptor
  LRU and pinned blocks (A4: no descriptor reused for another inode, nothing pinned or evicted that breaks a read; T2); the key store
  `sdk-keys.db` and its move out of `sdk-secrets.db` (D085; A1); the fake's `profile.json` and other test switches staying out of shipped builds.

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
