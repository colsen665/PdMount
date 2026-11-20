# Verification of the M6.0.3 area reports (verifier, 2026-11-18)

Reviewed tree: worktree at `9818aa1` (PdMount scope `976ea61..9818aa1`, plus the release pipeline, which no earlier review
covered). SDK fork: integration branch `pdmount-0.5` at `70f10ff` (cited `fork:<path>:line`), upstream `base/0.5.1` (cited
`sdk/…`, `rs2/…`), official SDK at tag `cs/v0.33.0` (cited `cs/<path>:line`). Threat model: `docs/security/threat-model.md`
(D070), its Ratings section. Area reports: `area1.md` … `area4.md` in this directory (finding IDs C1-n … C4-n). Method:
read-only; every High and Medium finding, and every Low, was traced in the source at these refs. Info bundle items were
checked at their cited lines; where an item was only read and not traced further, its row says so. Dependencies and tools
were read at the versions in use: `proton-rpgp` 0.3.1, tokio's `OnceCell`, cargo-dist 0.32.0 (its source: `src/config/v0.rs`,
`src/backend/ci/github.rs`, `src/build/cargo.rs`, `templates/ci/github/release.yml.j2`), and the official SDK's account
client (`cs/incubating/account/js/src/`). Nothing was built or run, no account was used, and no GitHub setting was looked
at. Line numbers are of the worktree at `9818aa1` and the fork at `70f10ff`. Duplicates across areas are merged into one
group (V-n, numbered anew for this review); findings of the M5.8 review are written "M5.8 V-n".

One finding is High (V-1); its fix and test are in its section. Three are Medium (V-2, V-3, V-4). V-2 is a secret in a
log, which the Ratings' letter puts at High; it is rated Medium for the reasons in its section, with a full fix plan so
that M6.0.4 can take it. Nothing Critical. No finding is rejected; corrections to the reports are in the sections. The
verifier adds no finding of its own, but four facts that change a fix or a rating (marked "verifier" below).

**What depends on GitHub settings.** The repository holds no settings, so these were not confirmed and need the user to
check them (list in "For the user: GitHub settings to check"): how far the release token reaches (V-4), which tags can
start a release (V-5), and whether Actions run on the fork (V-17).

