# Verification of the M5.8 area reports (verifier, 2026-11-15)

Reviewed tree: worktree at `976ea61` (scope `8b01e65..976ea61`, the M5 changes), SDK fork at `caafaea` (cited `sdk/<path>:line`),
official SDK at tag `js/v0.21.0` (cited `cs/<path>:line`). Threat model: `docs/security/threat-model.md` (D070), its Ratings
section. Area reports: `area1.md` … `area5.md` in this directory (finding IDs C1-n … C5-n). Method: read-only; every cited code path
below was traced in the source, and dependencies were read at the versions in `Cargo.lock` (`image-webp` 0.2.4, `zbus` 5.19.0,
`tracing-subscriber` 0.3.23, `proton-rpgp` 0.3.1). Nothing was built or run. Line numbers are of the worktree at `976ea61`.
Duplicates across areas are merged into one group (V-n); each Info bundle item that claims a defect has its own row.

One finding is High (V-1); its fix and a test are in its section. While tracing I found one further issue the reviewers missed
(V-2, Medium: in a share, the share's `/.pdignore` can negate the user's own local-only rules). Nothing Critical.

| Group | Members | Title | Verdict | Final rating |
|---|---|---|---|---|
| V-1 | C1-1 | A gone share's local-only (`.pdignore`d) files are neither counted nor exported and are deleted with `trees/<key>/`; any member with write access decides via the share's `/.pdignore` what is local-only | CONFIRMED | High |
| V-2 | verifier (same root cause as V-1) | The share's `/.pdignore` is applied after the user's own local rules, so a `!` rule there negates them and the user's local-only files upload into the other user's volume | CONFIRMED | Medium |
| V-3 | C1-2 | A share renamed to `<name> (<owner>)` of another share takes that shown name; the incumbent is renamed in place to `(<owner>, 2)` | CONFIRMED | Medium |
| V-4 | C1-3, C4 (preface note), C4-5 (adopted marker), C1-7 (device UUID) | Pending markers: read in every tree including other users' shares, no count cap, one notification per new marker revision; marker file name carries the device UUID; an adopted link is trashed without re-checking its name and folder | CONFIRMED | Medium (C1-3); Info (the rest) |
| V-5 | C2-1 | VP8X/ANMF WebP thumbnail: the inner VP8 frame (up to 16383 × 16383) is allocated and decoded before it is compared with the checked canvas; `set_memory_limit` covers only ICCP/EXIF/XMP | CONFIRMED | Medium |
| V-6 | C4-1 | `--clear-stale` / `unmount --if-stale` (and `preflight`'s advice) detach another profile's live mount at the same mountpoint; mounts do not say whose they are | CONFIRMED | Medium |
| V-7 | C5-1 | A swap move whose reply is lost is retried; `remote_moved_first` adopts `.pdmount-swap-<ino>` as the user's local name, silently | CONFIRMED | Medium |
| V-8 | C4-3, C5-5 | A marker `time` beyond `i64::MAX` panics the marker task (`UNIX_EPOCH + Duration`); no long-lived engine task or volume poller is restarted after a panic | CONFIRMED | Low |
| V-9 | C1-4 | Folder creation and block-upload requests in a share use the default address, not the membership address | CONFIRMED | Low |
| V-10 | C1-5, C4-5 (`is_hidden` gaps) | Share names and owners cleaned of control characters only; `dir = %…` log fields unescaped; `is_hidden` misses some invisible characters | CONFIRMED | Low |
| V-11 | C1-6 | `ShareTree::poll_events` uses `get_nodes`, so a degraded root on a refused stream is reported gone (against D119) | PLAUSIBLE | Low |
| V-12 | C2-2 | Each photo insert or name change renames its whole month: quadratic load time under the model's write lock | CONFIRMED | Low |
| V-13 | C2-3 | Thumbnail cache unbounded, survives `logout`/`reset`, keeps a looser existing folder mode | CONFIRMED | Low |
| V-14 | C3-1 | `login --force` (and the daemon's `adopt`) replaces a session without ending the old one on the server | CONFIRMED | Low |
| V-15 | C3-2 | The sign-in link goes into `xdg-open`'s (and possibly the browser's) argv | CONFIRMED | Low |
| V-16 | C4-2 | Unicast forged `PrepareForShutdown` signals fill zbus's 64-slot queue; each costs a 10 s `GetNameOwner`; the real signal and `Release` wait | CONFIRMED | Low |
| V-17 | C4-4 | Stop signals between the FUSE mount and `StopSignals::new()` kill the daemon with the default action | CONFIRMED | Low |
| V-18 | C5-2, C5-6 (inode numbers in swap names) | `.pdmount-swap-<ino>` is not unique; an `AlreadyExists` on the swap move renames the user's node to a conflict name, or stalls `move_aside` | CONFIRMED | Low |
| V-19 | C5-3 | A moved-aside folder that is kept comes back under the hidden swap name for every device and member | CONFIRMED | Low |
| V-20 | C5-4, C1-7 (per-share memory) | Every further tree populates a ~20 MiB pool at open; 32 shares ≈ 640 MiB, set by other users | CONFIRMED | Low |
| V-21 | C3-4 (`Retry-After`), S-10 | `now + wait` with a server `Retry-After` near `u64::MAX` can panic the waiting daemon's keyring/backend wait | PLAUSIBLE | Info |
| V-22 | C1-7 (export modes) | `orphaned/` folders with umask modes; exported files take the node's (owner-settable) mode | CONFIRMED | Info |
| V-23 | C1-7 (listing work) | `shared_listing` pages and `get_share`/root lookups before the 32-share cap, without a gate turn; `share_info` never pruned | CONFIRMED | Info |
| V-24 | C1-7 (slots), C5-6 (slots) | Tree slots are never reused (65 534 per mount) | CONFIRMED | Info |
| V-25 | C1-7 (revoked while unmounted) | A share revoked while the Shared mount is not mounted keeps `trees/<key>/` without an `Orphaned:` line | PLAUSIBLE | Info |
| V-26 | C2-4 | Thumbnail binding to the photo comes from the content key, not the manifest | CONFIRMED (observation) | Info |
| V-27 | C2-5 | Thumbnail hardening: `is_valid` follows symlinks/blocks on a FIFO; decrypt up to 50 MB on a worker; inode maps never shrink; `opendir` builds views in the FUSE thread; daemon's `XDG_CACHE_HOME` | CONFIRMED | Info |
| V-28 | C3-3 | Peer-uid check before a keyring call can be 2.5–5 s (timed-out write) or 30 s (own-bus name cache) old, not "a few milliseconds" | CONFIRMED | Info |
| V-29 | C3-4 (others) | `Wait::Backend` reason printed unescaped; `PDMOUNT_KEYRING_TIMEOUT_SECS` overflow panics; Secret Service session is `Plain` | CONFIRMED | Info |
| V-30 | C4-5 (OpenRC quoting) | `eval_quote` for `command`/logs collapses runs of spaces; `$RC_SVCNAME` reaches `command_args`' `eval` | CONFIRMED (generator); supervise-daemon side not re-read | Info |
| V-31 | C4-5 (readiness pipe) | The readiness descriptor is inherited by children spawned before readiness | CONFIRMED | Info |
| V-32 | C4-5 (unit files, third signal) | Unit files 0644 naming the binary; no check that the binary's directory is private; a third Ctrl-C is ignored | CONFIRMED | Info |
| V-33 | C5-6 (reset, raises) | `reset` deletes an unreadable further tree's state without the warning My Files gets; a dropped scan flight leaves its `raises` entry | CONFIRMED (reset); PLAUSIBLE (raises) | Info |

**Verifier's overall assessment.** The M5 surfaces hold where the reviewers say they do: the write gate, the router and slots, the
local paths built from remote input (hash keys, validated names, `orphaned/` names), the thumbnail verification chain, the bus
peer-uid checks, the systemd quoting and the read integrity after the cache split. The weak spot is the step from "remote input of
my own account" to "remote input of other users" that shares bring: features designed when every remote writer was the user's own
device (`/.pdignore`, pending markers, display names) now take another user's input with the same trust. V-1 is the one High: the
share's owner can make the member's writes local-only and then delete them by revoking, and the same loss happens without malice
for files the user's own local rules keep local. V-2 is its mirror image (another user un-ignores what the user keeps local). Both
are closed by one decision: a tree on another user's volume does not take its rules from that volume, and a gone tree exports every
staged inode, not only the journal's. The Mediums are real and each needs a specific precondition (an accepted share from the
attacker for V-3/V-4, a crafted thumbnail from a client of the account for V-5, a second profile at the default mountpoint for V-6,
a lost reply during a rename cycle for V-7). C5-5's "no panic reachable from remote input" does not hold: V-8 is one, in the marker
task, reachable from any share member. Nothing found reaches A1, nor writes outside PdMount's directories (the thumbnail cache is
the designed exception, C2's threat-model addition, which I support adding together with a line for shares as a new T2 sub-actor).

