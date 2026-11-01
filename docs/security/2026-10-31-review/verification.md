# Verification of the M4.16 area reports (Fable verifier, 2026-10-31)

Reviewed tree: worktree `4903f03` (`m4-start..4903f03`), SDK fork `pdmount` at `2ae4aa3` (checkout
`~/.cargo/git/checkouts/proton-sdk-rs2-<hash>/2ae4aa3`). Threat model: `docs/security/threat-model.md` (D070).
Area reports: `area1.md` … `area3.md` in this directory (finding IDs B1-n … B3-n; B1-4's six hardening items are verified one by
one). Read-only: every code path below was traced in the source; nothing was run. Line numbers are of the worktree at `4903f03`.

No finding is High or Critical, so no M4.17.n fix is required by this review. Nothing High or Critical was noticed while tracing.

| Group | Members | Title | Verdict | Final rating |
|---|---|---|---|---|
| R-1 | B1-1, B1-4 (textfile mode) | Prometheus textfile's temporary file (`.<name>.tmp`, fixed name, `std::fs::write`) follows a planted symlink in a shared-writable collector directory | CONFIRMED | Medium |
| R-2 | B1-2 | Autostart entry without `--no-login` runs the web login in a process whose stdout is the session log; the sign-in link goes there | CONFIRMED | Low |
| R-3 | B1-3 | Key passphrases written by a refused or unstorable web login stay in `sdk-keys.db` until `logout` | CONFIRMED | Low |
| R-4 | B1-4 (`.log.1`) | Rotated log keeps its old mode; only the current log is set to 0600 | CONFIRMED | Info |
| R-5 | B1-4 (autostart file) | Autostart entry written with the umask mode, names the binary path and the secret-storage flag; `current_exe()` persisted as resolved | CONFIRMED | Info |
| R-6 | B1-4 (`ATTACH`) | `ATTACH DATABASE ?1` on a connection opened with `SQLITE_OPEN_URI` | CONFIRMED | Info |
| R-7 | B1-4 (two-WAL move) | `move_passphrases` is one transaction over two WAL databases; a crash between the commits loses or duplicates a passphrase | CONFIRMED | Info |
| R-8 | B1-4 (`sync --timeout`) | `Duration::from_secs(u64::MAX) + SYNC_MARGIN` panics the CLI | CONFIRMED | Info |
| R-9 | B2-1 | Pin fetch checks the 1 GiB reserve once per file; a single remote file larger than the free space fills the disk with pinned, unevictable blocks | CONFIRMED | Medium |
| R-10 | B2-2 | A stale `Staged` whose `materialize` was awaiting a block write-through to the current staging's descriptor and sidecar of the same inode | CONFIRMED (code path) | Low |
| R-11 | B2-3 | `Journal::start` returns `true` for an entry `queue drop` forgot between `plan` and `start`; the trash or move runs anyway | CONFIRMED | Low |
| R-12 | B2-4 | `entry_path` recurses without a bound over tombstones' remote parent chain | PLAUSIBLE | Low |
| R-13 | B3-1 | `queue.sh` runs `queue drop --yes --all` and a mount-wide `sync` on the real account by default | CONFIRMED | Low |
| R-14 | B3-2 | `needs_login` decided by a substring of untyped error text | CONFIRMED (hardening) | Low |
| R-15 | B3-3 | A revision whose block listing fails verification is refetched (whole listing) on every read attempt | CONFIRMED | Low |
| R-16 | B3-4 | `scripts/bench/m4.sh` builds a `fake-backend` binary at `target/release/pdmount` | CONFIRMED | Info |

**Verifier's overall assessment.** The two Mediums are real but both need a precondition the user supplies: R-1 a collector directory
that another local user can write to (the examples in the config and the user guide point into `~`), R-9 a pinned folder (the
reserve then fails for one oversized remote file). Neither reaches A1–A3, neither corrupts A4. The staging LRU (D063), the key
store (D085), the control requests, the `-o ro` mount, the autostart quoting and the fork's P16–P18 hold as the area reports say.
R-10 deserves attention beyond its rating: in the `queue drop` variant the surviving sidecar names the same base revision as the
new staging, so after a crash in the window the upload is accepted and the blocks the dropped staging had written are uploaded as
zeros. The window is sub-millisecond and needs a second concurrent staging operation on the same file plus a kill -9, so the rating
stays Low; the fix (a retired flag or a generation on `Staged`) is cheap and should go into the M5 hardening list.