| Group | Members | Title | Verdict | Final rating |
|---|---|---|---|---|
| V-1 | C4-1 | A gone share deletes the staged edits of an uploaded file that the user moved into a local-only folder (M5.8 V-1's fix is incomplete) | CONFIRMED | High |
| V-2 | C2-2 | The session-fork selector is in the error text of a failed sign-in poll, which the daemon logs at WARN, keeps for `status` and prints in `login` | CONFIRMED (the log); PLAUSIBLE (the takeover) | Medium |
| V-3 | C2-1 | One node whose parent cannot be decrypted fails the whole P13 batch; the event poller replays the page without limit | CONFIRMED (code); PLAUSIBLE (an attacker reaching it) | Medium |
| V-4 | C1-1 | Every build script and the dist installer of a release run get a token that can write to the repository and its Releases | CONFIRMED (workflow); reach needs the user to check settings | Medium |
| V-5 | C1-3 | The release starts on far more tags than `v*`, the tagged commit need not be on `master` or have passed CI, and the tag name is pasted into shell commands | CONFIRMED (workflow); needs the user to check settings | Low |
| V-6 | C1-2 | The actions of `release.yml` can be pinned by commit through dist's configuration; the threat model's acceptance rests on the opposite | CONFIRMED | Low |
| V-7 | C1-4 | The release build runs with neither `--locked` nor the pinned Rust toolchain | CONFIRMED | Low |
| V-8 | C3-1 | Signatures and keys are dated by the server clock but verified against the device clock | CONFIRMED (code); the EIO window needs a cache miss | Low |
| V-9 | C3-2, C2-5 (P19, no bound) | Any response of any SDK client sets the process-wide crypto clock, without a bound | CONFIRMED | Low |
| V-10 | C2-3 | A cancelled caller leaves a token refresh half done; the next caller sends the same refresh token again | CONFIRMED (client); PLAUSIBLE (the server's answer) | Low |
| V-11 | C2-4 | A block's encrypted body is read into memory without a size limit before any check | CONFIRMED (code); reachable size unproven | Low |
| V-12 | C4-2 | The marker cap prefers the lowest device UUID after every start | CONFIRMED | Low |
| V-13 | C1-6 | `scripts/ci.sh` opens its lock file by a predictable name in `/dev/shm` before the private root exists | CONFIRMED | Low |
| V-14 | C1-7 | Scan exemptions ending in `\b` also exempt longer paths under the same prefix, for every rule | CONFIRMED (the patterns); PLAUSIBLE (the scanner's matching) | Low |
| V-15 | C1-5 | No provenance for the archive; the user guide presents the checksum as the verification | CONFIRMED | Info |
| V-16 | C1-8 | The release binary carries OpenSSL's engine and module directories as paths under the build directory | CONFIRMED | Info |
| V-17 | C1-9 (items 1–5) | `build-doctor.sh` Go-module text and `git ls-remote` without `--`; `about.hbs` link scheme; the threat model's pull-request sentence; the fork carries upstream's workflows | CONFIRMED (1–4); item 5 needs the user to check settings | Info |
| V-18 | C2-5 (P17 flags, key cache, listing shape; P11; P12; P8; P4/P5) | Fork hardening: key flags fail open, key cache unbounded in count, block hash length, own draft by client UID only, `PUT`/`DELETE` retried after a timeout, three endpoints skip the HTTP status, XAttr signature failures | CONFIRMED (flags, P11, P12, P8); read, not traced further (the rest) | Info |
| V-19 | C3-3 | PdMount builds the SDK with its default features, so `image`'s decoders are in the binary; the threat model says "without formats" | CONFIRMED | Info |
| V-20 | C3-4 | New upload paths: a draft made but not returned is not cleaned up; clean-up errors logged unbounded; storage `Retry-After` slept in full; storage retries | CONFIRMED (draft, `Retry-After`, retries); read (log size) | Info |
| V-21 | C4-3 (eight items) | `detach` removes whatever is mounted at a root's mountpoint; `MountEntry::profile()` not validated; `shared-names.json` mode and lifetime; a remote rename to this device's swap name; marker notifications over time; `is_dead` thread; D128's root fallback; two profiles starting together | CONFIRMED (first three); PLAUSIBLE (start race); read (the rest) | Info |

**Verifier's overall assessment.** The four reports are precise: every line reference I followed was right or off by a
line or two, and no claimed code path turned out not to exist. The rebase onto upstream 0.5.1 kept the patches' checks
(area 2's range-diff reading was spot-checked on P1, P9, P13 and P19), and upstream's own changes bring nothing above Low.
The one High is a residue of the M5.8 fix: that fix exported the two classes of content it knew (journal entries and
local-only files) instead of every staged file that is not uploaded, as the M5.8 verification had proposed, and a third
class exists. The release pipeline, reviewed for the first time, has no path by which someone without push rights
publishes or changes a release; what it lacks is containment (one write token for every job, V-4), a trigger as narrow as
the documentation says (V-5), and three controls the threat model states but the release build does not have (V-6, V-7).
The threat model has no asset, actor or rating for the pipeline; the proposed text is at the end.

## V-1: A gone share deletes staged edits of an uploaded file in a local-only folder (C4-1)

CONFIRMED, High.

Trace. `SharedRoot::remove` counts `counts.pending + counts.parked + engine.local_only_staged().len()`
(`crates/pdmount/src/roots/shared.rs:866-867`), exports only when that is positive (`:868-871`) and otherwise calls
`purge_tree` (`:891`), which removes `trees/<key>/` with its staging (`crates/pdmount/src/roots.rs:987-1004`).
`local_only_staged` keeps a staged inode only when `is_local_only_file` holds, which requires `node.local_only &&
node.remote.is_none()` (`crates/pdmount-core/src/engine/orphan.rs:91-97`, `:116-121`), and `export_queued` walks the
journal's inodes plus that set (`:133-136`). A file that has a remote and sits in a local-only folder is in neither:

- `reclassify` never sets `local_only` on a node with a remote (`crates/pdmount-core/src/engine/ignore.rs:48-49`, `:56`), and
  the move of an uploaded file into a local-only folder is accepted, with the file staying where it was remotely (D121; test
  `a_folder_whose_child_went_into_a_local_only_folder_is_kept`, `crates/pdmount-core/src/engine/tests.rs:3782`).
- A write to it is staged and makes `content_pending()` true (`crates/pdmount-core/src/vfs.rs:267-269`), but `plan`
  returns `Plan::Done` at `in_local_only_folder` before it looks at the content
  (`crates/pdmount-core/src/journal.rs:1227-1232`, `:1372-1374`), and `Done` removes the journal entry (`:1317`, `:687-703`).

So the edit is never uploaded, never queued, and not counted. With nothing else queued the tree's state is purged; with
something else queued, `export_queued` still skips the file and `retire_tree` purges the state when no entry has an error
(`roots.rs:1174-1191`). The module doc of `orphan.rs` says nothing staged is deleted (RQ3).

I looked for further members of the class. A file created inside the local-only folder is `local_only` (covered). A
remote folder moved into a local-only folder keeps its remote, and its children are planned and uploaded to the old remote
place (not lost). A staged file that is unlinked but open has no path and is reported as an error entry, which keeps the
state. The class is therefore "a file with a remote whose parent is a local-only folder", as C4-1 says.

Verifier. The offline path already uses the wider rule: `reset --keep-login` counts every staging file as a change to
keep (`crates/pdmount-core/src/engine/open.rs:8-18`, "Staging files count too"). Only the live removal of a share uses
the narrower count.

Rating. High, as M5.8 V-1: acknowledged edits are deleted from the disk, silently (no `Orphaned:` line), and the deletion
is triggered by untrusted input (a revocation or deletion of the share by T2s), or happens without malice. The
precondition is the user's own (a local-only rule, a move of an uploaded file under it, an edit), which makes it rarer
than M5.8 V-1 but does not change what is lost. In My Files the same file is also never uploaded, but nothing deletes
its staging; that behaviour is §7.7's and is not rated here (see the fix's last point).

Fix (PdMount, M6.0.4):
1. In `orphan.rs`, replace `local_only_staged` by a set that does not depend on why a staged file is not queued, for
   example `unqueued_staged`: every inode of `self.staged` whose node is an attached, not deleted file and has
   `content_pending()` or `local_only`. Use it for the count in `SharedRoot::remove` (`shared.rs:867`) and in
   `export_queued` (`orphan.rs:134`).
2. In the manifest, keep `local-only` for a file without a remote and add a value such as `not-uploaded` for one with a
   remote; update DESIGN §7.7/§10 and D124's entry (a decision amendment, for the user).
3. Decide (user) whether an edit of an uploaded file inside a local-only folder should upload to the file's remote place
   instead of staying local for ever; `status` shows nothing for it today. This is a design question, not part of the fix.

Tests that pin it: beside `engine/tests.rs:1525` (M5.8 V-1's test), one with a remote file moved into a local-only folder
and written, then `export_queued`: the entry is present with the edited content and `not-uploaded`. A second for the count:
the same tree with nothing else queued reports a positive count, so `remove` exports. In `x16.sh`, the same steps before
the revocation, checking `orphaned/<tree>/files/`.

## V-2: The session-fork selector reaches the log (C2-2)

CONFIRMED for the log, PLAUSIBLE for the takeover. Medium.

Trace. `poll_session_fork` puts the selector into the request path and returns any transport error and any status
other than 422 as an error (`fork:crates/proton-sdk-rs2/src/auth.rs:736-749`). A reqwest transport error's text ends with
the URL, and `error_for_status_with_body` writes `for url ({url})` itself (`:492-514`). `begin_via_web` ends the login on
the first such error (`fork:crates/proton-sdk-rs2/src/session.rs:276`, the `?`). PdMount formats the chain with `{error:#}`
(`crates/pdmount/src/auth.rs:333-336`), logs it at WARN and keeps it in `login_error` (`:293-295`), which `status` returns
as `sign_in_error` (`crates/pdmount/src/daemon/handler.rs:108`); `pdmount login` prints the same chain. The claim of the
2026-10-31 review that the selector stays in memory does not hold for a failed poll.

Does the selector let a reader take the session? From the clients, yes, within limits:

- The poll carries no credentials. The official client calls it through `unauthenticatedRequest`
  (`cs/incubating/account/js/src/accountApi.ts:108-116`), so the selector is the only thing that authorises collecting
  `UID`, `AccessToken` and `RefreshToken`.
- Verifier: the API description says the selector "can only be used once per parent fork call", and it has a call that
  invalidates a selector without forking (`cs/incubating/account/js/src/api-auth-types.ts:119-135`). So a selector whose
  poll succeeded is dead; the logged one is live only because the daemon gave up.
- The reader gets the tokens only if the sign-in is completed in the browser after the failed poll, or was completed just
  before it (a poll that fails in transport right after the user approved leaves a filled fork that nobody collects), and
  only while the server keeps the fork. How long that is, and whether a filled fork stays collectable, was not tested.
- The tokens give the account's API (list, trash, delete: A4). The key password also needs the key from the link's
  fragment, which the same local reader can take from `xdg-open`'s argv (M5.8 V-15).

Rating. The Ratings list secrets in logs as High. I rate this Medium: the value is single-use and short-lived, it is
written only when a poll fails, the takeover needs the user to finish a sign-in the daemon has abandoned, and the reader
needs the 0600 log while the fork lives (T4, in scope through D071's promise; a log attached to a bug report later
carries a dead selector). It stays a broken promise of D071 with a small fix, so I recommend taking it in M6.0.4 with the
High.

Fix:
1. Fork (P16, or a new P20 if the patch is kept separate): in `poll_session_fork`, map the transport error with
   `reqwest::Error::without_url()`, and give `error_for_status_with_body` a form that prints the path template
   (`auth/v4/sessions/forks/<selector>`) instead of the URL for this call.
2. Fork, same patch: a poll that fails in transport or with 429/5xx is retried until `FORK_MAX_WAIT` instead of ending the
   login, which removes the usual trigger; when the login does end without a session, send the invalidating call for the
   selector (best effort).
3. PdMount, second layer: redact `/sessions/forks/<segment>` in the sign-in error before it is logged, stored in
   `login_error` or printed (`auth.rs:293-295`, `:333-336`, and the `login` command's error path).

Tests: a fork test with a mock server that answers the poll with 503 and one that drops the connection; the returned
error's `{:#}` text does not contain the selector. A PdMount unit test of the redaction with a URL in the chain.

## V-3: A node with an undecryptable parent fails the P13 batch and stalls the event sync (C2-1)

CONFIRMED for the code on both sides, PLAUSIBLE for an attacker reaching it. Medium.

`get_fresh_nodes_metadata` resolves each link's parent key with `?` inside the loop over all links of all batches
(`fork:crates/proton-drive-sdk/src/node/mod.rs:997-1010`, `:1001`); `resolve_parent_key` calls `FolderOperations::get_secrets`
(`:1059-1070`), which is an `Err` for a degraded parent, from the cache or from a fresh fetch
(`fork:crates/proton-drive-sdk/src/node/folder.rs:289-310`). PdMount passes the error on (`crates/pdmount-backend/src/proton.rs:1061-1076`),
`apply_events` returns it (`crates/pdmount-core/src/events.rs:276-280`), and when every tree of the volume failed the
poller returns it without moving the cursor (`crates/pdmount-core/src/poller.rs:295-303`), then retries with back-off and no
limit (`:200-208`). The stored cursor stays before the page, so a restart does not help. That the API stores a folder
whose passphrase does not decrypt to its parent's key, which is what gives a share member the lever, was not tested;
degraded nodes are an accepted case (D119), and the same state arises without an attacker.

Rating. Medium: a lasting stop of one tree's remote changes from remote input, with no data lost and uploads and reads
going on.

Proposed action: fix now, with M6.0.4 if time allows, otherwise Known gaps with the workaround (`reset --keep-login`).
Fork (P13): a failed `resolve_parent_key` yields that link as degraded (pass `None`, the converter reports "Parent folder
key is degraded"); `?` stays for transport and API errors. PdMount: after a few failures of the same page with a
non-transient error, `begin_refresh()` from a fresh cursor, as for a partial failure (`poller.rs:304-310`). Tests: a fork
test with a link whose parent's secrets are cached as degraded; a fake-backend scenario.

## V-4: The release token can write to the repository and its Releases in every job (C1-1)

CONFIRMED for the workflow; how far the token reaches needs the user to check settings. Medium.

`release.yml` grants `contents: write` workflow-wide (`.github/workflows/release.yml:17-18`) and sets `GH_TOKEN` in the
job environment of `plan` (`:56-57`), where the dist installer is piped into `sh` (`:67`), and of
`build-local-artifacts` (`:112-113`), where `dist build` runs cargo and with it every build script and proc-macro
(`:148-150`). dist generates the grant: `root_permissions` is always `contents: write`
(cargo-dist 0.32.0 `src/backend/ci/github.rs:300-301`). A build input that is malicious in its locked version can alter
the archive it builds in any case; the token adds the Releases already published (assets and checksums together), tags
and `master`.

Correction (verifier). The report says narrowing the grant needs `allow-dirty = ["ci"]`. dist's template gives the
build job a `permissions` block of its own when attestations are on in their default phase: `attestations: write`,
`contents: read`, `id-token: write` (`templates/ci/github/release.yml.j2:213-217`). A job-level block replaces the
workflow-wide one, so `github-attestations = true` in `dist-workspace.toml` also takes the write token away from the
build scripts, with no hand edit. The `plan` job keeps it (it creates the draft Release). This was read in the template,
not generated; check `release.yml` after `dist generate`. The build scripts then hold a token that can create
attestations, which lets a malicious one attest the archive it already controls, nothing more.

Rating. Medium under the rating proposed for the pipeline (a change to published Releases or the repository that needs
a compromised build input and that a missing, cheap control would have contained); under the current Ratings it has no
asset and would be Low.

Proposed action: fix now. GitHub settings (user): immutable releases; a ruleset for `master` and one for tags (V-5)
without a bypass for GitHub Actions; default workflow permissions read-only. Repository: `github-attestations = true`
(also V-15), `dist generate`, and the residue recorded in the threat model.

## V-5: Which tags start a release, and the tag name in shell commands (C1-3)

CONFIRMED for the workflow; needs the user to check settings. Low.

The trigger is `'**[0-9]+.[0-9]+.[0-9]+*'` (`.github/workflows/release.yml:43-45`), so `0.5.0`, `pdmount-v0.5.0` or
`backup-0.5.0` start a run, while `AGENTS.md`, `TODO.md` and the threat model (line 145) speak of `v*` tags, and a tag rule
for `v*` would not cover them. Nothing ties the tagged commit to `master` or to a passed CI. `github.ref_name` is expanded
inside `run:` scripts (`:80` directly; `:150`, `:203`, `:253`, `:284` through the `tag` and `tag-flag` outputs of `:53-54`),
and git allows `$`, `(`, `)`, `;`, `|` and backquotes in tag names, so a tag such as `v0.5.1$(…)` runs a command in `plan`
with the write token. On a pull request the expression is the constant `plan`, so only someone who can push a tag reaches
this; it gives them nothing they lack today and becomes an escalation once tag creation is restricted by pattern.

Proposed action: fix now, by settings (user): a tag ruleset that restricts creation, update and deletion for all tags
(`*`) to the maintainer (area 1 notes that rulesets on a personal repository cannot restrict tag names by regex; not checked). Repository: write the
real pattern into the threat model and `AGENTS.md`.

## V-6: `release.yml`'s actions can be pinned by commit (C1-2)

CONFIRMED, Low. dist 0.32.0 has `github-action-commits`, a map from action name to the ref to use
(`src/config/v0.rs:434-436`), applied over the default major tags for `actions/checkout`, `actions/upload-artifact`,
`actions/download-artifact` and `actions/attest` (`src/backend/ci/github.rs:368-386`). This settles the report's medium
confidence about the format: `[dist.github-action-commits]` with `"actions/checkout" = "<40-hex commit>"` and so on. The
threat model's acceptance (lines 132-133, "pinning would need `allow-dirty`") no longer holds. The installer script at
`release.yml:67` stays without a hash check.

Proposed action: fix now (`dist-workspace.toml`, `dist generate`, threat model); add `actions/attest` to the map when
V-15 is done.

## V-7: The release build has neither `--locked` nor the pinned toolchain (C1-4)

CONFIRMED, Low. dist's cargo command carries no `--locked` (cargo-dist 0.32.0 `src/build/cargo.rs:191-241`: `build`,
`--profile`, `--message-format`, `--target`, features, `--workspace` or `--package`), which raises the report's medium
confidence to high. `release.yml` installs Rust only inside containers (`:123-129`), and there is no `rust-toolchain.toml`,
so the compiler is the runner image's stable. Correction: the threat model's line 138 is in the bullet on the hand-written
workflows, where it is true for `ci.yml` (`.github/workflows/ci.yml:26`, `:49-52`); what is missing is a sentence that the
release build is not covered. Line 140 ("built with `--locked`") is true of `scripts/ci.sh` (`:115-117`, `:164-165`) and
wrong for the release build. With a lock file that matches `Cargo.toml`, cargo honours it without the flag.

Proposed action: fix now. Two steps in `.github/build-setup.yml`: install and select the pinned toolchain (or a
`rust-toolchain.toml` used by both workflows), and `cargo metadata --locked --format-version 1 >/dev/null`, which fails
when the lock would change.

## V-8: Signed at server time, verified at device time (C3-1)

CONFIRMED for the code; the EIO window is real but needs a cache miss. Low.

Signing and key generation use `server_time::check_now()` / `now()` (for example `fork:crates/proton-drive-sdk/src/pgp.rs:50`,
`:61`, `crypto.rs:27`, `node/draft.rs:230`); verification uses `Verifier::default()`
(`fork:crates/proton-drive-sdk/src/node/download.rs:164`, reached from `node/file/block_map.rs:272`), whose date is the
system clock (`proton-rpgp` 0.3.1 `src/verify.rs:55`), and rPGP refuses a signature created after that date with no
tolerance (`src/signature.rs:77-85`). PdMount maps the failure to `BackendError::Integrity`
(`crates/pdmount-backend/src/proton.rs:716-722`) and that to EIO (`crates/pdmount-core/src/errors.rs:102-108`), with an ERROR
line "block failed verification" (`crates/pdmount-core/src/cache/block_cache.rs:551-555`).

Whether the window happens. On a machine whose clock is slow by N seconds, a revision this machine uploaded cannot be
verified by it for N seconds. PdMount does not normally read its own upload back: after the commit it seeds the block
cache and the layout from the snapshot (`crates/pdmount-core/src/journal.rs:2963-2975`,
`crates/pdmount-core/src/cache/block_cache.rs:640-677`). The window opens only when the seed did not happen or is gone
within those N seconds: the disk cache is closed (`seed` returns at `:649-651`, the daemon waiting for the keyring), the
block was evicted or the cache wiped, or another profile or machine with the same slow clock reads the file. Verifier:
the same machine already failed, with 0.4, to verify revisions that other clients uploaded in its last N seconds, since
the official clients date by server time too; 0.5 adds its own uploads to that. The check fails closed and passes once
the clock has caught up.

Proposed action: fix now if a fork patch is made for V-2 anyway (P20 or P21: verification sites take
`server_time::check_now()`), otherwise Known gaps plus a sentence in `doctor`'s clock line.

## V-9: The process-wide crypto clock takes any response, without a bound (C3-2, C2-5's P19 item)

CONFIRMED, Low. `update` adopts any observed offset that differs by 2 s or more
(`fork:crates/proton-drive-sdk/src/server_time.rs:27-41`), from `x-pm-date` or `Date` (`:44-54`), on every response of the
API, storage, Photos and public-link clients (`client.rs:199-201`, `:299-301`, `photo.rs:131`, `sharing.rs:917`, `:921`,
`http.rs:70`). No overflow (`saturating_sub`, `abs_diff`). Both areas reported it; C3-2 has the latency case, C2-5 the
arithmetic. Upstream's behaviour and the official client's; P19 makes it neither better nor worse.

Proposed action: M7 (or with V-8's patch): time from the Drive API host only, an offset beyond a day refused with a WARN,
the offset shown in `doctor`.

## V-10: A cancelled caller leaves a token refresh half done (C2-3)

CONFIRMED on the client side, PLAUSIBLE for the server's answer to a reused refresh token. Low.
`get_refreshed_access_token` runs the request inside the caller's future through tokio's `OnceCell::get_or_init`
(`fork:crates/proton-sdk-rs2/src/auth.rs:359-364`), and `in_flight` is cleared only when `run_refresh` completes
(`:383-395`), so a dropped caller leaves the attempt with the old pair for the next one. Not introduced by P9.

Proposed action: Known gaps; fix in M7 (run the refresh in a task of its own).

## V-11: Block body read without a size limit (C2-4)

CONFIRMED for the code, Low. `response.bytes().await?.to_vec()` (`fork:crates/proton-drive-sdk/src/block/download.rs:75-78`),
the whole plaintext before the write (`:92`). The largest block the storage accepts was not tested.

Proposed action: Known gaps; fix in M7 (a `Content-Length` and chunked limit in `download_verified`).

## V-12: The marker cap's order after a start (C4-2)

CONFIRMED, Low. The sort key is "not in `previous`", then the UUID (`crates/pdmount-core/src/engine/marker.rs:621-628`);
`previous` is the in-memory `seen`, empty at a start and cleared when the gate closes (`:670-673`). Sixteen markers with
low UUIDs planted by a share member keep the user's other devices' markers out of that share's list. Advisory data;
`status` shows the "+N more" count.

Proposed action: Known gaps; fix in M7 (keep the known device ids with the stored `notified` keys and prefer them).

## V-13: `ci.sh`'s lock file in `/dev/shm` (C1-6)

CONFIRMED, Low. `exec 9>"$SCENARIO_ROOT.lock"` (`scripts/ci.sh:157`) opens a predictable sibling of the root before
`private_root` (`tests/scenarios/lib.sh:60-69`) has run. With the usual `fs.protected_symlinks` and `fs.protected_regular`
the result is a denial of the full CI for that checkout; the sysctls were not read. The pattern S-8 fixed for the root.

Proposed action: fix now (create the root first, lock `"$SCENARIO_ROOT/.lock"`); it is a three-line change.

## V-14: Scan exemptions wider than their descriptions (C1-7)

CONFIRMED for the patterns (`.betterleaks.toml:4`, `:9-15`, `:21-29`, `:39-40`, `:45`, `:50`; lines 23 and 25 show the tight
form), PLAUSIBLE for the effect: what the scanner matches an allowlist against was inferred from gitleaks' semantics, and
the private rule file was not read, by rule. Low.

Proposed action: fix now for `:4`, `:45` and `:50` (end with `(?:[^/\w.-]|$)` or list the children used); ask the user
before naming a rule ID in the repository.

## V-15 to V-21: Info items

- **V-15** (C1-5): `dist-workspace.toml:16` has the checksum only; `docs/user-guide.md:17-25` gives `sha256sum -c` as the
  one check. dist 0.32.0 has `github-attestations` with a phase option (`templates/ci/github/release.yml.j2:213-217`,
  `:509-514`). CONFIRMED. Proposed action: fix now together with V-4 (it also narrows the build job's token); add
  `gh attestation verify` and one sentence on what the checksum proves to the user guide.
- **V-16** (C1-8): a local release build has `ENGINESDIR` and the modules directory under
  `<checkout>/target/release/build/openssl-sys-<hash>/out/openssl-build/install/lib/` (four strings in the existing
  binary). CONFIRMED for the strings; that nothing loads a provider or engine by name was not traced beyond the report.
  Proposed action: record in the threat model.
- **V-17** (C1-9): (1) `scripts/build-doctor.sh:157`, `:167-183` speak of Go module downloads although `gopenpgp-sys`
  vendors them; (2) `git ls-remote --heads "$fork"` without `--` (`:161`); (3) `about.hbs:103` writes `crate.repository` into
  an `href` without a scheme check; (4) the threat model's pull-request sentence (lines 134-135) credits `pr-run-mode` where
  the read-only token of fork pull requests and the `publishing` condition do the work (`release.yml:55`, `:96`); (5) the
  fork's tree has upstream's `.github/workflows/ci.yml` and `release.yml` (manual, publishes crates). CONFIRMED for 1–4;
  item 5 is a setting (whether Actions are off on the fork). Proposed action: fix 1–4 now (small edits), user checks 5.
- **V-18** (C2-5 without its P19 item): `may_verify` accepts a missing `Flags`
  (`fork:crates/proton-sdk-rs2/src/api/response.rs:262-265`); an own draft is recognised by client UID alone
  (`fork:crates/proton-drive-sdk/src/node/draft.rs:62-95`), with PdMount's UID `pdmount-<device id>`
  (`crates/pdmount-core/src/device.rs:18-20`) and no override of other clients' drafts
  (`crates/pdmount-backend/src/proton.rs:1163`); `PUT` and `DELETE` count as idempotent and a 429 is retried for every
  method (`fork:crates/proton-drive-sdk/src/http/retry.rs:93-103`, `:113-128`), which widens S-25 from 5xx to timeouts;
  `move_link`, `move_multiple` and `copy_link` parse the body without the status (`fork:…/api/links.rs:194-200`, `:215-221`,
  `:294-298`). CONFIRMED. The key cache's growth per email, the 32-byte hash length and the XAttr signature handling were
  read at their lines and not traced further. Proposed action: Known gaps; the flags default and the three endpoints are
  one-line fork changes for the next patch round.
- **V-19** (C3-3): `Cargo.toml:47` takes the SDK with default features, which include `thumbnail-generation`
  (`fork:crates/proton-drive-sdk/Cargo.toml:56`, `:71`), so feature unification builds `image` with formats although
  `Cargo.toml:34` asks for none. PdMount calls no generic decoder. CONFIRMED. Proposed action: fix now if a build shows
  `cache-sqlite` is the only default needed (then regenerate `THIRD-PARTY-LICENSES.html`); otherwise correct the threat
  model's line 116.
- **V-20** (C3-4): a failed block verifier after `create_file` returns without the draft's uid
  (`fork:crates/proton-drive-sdk/src/node/draft.rs:283-290`); a storage 429's `Retry-After` is slept in full
  (`fork:…/api/storage.rs:153-157`), the SDK-side twin of M5.8 V-21 and S-10; storage retries now include `is_request()` and
  `is_body()` (`:198`). CONFIRMED. Proposed action: Known gaps, with S-10.
- **V-21** (C4-3): `detach` runs `fusermount3 -u`, then `-uz`, on whatever `mounts::find` returns
  (`crates/pdmount/src/roots.rs:1438-1448`), without D127's profile check; `MountEntry::profile()` returns the mount table's
  text unvalidated (`crates/pdmount/src/mounts.rs:31-35`); `save_names` writes with the umask's mode
  (`crates/pdmount/src/roots/shared.rs:143-152`) and `reset` does not remove the file (`crates/pdmount/src/daemon/reset.rs:66-99`;
  only `shared.rs:473` uses `shared_names_file`). CONFIRMED, same uid or protected by the 0700 data directory (as M5.8
  V-22). The start race of two profiles is PLAUSIBLE (not traced into fuser). The swap-name, notification-rate, `is_dead`
  thread and root-fallback items were read and are as described. Proposed action: fix `detach` and `profile()` now (two
  small checks that complete D127); the rest to Known gaps.

## Fix completeness of the M5.8 findings

Area 4's table stands with one change in wording: M5.8 V-1 is not complete (this review's V-1). M5.8 V-2 to V-8 and D128
are complete as decided; the residues are V-12 (cap order) and V-21 (`detach`, the names file, the swap name).

## Fix plan for M6.0.4

| Group | Rating | Where | Change | Test |
|---|---|---|---|---|
| V-1 | High | PdMount | Export and count every staged, attached file with `content_pending()` or `local_only` when a tree is gone; manifest value `not-uploaded`; D124 amended | Engine tests beside `engine/tests.rs:1525` (export and count); `x16.sh` step |
| V-2 | Medium (High by the Ratings' letter) | Fork (P16 extended or P20) and PdMount | Selector out of poll errors; poll retried on transport and 429/5xx errors; selector invalidated when a login ends without a session; redaction in PdMount | Fork tests with a 503 and a dropped connection; PdMount redaction test |
| V-4 | Medium | GitHub settings (user) and `dist-workspace.toml` | Immutable releases, rulesets, read-only default token; `github-attestations = true` | `dist plan`; read the generated `release.yml`; check an `-rc` Release |

The next free patch number is P20. If V-2's change extends P16 no number is used; V-8 would then be P20.

## For the user: GitHub settings to check

None of these is visible in the repository; each decides how far a finding reaches.

1. **Immutable releases** for the repository (V-4): on, so published assets and their tag cannot be replaced.
2. **Tag ruleset** (V-5): creation, update and deletion of all tags restricted to the maintainer, with no bypass for
   GitHub Actions. A rule for `v*` alone does not cover the workflow's trigger.
3. **Branch ruleset for `master`** (V-4): no force push or deletion, no bypass for GitHub Actions.
4. **Default workflow permissions** (V-4): read-only (Settings, Actions, General). `release.yml` asks for its own grant.
5. **Approval for pull requests from forks** (threat-model actor S1): required at least for first-time contributors.
6. **Actions on the SDK fork** (V-17): disabled, since the fork carries upstream's workflows.
7. **The v0.5.0 release run's log**, if that release exists: the `cargo build` line (V-7) and the Rust version used.

## Proposed threat-model changes

Line numbers are of `docs/security/threat-model.md` at `9818aa1`. Area 1's proposals were checked against the workflow
files and dist's source; the changes from its text are noted.

**1. Line 9, after the sentence on the 2026-10-31 review (and after any entry for 2026-11-15), add:**

> 2026-11-18 (M6.0.3), PdMount `976ea61..9818aa1` and, for the first time, the release pipeline; SDK fork `pdmount-0.5`
> `70f10ff` on upstream v0.5.1; reports in `docs/security/2026-11-18-review/`.

**2. After line 26 (asset A5), add:**

> - A6 The published Releases: the archive and checksum of every version, and their binding to a tag and commit.
> - A7 The repository: `master`, the tags and the workflow files, and the SDK fork repository at the pinned commit.
> - A8 The development machine as far as the project's scripts touch it: the push credentials (kept outside the
>   repository, D090) and the files that `scripts/ci.sh`, `scripts/build-doctor.sh` and the scenario scripts create or
>   remove.

Change from area 1: A7 names the fork repository too, since the build fetches it.

**3. Line 57, in T4, after "secrets never printed or logged (tokens, passphrase", insert:**

> , and the session-fork selector of a web sign-in, which alone authorises collecting the new session's tokens, V-2

**4. After line 63 (T5), add:**

> - Release pipeline actors (M6.0.3):
>   - S1 A contributor without write access (a pull request from a fork): runs their own code in `ci.yml` and in the
>     `plan` job of `release.yml`, with a read-only token and no secrets. Controls: no `pull_request_target`, no cache
>     shared with tag builds, the `publishing` condition of the build and host jobs, approval for pull requests from
>     forks (a GitHub setting).
>   - S2 A compromised build input: the runner image and its apt packages, an action, the dist installer, a crate's build
>     script or proc-macro, the Go toolchain, the fork repository. It can alter the archive it builds; the controls limit
>     what else it reaches (A6's earlier Releases, A7).
>   - S3 A holder of push rights: the maintainer's account, the push key on the development machine, a mistaken tag.
>     Any tag matching `**[0-9]+.[0-9]+.[0-9]+*` starts a release, not only `v*`; the release does not wait for CI, and the
>     tagged commit need not be on `master`.
>   - S4 Someone between the Release and the user: a replaced asset, a mirror.

**5. Lines 129-135 (the dist bullet), replace from "so it is not hand-edited" to the end of the bullet with:**

> so it is not hand-edited. Its actions (`actions/checkout`, `upload-artifact`, `download-artifact`) are pinned by commit
> through `github-action-commits` in `dist-workspace.toml` [once V-6 is fixed; until then: are referenced by major tag,
> although dist can pin them]. It installs the `dist` binary at the pinned version with the installer script from dist's
> GitHub release, with no hash check (dist offers none). Reviewed when dist is upgraded (`dist init`, then read the diff
> of `release.yml`). The workflow grants `contents: write` to every job and uses only `GITHUB_TOKEN`; no other secrets
> exist. The build job's token is narrowed to `contents: read` by the attestation settings [once V-4 and V-15 are fixed];
> the `plan` and `host` jobs keep the write token. Pull requests run `dist plan` only (`pr-run-mode = "plan"`). A pull
> request from a fork runs the workflow of its own merge commit, so what keeps it from a release is the read-only token
> GitHub gives such runs and the `publishing` condition, not the run mode.

**6. Line 138, replace "The Rust toolchain comes from rustup at a pinned version;" with:**

> In `ci.yml` the Rust toolchain comes from rustup at a pinned version; the release build uses the same pin through
> `.github/build-setup.yml` [once V-7 is fixed; until then: the runner image's stable toolchain];

**7. Line 140, replace "`Cargo.lock` is checked in and built with `--locked`;" with:**

> `Cargo.lock` is checked in. `scripts/ci.sh` builds with `--locked`; dist's build does not pass it, so the release job
> first checks that the lock file would not change (`cargo metadata --locked`) [once V-7 is fixed];

**8. Lines 145-147 (the tag bullet), replace with:**

> - **What a tag push does.** A pushed tag matching `**[0-9]+.[0-9]+.[0-9]+*` starts the release build (`v0.6.0`, but also
>   `0.6.0` or `x-0.6.0`), from whatever commit it names. Whoever can push tags can publish a Release, and the tag name is
>   expanded in the workflow's shell commands. The controls are GitHub settings: a ruleset restricting the creation,
>   update and deletion of all tags to the maintainer, a ruleset for `master`, immutable releases, and read-only default
>   workflow permissions (state checked by the user: [date and result]). The archive's `.sha256` is produced in the same
>   run, so it proves integrity of the download, not provenance; provenance comes from the build attestation
>   (`gh attestation verify`) [once V-15 is fixed].

**9. Lines 148-149 (local archive check), append:**

> The hosted build embeds only paths under the runner's home, among them the directories OpenSSL would load engines and
> providers from, which nothing in PdMount asks for (V-16).

**10. Line 150, replace with:**

> - **Not done:** reproducible builds, signed archives, hash-pinned apt packages, a hash check of the dist installer.

(Build attestations leave the list once V-15 is fixed.)

**11. Line 116, replace "`image` 0.25.10 without formats (scaling only)" with** (unless V-19 is fixed by dropping the
SDK's default features):

> `image` 0.25.10, asked for without formats and used for scaling only; the SDK's default `thumbnail-generation`
> feature builds its decoders into the binary, and no PdMount code calls them (V-19)

**12. Ratings, after line 165, add:**

> - Release pipeline (A6–A8). Critical: someone without write access publishes or changes a Release, or changes
>   `master` or a tag. High: the same by a holder of a narrower right than the maintainer's (a restricted tag or branch
>   right, a pull request).
>   Medium: a compromised build input (S2) reaches beyond the build it runs in (earlier Releases, `master`, tags) where a
>   stated or cheap control would have contained it. Low / Info: hardening, a stated control that a normal release does
>   not depend on.

Change from area 1: its proposal had two steps (High without the maintainer's credentials, Medium with a compromised
build input). The scale above adds Critical for an outsider, to match A1's and A5's weight, and ties Medium to reach
beyond the build, since a compromised build input controls its own build whatever the controls. By it, V-4 is Medium and
V-5 to V-7 are Low.