## V-1: A gone share's local-only files are deleted with its state (C1-1)

CONFIRMED, High.

Trace. `SharedRoot::remove` (`crates/pdmount/src/roots/shared.rs:724-781`) counts `queued = counts.pending + counts.parked` from
`engine.queue_counts()` (`:748-749`), exports only when `queued > 0` (`:750-753`), and otherwise calls `purge_tree` (`:773`), which
`remove_dir_all`s the parent of `tree_state_db(key)` (`crates/pdmount/src/roots.rs:987-1004`), i.e. `trees/<key>/` with its
`staging/` (`crates/pdmount-core/src/paths.rs:234-241`). With an export, `retire_tree` keeps the state only when an entry has an
error (`shared.rs:767-769`, `roots.rs:1172-1190`, `OrphanManifest::incomplete` in `engine/orphan.rs`). `export_queued` iterates
`self.journal.inos()` only (`crates/pdmount-core/src/engine/orphan.rs:103-151`, `:106`). A local-only node never stays in the
journal: `plan` returns `Plan::Done` for `local_only && remote.is_none()` and for a node whose parent is such a folder
(`crates/pdmount-core/src/journal.rs:1369-1374`, `in_local_only_folder` at `:1227-1232`), and `run_journal` turns `Done` into
`journal.done`, which removes the entry (`:1317`, `:686-703`). Local-only content lives only in the staging file (DESIGN §7.7).