## R-1: Textfile temporary file follows a symlink (B1-1, with B1-4's textfile mode)

CONFIRMED, Medium. `crates/pdmount/src/stats.rs:218-228` builds `.<name>.tmp` next to the configured path and calls
`std::fs::write(&temp, text)` (`:226`, `O_CREAT|O_TRUNC|O_WRONLY`, no `O_EXCL`, no `O_NOFOLLOW`), then `rename` (`:227`). The path
is `[stats] prometheus_textfile`, any absolute path or `~/…` (`crates/pdmount-core/src/config.rs:340-343`, `:366-369`); the writer
runs every 15 s for the life of the daemon (`stats.rs:187-214`, spawned at `daemon.rs:395-397`). A symlink planted at the fixed
temporary name is followed and its target truncated and overwritten with the metrics text; the rename then moves the symlink over
`<name>.prom` and the next tick repeats.

Threat-model check. The attacker is T1 and needs write access to the directory. This host has `fs.protected_symlinks=1`, which only
protects symlinks in *sticky* world-writable directories; a group-writable collector directory without the sticky bit (the usual
shape of a shared node_exporter textfile directory) is not protected. DESIGN §5.8 (`DESIGN.md:468-471`) invites sharing a collector
directory between profiles, but every example points into the user's home (`config.rs:444`, `DESIGN.md:1836`), where no other uid
writes. So the precondition is a user-chosen shared directory; given one, the attack is reliable and repeats every 15 s. Content is
not attacker-chosen (metrics text), so this is file destruction, not arbitrary write: Medium, as reported, not Critical. The
umask-mode item of B1-4 (`stats.rs:226`) belongs here: the file holds counters and the escaped profile name only (`:179-184`), so its
mode is fine once the temporary file is created `O_EXCL|O_NOFOLLOW` under a random name. The reporter's fix (random name,
`create_new`, `O_NOFOLLOW`, refuse a directory writable by others) is right; `rename` itself is safe.

## R-2: Autostart entry runs the login with stdout in the session log (B1-2)

CONFIRMED, Low. The entry's `Exec` is `<binary> --profile <p> [secrets flag] mount` (`crates/pdmount/src/autostart.rs:47-51`), without
`--no-login`; `main.rs:779` turns the missing `--no-login` into `Some(browser mode)`, and `mount_background` then runs
`session::login` in the CLI process when `Credentials::load()` is `None` (`daemon.rs:175-188`). `present_link` writes the URL to
stdout unconditionally (`session.rs:113-115`); `should_open_browser(Auto, interactive=false, …)` is false (`:76-86`), and this path
sends no `notify-send` (the daemon's `link_received`, `auth.rs:302-322`, is not involved because no daemon runs yet). So the link,
with the fork key in its fragment, lands wherever the display manager sends autostart stdout, for 10 min, and the user sees
nothing. Exposure is bounded: the link alone does not yield the session (the selector stays in memory, and a sign-in by another
account is refused by `adopt`, `auth.rs:269-276`); the file mode of the X session log or the journal is the desktop's, usually
owner-only; and the precondition is autostart enabled for a profile without stored credentials. Low stands. Either fix (pass
`--no-login` in the entry, or refuse the interactive login when `!interactive()`) closes it; D071 already routes the headless case
through the daemon's 0600 socket.

## R-3: Passphrases of a refused login persist (B1-3)

CONFIRMED, Low. The fork's `begin_via_web` writes `account:passphrase:<key id>` for every key of the signed-in user before
returning (fork `crates/proton-sdk-rs2/src/session.rs:319-324`); the session's secret cache is the profile's (`session.rs:149-157`
→ `client_options`), so the entries go to `sdk-keys.db` through `SecretCache::set` (`secret_cache.rs:31-37`, `:42-44`). `adopt`
refuses another account and only ends the server session (`auth.rs:269-276` → `end_unstored`, `session.rs:176-180`); a failed
`store_login` does the same (`session.rs:160-173`). Nothing removes the entries: `clear` keeps passphrases by design (`:57-59`), the
key store has no cap (`session.rs:61`, `keys` opened with `None`), and no fork code calls `remove` for them. The value is the
refused account's key password sealed with this profile's cache key, 0600 in a 0700 directory. T4 hygiene of A1 material at rest;
Low. Fix as proposed (`remove` the ids after `end_unstored`).

## R-4: Rotated log keeps its mode (B1-4)

CONFIRMED, Info. `rotate_log` renames (`daemon.rs:131-143`) and `open_log` sets 0600 only on the file it opens (`:154-159`); the
directory is forced to 0700 in the same call (`:148-152`), so a 0644 `.log.1` is unreachable for other users. Chmod it anyway.

## R-5: Autostart entry mode and resolved binary path (B1-4)

CONFIRMED, Info. `enable_at` uses `create_dir_all` and `std::fs::write` (`autostart.rs:65-70`), so umask modes; the content is the
binary path, profile and `--allow-file-secrets`/`--force-file-secrets` (`:47-51`, `:90`). `current_exe()` (`:82`) is `/proc/self/exe`
resolved. Nothing secret; 0600 and a warning for a path under `target/` are cheap.

## R-6: `ATTACH` with URI parsing (B1-4)

CONFIRMED, Info. `Connection::open` (`secret_cache.rs:75`) uses rusqlite's default flags, which include `SQLITE_OPEN_URI`
(rusqlite `OpenFlags::default`), so the bound `ATTACH DATABASE ?1` (`:87`) would URI-parse a path starting with `file:`. The path
is `$XDG_DATA_HOME/pdmount/<p>/sdk-keys.db` (T5 only). Open without `SQLITE_OPEN_URI`.

## R-7: Two-WAL transaction (B1-4)

CONFIRMED, Info. The fork sets `journal_mode = WAL` (fork `crates/proton-drive-sdk/src/cache/sqlite.rs:239`); SQLite commits a
multi-database transaction atomically only when the main database is not in WAL mode, and commits `main` before the attached
database, so a crash between the two commits can lose the passphrase (one more login) or leave a duplicate (handled by
`INSERT OR IGNORE`, `secret_cache.rs:90-95`). Runs once per profile (the move is skipped when nothing matches, `:78-85`). Copy,
commit, then delete.

## R-8: `sync --timeout` overflow (B1-4)

CONFIRMED, Info. `Duration::from_secs(timeout_secs) + SYNC_MARGIN` (`crates/pdmount/src/queue.rs:165`, margin 60 s at `:16`)
panics on `u64::MAX`. CLI only (the daemon receives the raw seconds in `Request::Sync`); T5. `saturating_add`. Rated Info rather
than S-16's Low because only the CLI process panics.

## R-9: Pin fetch fills the disk with one oversized file (B2-1)

CONFIRMED, Medium. `pin_set` refuses a pin whose *known* missing bytes exceed the free space minus the reserve
(`crates/pdmount-core/src/engine/pins.rs:258-265`). After that, `pin_round` checks `free_disk_bytes() < FREE_RESERVE`
(`:25`, 1 GiB) once per file before `fetch_pinned` (`:511-523`), and `fetch_pinned` fetches block after block with no space check
(`:546-572`, one `cache.get` per block, paced only by `BLOCKS_PER_TURN`). Every `put` of a pinned revision lands as a pinned row
(`cache.rs:612-665`, `pinned` decided from `pinned_revs` at `:639-642`), pinned bytes are outside `max_bytes`
(`:432-436`, `enforce_cap` at `:667-673` counts `index.bytes` of unpinned entries only), `evict_to` walks `by_age`, which holds
unpinned entries only (`:364-366`, `:704-723`), and `wipe` filters `!entry.pinned` (`:748`). So a file added or replaced remotely
in a pinned folder (T2) that is larger than the free space is fetched until `put` fails with ENOSPC, and the blocks already on disk
stay until the user finds the path and runs `unpin`. Within the threat model this is local disk exhaustion from remote input
(Medium: DoS), gated on the user having pinned a folder (T5). Not High: nothing is written outside PdMount's directories and no
A4 data is lost. The reporter's fix (check the reserve inside `fetch_pinned` per turn, let a partial revision go on ENOSPC, refuse a
single file larger than the free space) is right.