Rule source. Every further tree gets the profile's local file (`roots.rs:1044`) and the tree root's own `/.pdignore`, downloaded from
the drive (`crates/pdmount-core/src/engine/ignore.rs:90-111`); `create`, `mkdir` and `rename` take the flag from these rules
(`engine/namespace.rs:156`, `:188`, `:264`; `reclassify` keeps a renamed never-uploaded node local-only when the new path also
matches, `ignore.rs:35-72`, so rsync's temp-then-rename and editors' atomic saves stay local-only). A `*` line matches every path
(`pdignore.rs:82-125`: an unanchored pattern also gets `**/<pattern>`). The share's root is the tree root, so its owner or any
editor writes the rule file. Trigger for the removal: revocation, leaving the listing, or the first "root not found"
(`drop_gone`, `shared.rs:788-805`).

Rating. The deleted content is the user's own writes into the mount, deleted by another user's action, with no export,
notification or log line naming them: "silent … loss of A4 triggered by untrusted input" (High). The non-malicious variant (the
user's own local rules matching in a share whose owner revokes it) makes it more likely, not less. Note that `reset` already
counts staged inodes (`crates/pdmount-core/src/engine/open.rs:8-18`, `pending_changes` adds `staged_inos()`), so the share path
is inconsistent with the protection My Files has.

**Fix (minimal).** In `remove`, count staged inodes as `pending_changes` does and treat any as "queued"; in `export_queued`, iterate
the union of `journal.inos()` and the staging directory's inodes (`StagingDir::staged_inos`), exporting a staged attached node's
content whatever its journal state (change `local-only`); purge only when every staged inode was exported, else `retire_tree` with
`keep`. Separately (closes the malicious half and V-2): for `!spec.own` trees, build the matcher from the profile's local file only
(do not read the tree root's `/.pdignore`), and say so in DESIGN §7.7/§14.5.
**Test.** Fake scenario with `TARGET=share`: the owner writes `/.pdignore` with `*` into the share (via the fake), the member creates
`a.txt` in the mount, then `pdmount fake share-role --role none`; assert `orphaned/*/files/a.txt` exists with the content and the
manifest lists it. A unit test on `export_queued` with one local-only staged file; and with the second fix, a unit test that a
non-own tree's `is_ignored` ignores a root `/.pdignore`.

## V-2: The share's `/.pdignore` negates the user's own local rules (verifier)

CONFIRMED, Medium. Missed by the area reports; same mechanism as V-1, opposite direction.

`keep_ignore_rules` compiles `IgnoreMatcher::new([self.local_ignore.as_str(), text.as_str()])` (`engine/ignore.rs:109`): the local
file first, the tree root's remote `/.pdignore` second. `is_ignored` lets the last matching rule win, including negations
(`crates/pdmount-core/src/pdignore.rs:47-56`, `!` parsed at `:88-90`). So in a share, a `!*` line (unanchored, so also `**/*`) in the
share's `/.pdignore` un-ignores every path the user's `~/.config/pdmount/<profile>.pdignore` keeps local (`*.env`, `*.key`,
`secrets/`), and `is_ignored` (`engine/ignore.rs:18-31`) returns false for them; they are queued and uploaded into the owner's
volume. The threat model names exactly this as an A4 confidentiality concern ("nothing uploaded that … e.g. local-only `.pdignore`
files"), here toward another user and triggered by that user's remote input. Not High: the user must have put the files into
the share's folder, and the local rules are a convenience against the user's own tools rather than a security boundary (by
analogy with access rules in the threat model's Out of scope). In My Files the same order only lets the user's own devices
override (S-19, known). Fix: V-1's second part (no remote rules for `!spec.own`); at least apply the local file last for those
trees, or ignore negations from a non-own tree's file. Test: a share with `!*` in `/.pdignore`, a local rule `*.env`, create
`x.env` in the share; assert `user.pdmount.state` is `local-only` and nothing is uploaded.

## V-3: Share display-name takeover (C1-2)

CONFIRMED, Medium. `display_names` (`crates/pdmount/src/roots/shared.rs:109-152`) first inserts every non-clashing name into `taken`
(`:128-134`), then suffixes the clashing ones, moving to `(<owner>, n)` when the suffixed form is taken (`:135-148`). A share that
another user renames to `Team (alice@…)` does not clash, so it is taken first; Alice's clashing `Team` then becomes
`Team (alice@…, 2)`. `refresh` applies the changed name in place (`:508-513`, `self.set.rename`). New path lookups resolve to the
attacker's tree; open handles and cwds stay on the old inode. `owner_label` (`:68-83`) is the creator address, which the attacker
must know or guess. Rating: data the user writes by path goes into a volume another user chose (A4 "no data written to the wrong
file or path"), needs an accepted editor share from the attacker and tools that write by path; no loss. Medium stands. The
reporter's fix (sticky names; a suffix-shaped name always counts as clashing; log and notify on a shown-name change) is right.

## V-4: Pending markers in shares (C1-3, with C4's notes and C1-7's device UUID)

CONFIRMED, Medium (C1-3); the merged Info items CONFIRMED, Info.

Reading is on in every tree (`crates/pdmount-core/src/engine/marker.rs:7-10`; `run_markers` spawned for every engine,
`engine/background.rs:105`); `scan_markers` fetches every canonical `<uuid>.pending` child of `.pdmount` with a new revision, with
no count cap (`marker.rs:520-605`, one gate turn and two backend calls each in `fetch_marker`, `:671-700`); `replace_seen` sends one
notice per new marker revision (`:624-668`); `announce_tree` subscribes every further tree (`crates/pdmount/src/roots.rs:1560-1561`)
and `incoming::announce` calls `notify::desktop` per notice, each a thread and a `notify-send` child (`crates/pdmount/src/incoming.rs:12-24`,
`notify.rs:107-137`). Correction: the notice channel holds 16 (`marker.rs:283`), so a burst sent faster than the announce task
reads it loses all but the last 16 (`Lagged` is ignored); with the multi-thread runtime the receiver can also keep pace, so the bound
is not firm. The fetch cost equals the attacker's upload cost (one fetch per uploaded revision), but it is paid from the session's
background gate shared with My Files. Rating: remote-triggered resource use and notification spam with another user's text
(64 cleaned characters) in the user's session; Medium stands, at its lower edge. Fix as reported; in addition, do not read markers
in `!spec.own` trees at all (D114 says a member's marker is for that member's devices).

- C4-5 (adopted marker): `adopt_own_marker` takes any link named `<this device id>.pending` (`marker.rs:609-618`), and
  `trash_own_marker` trashes it after only a liveness check (`:789-811`). The id is public to every member (V-4's next item), so a
  member can have another file adopted and trashed under the user's account; the member could trash it directly. Info.
- C1-7 (device UUID): `marker_name(&self.device.id)` names the file in shares too (`marker.rs:355`); only the JSON's device name is
  dropped (`set_marker_anonymous`, `roots.rs:1076`). Correlation of a member's devices across shares. Info.

## V-5: WebP inner frame decoded before the canvas check (C2-1)

CONFIRMED, Medium. `decode` (`crates/pdmount-core/src/photos/thumbnails.rs:195-225`) checks the length, `dimensions()` and
`output_buffer_size()`, sets `set_memory_limit(len * 2)` and calls `read_image`. In `image-webp` 0.2.4: for `VP8X`, `dimensions()`
is the canvas from the extended header (`src/decoder.rs:427-431`); a still `VP8X` image calls `Vp8Decoder::decode_frame` and only
afterwards compares the frame with the canvas (`src/decoder.rs:695-702`); an animation's first frame checks the `ANMF` rectangle
against the canvas but the inner VP8 header only after `decode_frame` (`src/decoder.rs:780-808`). `read_frame_header`
(`src/vp8.rs:1170-1215`) allocates `ybuf` (`mbw·16 × mbh·16`), `ubuf` and `vbuf` from the frame's own 14-bit width and height before
any other check: 268 MB + 2 × 67 MB for 16383². `memory_limit` is consulted only by the ICCP, EXIF and XMP chunk reads
(`src/decoder.rs:644`, `:649`, `:654`; the setter's own comment, `:551-556`, says "Some allocations currently ignore this limit").
The arithmetic decoder fails at end of data (`src/vp8_arithmetic_decoder.rs:128-143`), so the macroblock loop
(`src/vp8.rs:1950-1980`) needs real mode bits for each of about a million macroblocks; with `prob_skip_false = 1` and DC modes that
is a few bits each, within the 19-bit first-partition size and the 1 MiB thumbnail cap (estimate, as the reviewer says; even a
frame that errors halfway has touched half the planes). `vec!` failure aborts (outside `catch_unwind`, `thumbnails.rs:401`).
Actor: a client holding a session of the account (T2 for Photos; shared albums do not reach the tree). Memory exhaustion from
remote input: Medium. DESIGN §14.5 (`DESIGN.md:2323`) and D115 (`docs/decisions.md:3333`) claim a decoder memory limit that does
not cover this. The reporter's fix (parse every `VP8 ` frame header before `WebPDecoder::new`, refuse frames larger than
`MAX_SIDE` or, for a still `VP8X`, different from the canvas; or refuse animations) and the unit test are right.

## V-6: Stale-mount rule detaches another profile's live mount (C4-1)

CONFIRMED, Medium. `stale_action` (`crates/pdmount/src/daemon/launch.rs:185-196`) calls a mount stale when it is `fuse.pdmount`
(`crates/pdmount/src/mounts.rs:16-18`, type only) and *this profile's* daemon neither answers nor runs; `detach_if_stale`
(`launch.rs:213-240`) then runs `fusermount3 -uz`, for the main mountpoint and the further roots' (`config.root_mountpoints`). Every
mount has `fsname=pdmount, subtype=pdmount` (`crates/pdmount-fuse/src/mount.rs:137-138`, `readonly.rs:54-55`), and every profile
defaults to `~/ProtonDrive` (`crates/pdmount-core/src/config.rs:417`). `--clear-stale` runs before `preflight`
(`crates/pdmount/src/daemon/run.rs:51-54`), and the systemd unit's `ExecStopPost=… unmount --if-stale` (`service.rs:405-422`) runs
the same rule. The second profile's service start therefore lazily detaches the first profile's live mount and mounts its own
account there. Actor T5 (a default configuration, no attacker), consequence: writes by path go to the other account and the first
daemon stops without a flush. Medium stands (data to the wrong account from a default setup; not untrusted input, so not High).
Fix as reported (profile in `fsname`, pid-file check across profiles, refuse a mountpoint another profile's pid file names).

## V-7: Swap move adopted as the user's name after a lost reply (C5-1)

CONFIRMED, Medium. The planner sends `Work::Move` to `swap_name(ino)` while the node's local name is unchanged
(`crates/pdmount-core/src/journal.rs:1376-1391`, `swap_name` at `:1191-1193`). If the rename is applied but the reply is lost, the
node's stored remote name stays the old one (the update after `Ok` at `:1691-1707` does not run) and the retry plans the same move.
`move_node` then finds the fresh node off the known placement and calls `remote_moved_first` (`:1626-1629`; the `NameConflict`
branch of S-25 at `:1650-1661` does the same). `remote_moved_first` (`:1717-1748`) calls `take_remote_placement` *before* its
"same move" test, and `take_remote_placement` (`:1780-1798`) runs `Tree::adopt_remote_placement`
(`crates/pdmount-core/src/vfs.rs:1893-1914`), which unplaces the inode and places it at `.pdmount-swap-<ino>` (a free, representable
name in a listed folder). The "same move" branch then records no conflict. The node is "done" at the swap name, the other node takes
the freed name, and the user's file sits under a hidden name on every device. This holds when the retry runs before the event pass
applies the echo (back-off of a transient failure is shorter than the poll interval). A4 integrity, no content lost, needs a lost
reply (T3; an on-path attacker can drop the response of a request it cannot read) during a rename cycle: Medium stands. Fix: in
`move_node`/`remote_moved_first`, when the fresh placement equals the planned target and that target is not the node's local
placement, record it as the remote state only (as `move_aside` does) and replan; test with a fake fault "apply, then fail".

## V-8: Remote marker `time` panics the marker task; nothing restarts tasks (C4-3, C5-5)

CONFIRMED, Low. `parse` takes any `u64` for `time` (`marker.rs:140-157`); `IncomingMarker::age` computes
`UNIX_EPOCH + Duration::from_secs(self.time)` (`:105-108`), and std's `SystemTime + Duration` panics on overflow (any value above
`i64::MAX` seconds). `replace_seen` calls `is_stale` for each new marker revision (`:640`) in `run_markers` (`:505-515`), a task
spawned once (`engine/background.rs:105`); the panic ends it, and since the store write of `notified` (`:652-656`) comes after the
loop, the next start panics again while the file exists. A share member or any device of the account can do it with a 39-byte
file. Effect: no other device's marker is read, shown or announced for that tree. `incoming::status_lines` would panic the same way
but is never reached, because `seen` is replaced after the loop. This refutes C5-5's "no remote-triggered panic was found"; C5-5's
point (no supervisor for the pollers and engine tasks, `poller.rs:394`, `:467`, `engine/background.rs:71-125`; only the Photos
stages restart) is confirmed and is why this panic is permanent. Low: a minor feature stops, nothing else. Fix: refuse `time >
i64::MAX` (or far future) in `parse`, `checked_add` in `age`; supervise long-lived tasks as D119 does for Photos.

## V-9: Default address in share folder creation and block requests (C1-4)

CONFIRMED, Low. `FolderOperations::create` takes the signing key of `get_default_address()` (`sdk/crates/proton-drive-sdk/src/node/folder.rs:36-45`)
and names it as `signature_email_address` (`:85-88`); `BlockUploadPreparationRequest.address_id` is the default address
(`sdk/crates/proton-drive-sdk/src/block/upload.rs:108-110`, `:198-200`); `membership_address_for` falls back to the default on any
lookup error with a warn (`sdk/crates/proton-drive-sdk/src/node/draft.rs:148-158`). The official client uses the context share's
member address (`cs/client/js/src/internal/nodes/nodesAccess.ts:460-487`). Disclosure of the primary address to a share joined on an
alias; content signatures still use the membership key. Low; the server's reaction to the mismatch is untested.

## V-10: Share names, owners and log fields (C1-5, C4-5's `is_hidden` gaps)

CONFIRMED, Low. `printable` filters `char::is_control` only (`crates/pdmount/src/status.rs:100-104`), `owner_label` control
characters and `/` (`roots/shared.rs:68-83`); bidirectional and zero-width characters pass. In `tracing-subscriber` 0.3.23 a named
field is written with `{:?}` of the value (`src/fmt/format/mod.rs:1300-1336`); for a `%` field that value is `format_args!("{}")`,
whose Debug is its Display, so `dir = %dir.display()` (`roots.rs:1142-1158`) writes a share name's newline and ESC raw; only the
`message` field passes through `EscapeGuard` (and that escapes ANSI and C1, not newlines). Log injection from T2. The `is_hidden` gaps
of C4-5 (`marker.rs:176-189`: tag characters, variation selectors, U+034F, Hangul fillers, U+180B–U+180D) are confirmed; they allow
display spoofing only. Fix: one shared cleaning function (widened `is_hidden`) for names, owners and device names; `?` for path
fields.

## V-11: Degraded share root reported gone from `poll_events` (C1-6)

PLAUSIBLE, Low. `ShareTree::poll_events` (`crates/pdmount-backend/src/shared.rs:425-442`) calls `get_nodes`, whose default maps
`Degraded` to `None` (`crates/pdmount-backend/src/lib.rs:98-105`, `:300-316`), then `gone_now`. The code path is real; whether the
server refuses the stream while returning an undecryptable root was not traced. Combined with V-1 the effect would be loss of
local-only files. Use `get_nodes_checked` and act only on `Absent`.

## V-12: Month-wide renaming per insert (C2-2)

CONFIRMED, Low. `place` calls `rename(month)` on every call (`crates/pdmount-core/src/photos/model.rs:273-298`); `rename` collects,
sorts and re-runs `unique_name` over the whole month (`:387-412`); `unique_name` probes `(2)`, `(3)`, … linearly (`:531-549`). Batches
apply under one write lock (`photos/sync.rs`). Quadratic (cubic with equal names) load and stalls; no data effect. Fix as reported.

## V-13: Thumbnail cache bounds and lifetime (C2-3)

CONFIRMED, Low. `write_atomic` makes missing folders 0700 but leaves existing ones (`thumbnails.rs:265-283`); nothing outside
`crates/pdmount-core/src/photos` uses `thumbnails_dir` except the Photos root's options (`roots.rs:949`), so `logout`/`reset` never
remove the files, and no size cap or free-space check exists. Fix as reported; mention in the user guide.

## V-14: `login --force` leaves the old session open (C3-1)

CONFIRMED, Low. `adopt` stores the new session and overwrites `*self.logged_in` (`crates/pdmount/src/auth.rs:341-374`) without
`end_from_session` on the old one; `store_login` ends only a session it could not store (`session.rs:159-176`). A1 hygiene when the
user replaces a session they suspect leaked. Fix as reported.

## V-15: Sign-in link in `xdg-open`'s argv (C3-2)

CONFIRMED, Low. `open_in_browser` passes the URL as an argument (`crates/pdmount/src/session.rs:93-101`), readable by other local users
in `/proc/<pid>/cmdline` for `xdg-open`'s lifetime and, when it starts the browser, the browser's. Predates M5. S-2 was Medium
because of the persisted world-readable log (S-3); here the exposure is transient and the selector stays in memory, so Low stands.

## V-16: Unicast forged `PrepareForShutdown` stalls the logind task (C4-2)

CONFIRMED, Low. In zbus 5.19.0, `MatchRule::matches` does not filter by a well-known sender (`src/match_rule/mod.rs:219-228`); the
socket reader awaits `broadcast_direct` into the stream's queue (`src/connection/socket_reader.rs:76-108`; 64 slots,
`src/connection/mod.rs:47`, `:993-996`), so with the queue full it reads nothing, method replies included. Each mismatched message
makes `run` await `GetNameOwner` (10 s limit) inside the `select!` arm (`crates/pdmount/src/logind.rs:129-139`, `:177-190`), so
`Release`/`Take` and the real signal wait. SIGTERM still starts the flush; the sender check holds. Low stands; fix as reported
(drop messages with a destination; no bus call per mismatch).

## V-17: Signals before the handlers are installed (C4-4)

CONFIRMED, Low. The mount happens at `crates/pdmount/src/daemon/run.rs:168`, `StopSignals::new()` at `:277`; no earlier handler is
installed in the daemon path. A stop in that window is a crash (journal-safe), without the D075/D092 guarantees. Fix as reported.

## V-18: Swap name not unique (C5-2, with C5-6's inode-number note)

CONFIRMED, Low. `swap_name` is `.pdmount-swap-<ino>` (`journal.rs:1191-1193`); the planner does not check that the name is free
remotely; `AlreadyExists` on the swap move goes to `yield_to_remote` (`:1666-1680`), which renames the user's node locally to a
conflict name, and in `move_aside` it is a failed step retried with back-off. The name also shows the device's inode numbers to
share members (Info). Fix: per-device unique temporary name, retry with another on `AlreadyExists`, never `yield_to_remote` for a
swap move.

## V-19: Kept folder under the swap name (C5-3)

CONFIRMED, Low. `keep_remote` (`journal.rs:2076-2110`) unbinds the tombstone and lists the parent, so the folder comes back at its
remote name, which after `move_aside` is the swap name; the conflict note names the old local name. Fix as reported (rename to a
visible conflict name first).

## V-20: Per-tree pool memory (C5-4, C1-7's per-share memory)

CONFIRMED, Low. Further trees get `ram_bytes: MIN_RAM_BYTES` (16 MiB, `roots.rs:1050`, `pool.rs:128`), and the pool maps with
`MAP_POPULATE` (`crates/pdmount-core/src/pool.rs:5`, `:75`) at open, whether the share is read or not; `MAX_SHARES` is 32
(`roots/shared.rs:45`). About 640 MiB resident, bounded. C1-7's point (a browsed folder holds all its children in RAM) is the same as
My Files and stays Info. Fix: lazy or shared pool for further trees.

## V-21: `Retry-After` overflow in the keyring/backend wait (C3-4, S-10)

PLAUSIBLE, Info. `classify` computes `until: now + wait` with `wait = backoff.max(retry_after)`
(`crates/pdmount/src/keywait.rs:456-460`); the fork's `parse_retry_after` accepts any `u64` (`sdk/crates/proton-drive-sdk/src/error.rs:191-194`).
But the fork's retry middleware sleeps a 429's `Retry-After` before returning (`sdk/crates/proton-drive-sdk/src/http/retry.rs:113-121`,
`:247-262`), and `tokio::time::sleep` clamps a huge duration to its far future, so that path hangs rather than panics; the panic
needs a `RateLimited` error carrying the value without that sleep (the circuit breaker, `:236-238`, was not traced). Server actor,
out of scope; it is the S-10 gap. Clamp with `checked_add` or a 1 h cap when S-10 is fixed.

## V-22 to V-33: Info items

- **V-22** (C1-7 export modes): `export_file` sets `mode & 0o777` of the node (`engine/orphan.rs:155-177`, `:175`) and
  `create_dir_all` uses the umask; the data directory's 0700 is the only protection. CONFIRMED.
- **V-23** (C1-7 listing work): `MAX_SHARED_PAGES` 64 (`crates/pdmount-backend/src/proton.rs:310`), `share_info` cached and never
  pruned (`:298`, `:1421-1440`), work before `select`'s cap. CONFIRMED.
- **V-24** (C1-7, C5-6 slots): slots are never reused (`crates/pdmount-fuse/src/trees.rs:105-115`). CONFIRMED; safe direction.
- **V-25** (C1-7 revoked while unmounted): plausible from the code (only an open Shared mount follows the listing); safe direction
  (state kept). PLAUSIBLE.
- **V-26** (C2-4): `thumbnail_ref` verifies the listing manifest with the unsigned `signature_email_address`'s keys and does not
  compare the revision id (`proton.rs:1586-1631`); the content-key decryption binds the thumbnail (`:1715-1735`). Server out of scope.
  CONFIRMED as an observation.
- **V-27** (C2-5): `is_valid` uses `File::open` (`thumbnails.rs:160-186`), which follows a symlink and blocks on a FIFO (same uid);
  `content_key.decrypt` with `proton-rpgp`'s default 50 MB `max_reading_size` (`proton-rpgp` 0.3.1 `src/profile/settings.rs:47`)
  before the 1 MiB check (`proton.rs:1725-1733`); the other three items are as described. CONFIRMED.
- **V-28** (C3-3): `checked_write` sleeps 500 ms and writes again after a `NoReply` (`crates/pdmount/src/credentials.rs:283-333`),
  on fresh connections from the helper. CONFIRMED; the threat model's "few milliseconds" should be corrected. libdbus's `guid=` check
  keeps it Info.
- **V-29** (C3-4 others): `reason: format!("{error:#}")` (`keywait.rs:457-459`) printed by `status`; `PDMOUNT_KEYRING_TIMEOUT_SECS`
  read in shipped builds (`credentials.rs:156-162`) and added to `Instant::now()` (`:838`); `keyring` built with `linux-native` and
  `sync-secret-service` only (`Cargo.toml:36`). CONFIRMED.
- **V-30** (C4-5 OpenRC quoting): `command`, `output_log`, `error_log` use `eval_quote` (`crates/pdmount/src/service.rs:189-195`,
  `:259`, `:318-330`) while `command_args` words use the backslash form; `profile=${RC_SVCNAME#pdmount.}` reaches `command_args`
  (`:256-264`) and `start_pre` accepts any `pdmount.?*` (`:274-281`). Same uid only. CONFIRMED for the generator;
  `supervise-daemon.sh`'s expansion was not re-read.
- **V-31** (C4-5 readiness pipe): `notify_fd_ready` writes and closes the inherited descriptor by number (`supervise.rs:180-209`);
  nothing marks it close-on-exec before children are spawned. CONFIRMED.
- **V-32** (C4-5 unit files, third signal): as described. CONFIRMED.
- **V-33** (C5-6): `trees_with_changes` falls back silently to counting staged files (`crates/pdmount/src/daemon/reset.rs:125-140`),
  unlike My Files' warning (`:22-31`). CONFIRMED. The `raises` entry (`crates/pdmount-core/src/cache/block_cache.rs:490-506`) leaks
  only if a flight's future is dropped before `class` resolves; whether the flight runner ever drops it was not traced. PLAUSIBLE.