## R-10: Stale `Staged` writes through the current staging (B2-2)

CONFIRMED (code path), Low. `Engine::materialize` takes the `Arc<StagingLock>` once (`engine.rs:2060-2066`), awaits
`read_revision` with no lock held (`:2077`), then locks and calls `Staged::materialize` without checking that the `Arc` is still the
map's entry (`:2080`); `with_staged` (`:1990-2011`) does make that check, `materialize` does not. `Staged::file()` resolves the
descriptor by inode through the shared `FdCache` on every call (`staging.rs:433-435`, `:75-84`), and `save()` writes the shared
`staging/<ino>.state` (`:667-685`). `StagingDir::create` for a new staging of the same inode truncates the same data file, replaces
the descriptor and (with a base) writes the sidecar (`:300-345`). Unstage sites that can run during the await: `queue drop`
(`engine/queue.rs:570-592`, which holds the old `Staged`'s lock while reverting and so only delays the stale writer), the clean
upload commit (`journal.rs:2287-2299`), `readopt_remote_content` (`:2128-2134`), `drop_detached` (`engine.rs:2200-2215`). The stale
`materialize` then passes its own base check (its own sidecar), writes the old base's block into the new staging's data file and
overwrites the new sidecar with its own.

Reachability. The stale writer resumes at once after the lock is free; for the new staging to exist by then, a second FUSE
operation that stages (write, truncate) must land on the same file inside that sub-millisecond gap: the first write's own
continuation creates the new staging only after `materialize` returns (`engine.rs:1263-1283`), and the delayed removal
(`remove_committed_staging`, `:274-296`) skips an inode that has a new staging. Single-writer tools (rsync, git) do not produce
two concurrent stagings. Without a crash the new staging's next `save` (its own materialize or any covering write,
`staging.rs:590-605`) repairs the sidecar within milliseconds, so there is no effect.

Data after a crash in that window. Upload-commit and readopt variants: the surviving sidecar names base R1 while the server holds
R2; the upload sends `base_rev` R1 (`journal.rs:1679-1690`) and the server rejects it (layer 3 of §10.4), so a visible conflict copy
results, as reported. `queue drop` variant: the node reverts to R1 and the new staging is created on R1 too, so the surviving
sidecar (R1, the old `materialized` set) matches the current revision, and the new data file holds zeros where the dropped staging
had written. An upload then would be accepted with those zero blocks: silent corruption of A4. The trigger is the user's own
`queue drop --yes` plus a concurrent second staging operation on the same file in a sub-millisecond window plus a kill -9 within
the next milliseconds, so it is not "triggered by untrusted input" and the probability is negligible: Low stands, but the fix is
cheap and should be done (M5 hardening): re-check `Arc::ptr_eq` under the staged lock in `Engine::materialize` and `setattr_slow`,
and make a retired `Staged` refuse `materialize`/`save`.

## R-11: `Journal::start` on a forgotten entry (B2-3)

CONFIRMED, Low. `run_journal` calls `plan(ino)` and then `start(ino)` for each ready entry (`journal.rs:846-876`); `start` returns
`true` for an entry that no longer exists (`:482-496`, `None => true`). `queue_drop` with `--yes` runs `hold` (succeeds while the
entry is not yet `in_flight`, `:664-678`), `undo_entry` → `unbind_tombstone` + `forget` (`engine/queue.rs:481-505`,
`journal.rs:695-703`) and `release`, all possible between `plan` and `start`. For a trash plan the link is pushed and `trash`
runs: `trash_check` reads `base_of(ino)` (`:1411-1425`), which is `None` when `unbind_tombstone` dropped the node
(`vfs.rs:1867-1880`, `is_droppable` at `:957-966`) and then returns `Ok(true)`: the remote file is trashed although the user was
told the delete is undone. For a surviving node (`nlookup > 0`) the base is `None` ≠ the active revision and the conflict path
keeps the file. `move_node` re-reads the tree but only checks the link (`:1146-1152`), so a dropped move is still sent. Uploads
are safe (`try_start_upload` needs the entry). The window is microseconds inside one executor turn; the outcome is recoverable
(trash, or a rename that the next event brings back). Low. `None => false` is the right fix.

## R-12: `entry_path` recursion (B2-4)

PLAUSIBLE, Low. `entry_path` recurses on `remote.parent_link` → `by_link` for detached nodes with no bound
(`engine/queue.rs:100-116`), while `entry_within` bounds the same walk to `tree.len()` and notes that cycles are possible
(`:119-140`). A cycle needs two tombstones whose stored remote parents name each other. I could not construct one through the
paths traced: listings skip `deleted` nodes when updating placement (`vfs.rs:1240-1245`), the "displaced" path rewrites a node's
remote only when the new parent is unknown to the tree, so `by_link` cannot reach it (`engine.rs:1786-1810`), and
`take_remote_placement` applies to attached nodes in a move conflict (`journal.rs:1308-1329`). A tombstone's remote parent is
therefore the folder it was under when deleted, and two nodes cannot be under each other at the same time. Not refuted either
(the tree's own comment allows cycles and I did not trace every `set_remote_at` site), so PLAUSIBLE. If reachable: stack overflow
aborts the daemon on a `queue`/`sync` command only (callers: `queue_entries`, `queue_items`, `queue_drop`, `sync_path`; not
`status` or the textfile). Low; add the bound as defence in depth.

## R-13: `queue.sh` on the real account (B3-1)

CONFIRMED, Low. `tests/scenarios/queue.sh:45` runs `pd queue drop --yes --all` and `:48` a bare `pd sync`; `QueueTarget::All`
covers every journal entry of the mount. `lib.sh:78` defaults `BACKEND=proton` with profile `default` and `MNT=~/ProtonDrive`
(`:90-93`); `run-all.sh:26` lists `queue` in the default set; `ops.sh:6` has the fake-only guard that `queue.sh` lacks;
`scripts/ci.sh:68` exports `BACKEND=fake` before running the scenarios, which is why CI is unaffected. So `tests/scenarios/queue.sh`
or `run-all.sh` without `BACKEND=fake` discards every pending local change of the real-account mount, including outside
`pdmount-test/` (D028). Actor T5 (the developer), target a test account: Low. Guard like `ops.sh` or drop by `"$area"`.

## R-14: `needs_login` by substring (B3-2)

CONFIRMED (hardening), Low. `map_error` ends with `error.chain().any(|c| c.to_string().contains("none could be unlocked"))`
(`crates/pdmount-backend/src/proton.rs:396-402`, constant at `:409`), matching the fork's untyped `anyhow::bail!`
(fork `crates/proton-sdk-rs2/src/account.rs:238-246`). The classification comes after every typed check, and the reporter's
search for remote-controlled text that can reach a cause's `Display` on PdMount's call paths found none that can carry the phrase
(the URL in a `reqwest` status error percent-encodes the uploader-chosen email). No in-scope path; a typed error in the fork is
the right fix. Low.

## R-15: Unverified block listing refetched per read (B3-3)

CONFIRMED, Low. `block_map` returns early on an error (`proton.rs:204-209`, the `?` before `*cached = Some(...)`), so a listing that
fails `verify_block_listing` (P17, fork `sdk/node/file/block_map.rs:356-375`) is paged again on every read attempt, about one
request per 10 blocks. The read fails correctly with `Integrity`; the cost is rate limiting of the mount while a reader retries.
Low; a negative cache for `BLOCK_MAP_TTL` is the fix.

## R-16: Bench binary at the release path (B3-4)

CONFIRMED, Info. `scripts/bench/m4.sh:55` builds `--release --features fake-backend` into the default target directory;
`scripts/ci.sh:64` writes the same path; `tests/scenarios/lib.sh:136-139` prefers it. Same-uid hygiene only. Use a separate target
directory.
