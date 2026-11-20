# Verification of the M6.5 area reports (verifier, 2026-11-20)

Reviewed tree: `7faae80` (scope `6456097..7faae80`: M6.1–M6.4 and the two review-fix commits). Threat model:
`docs/security/threat-model.md` (D070), its Ratings section. Area reports: `area1.md` (notification text: escaping,
names, privacy; finding IDs C1-n) and `area2.md` (D-Bus, processes, the dependency; C2-n) in this directory, copied as
the reviewers wrote them. Method: every finding, the Info bundles included, was traced in the source at `7faae80`;
line numbers below are of that commit. The notification servers' and file managers' sources (GNOME Shell, Plasma,
dunst, mako) were not re-read: what area 1 says about them is taken as stated, and the verdicts say so. Duplicates
across the areas are merged into one group (V-n, numbered anew for this review); findings of earlier reviews are
written "M5.8 V-n" or "M6.0.3 V-n". Fixes were made in the M6.5 exit commit with a test each, at the lines named.

Nothing is Critical, High or Medium. Seven groups are Low and five Info. No finding is rejected.

| Group | Members | Title | Verdict | Final rating | State |
|---|---|---|---|---|---|
| V-1 | C1-4 | A renamed share keeps its old name and folder in its notifications; **Show** can point into another share | CONFIRMED (code); the harm needs the user to act on **Show** | Low | open (M7) |
| V-2 | C1-2, C2-4 | The unverified-signatures summary cleans a share's name with `status::printable`, not `clean_name` | CONFIRMED | Low | fixed |
| V-3 | C1-3 | `clean_name` lets invisible and blank characters and slash look-alikes through | CONFIRMED | Low | fixed (notifications); one rule for every name open (M7) |
| V-4 | C1-5 | The server's capabilities are asked once per connection; a server that changes keeps the old markup decision | CONFIRMED | Low | fixed |
| V-5 | C1-1 | Address-like names in a body become links (GNOME Shell, dunst) | CONFIRMED (code); PLAUSIBLE (servers, not re-read) | Low | open (M7) |
| V-6 | C2-1 | **Show**'s `xdg-open` fallback also runs when the bus did not answer | CONFIRMED (code); PLAUSIBLE (the race) | Low | fixed |
| V-7 | C1-6, C2-5 (log site) | **Show**'s fallback logs the folder with Display formatting (log injection) | CONFIRMED | Low | fixed |
| V-8 | C2-2 | The `xdg-open` child: `PATH` lookup in the working directory, `NOTIFY_SOCKET`, the service's cgroup | CONFIRMED | Info | fixed (lookup, cwd, `NOTIFY_SOCKET`); cgroup no fix |
| V-9 | C2-3 | Forged `NameOwnerChanged` lets any session process invoke **Show**; no limit | CONFIRMED | Info | fixed (once per notification) |
| V-10 | C1-7 | Name lengths; `conflict_names` covers conflicts only; a share named `My Files`; no hints | CONFIRMED | Info | documented (`conflict_names`); rest open (M7) or no fix |
| V-11 | C2-5 (four items) | Stale connections kept; the attach's display cleared by a late drop; a hung server costs each conflict; recounted sums | CONFIRMED (stale, hung, recount); PLAUSIBLE (display race) | Info | open (M7) |
| V-12 | C2-5 (threat model) | The threat model still describes `notify-send` and lacks the M6 surfaces | CONFIRMED | Info | fixed (this commit) |

**Verifier's overall assessment.** Both reports are accurate: every cited line was right or off by one or two, and every
traced code path exists as described. The M6 code keeps the D102 rule (no connection to a session bus without the peer
check) everywhere, and no remote text reaches a process's arguments or the summary as markup. What is left is display:
remote names can still mislead in a notification (V-1, V-5), which the threat model rates Low because nothing on the
disk or in the account changes without the user's own action. One correction to area 1's fix for V-5 (below).

## V-1: A renamed share keeps its old name and folder (C1-4)

CONFIRMED, Low. `SharedRoot::open` builds the conflict `Place` from the shown name once
(`crates/pdmount/src/roots/shared.rs:822-830`, `conflict_notify.rs:109-115`: `root = mountpoint.join(name)`) and
`announce_tree` starts the incoming and signatures announcers with the same name (`roots.rs:1775-1787`). An owner's rename
reaches `set.rename` and `share.shown.name` only (`roots/shared.rs:633-639`); nothing tells the announcers. `display_names`
gives the freed name to a newcomer whose own name it is (`roots/shared.rs:170-215`: a kept name stays only while the
share's name is unchanged), so a second share named `Team` takes `~/Shared/Team`, and **Show** (`target`,
`conflict_notify.rs:268-277`) points there. Rated Low: the user is misled into opening another share's file; nothing is
written without the user's action, and only within a share the user may already write. Without a second share, **Show**
opens the parent folder of a path that does not exist (`xdg-open` fallback) or the file manager reports it missing.

Not fixed in M6: the fix is not local. Proposed for M7: keep the share's key in `Place` (or let `Tree` hold a place that
`SharedRoot`'s rename updates), resolve label and root when a notification is built, and pass the current name to the
incoming and signatures announcers the same way; a test that renames a share between two conflicts.

## V-2: The signatures summary uses `printable` (C1-2, C2-4)

CONFIRMED, Low (area 2 rated it Info; area 1's Low is kept, as the case is the one `clean_name` exists for).
`signatures::notification` (`crates/pdmount/src/signatures.rs:417-421`) wrapped the tree's name in `status::printable`,
which replaces only `char::is_control` (`status.rs:108-112`); DESIGN.md §14.4 says every name in a notification goes
through `notify::clean_name`. **Fixed:** `clean_name` there; test `the_notification_gives_the_count_and_the_command_never_names`
(U+202E and U+2028 in a share's name).

## V-3: `clean_name` is narrower than the device-name rule (C1-3)

CONFIRMED, Low. `clean_name` (`notify.rs:283-310`) replaced Cc, U+2028/2029 and the bidirectional controls only;
`marker::is_hidden` (`pdmount-core/src/engine/marker.rs:213-224`) also removes U+00AD, U+180E, U+200B–U+200D,
U+2060–U+206F, U+FEFF and U+FFF9–U+FFFB. **Fixed for notifications:** `clean_name` now replaces `is_hidden`'s set plus the
tag characters (U+E0000–U+E007F), U+034F, U+180B–U+180D, the supplementary variation selectors (U+E0100–U+E01EF), the
letter blanks U+115F, U+1160, U+3164, U+FFA0, U+2800, and the slash look-alikes U+2044, U+2215, U+29F8, U+FF0F; the emoji
variation selectors U+FE00–U+FE0F stay (they hide nothing and are part of ordinary emoji), and so U+200D (ZWJ) is now
replaced inside emoji sequences, as in device names. Test `names_are_cleaned_of_invisible_blank_and_slash_like_characters`.
Open (M7): one function for every name shown (notifications, `status`, owner labels, device names, which still use
`printable` and `is_hidden`), as M5.8 V-10 asked.

## V-4: Capabilities cached per connection (C1-5)

CONFIRMED, Low. `Entry::caps` was a `OnceCell` per connection (`notify.rs:349`, `:396-407`); the proxy addresses the
well-known name, so after another server takes it (or mako reloads with markup on) bodies were sent unescaped to a
server that parses markup. **Fixed:** the capabilities are a cache that the entry's listener clears on the proxy's
`NameOwnerChanged` stream; the next send asks again. Test `another_server_is_asked_for_its_capabilities_again` (a server
without `body-markup` replaced by one with it: the body is escaped; it fails without the fix). A forged
`NameOwnerChanged` (V-9) only makes the daemon ask again.

## V-5: Address-like names become links (C1-1)

CONFIRMED for PdMount's side, PLAUSIBLE for the servers (area 1 read GNOME Shell's `URLHighlighter`/`findUrls` and
dunst's URL extraction; not re-read here, not run on a desktop). Names go into the download body
(`downloads.rs:148-157`) and the conflict body (`conflict_notify.rs:235-251`) as plain text, which the server may
linkify. Low: a phishing link in a notification under PdMount's name, one click away; the same name is visible in the
file manager anyway, and nothing on the disk changes.

Verifier correction to the proposed fix: a U+2060 WORD JOINER inserted *after* `.` does not stop GNOME's `www\d{0,3}\.`
pattern, which matches `www.` and then takes any run of non-space characters, U+2060 included (it is not `\s` in
JavaScript); the link would only point at a different host. Inserting the joiner *before* each `.` and after each `:`
would break `www.`, `name.tld/` and `scheme:`, but how each server's regular expression treats it must be checked on
the desktops. Open (M7), with that check; V-3's cleaning does not touch the case. Moving names into the summary (plain
text for GNOME and Plasma) is the alternative.

## V-6: **Show**'s fallback on a bus that did not answer (C2-1)

CONFIRMED in the code, PLAUSIBLE for the race (another user must bind the freed address of a `dbus-launch` bus within
the logout window and the user must click **Show** in it). `show_items` (`conflict_notify.rs:399-421`) turned every
`zbus::Error` (a closed connection too) and the timeout into `ShowError::Call`, and `show` (`:367-386`) then ran
`xdg-open` with that address, unchecked. Low, as M5.8's V-28 class: the click goes to the other user's process, no
secret on that path. **Fixed:** only an error reply (`zbus::Error::MethodError` or `FDO`, the bus answered over the
connection that passed the peer check) starts the fallback; a connection error or a timeout opens nothing and
calls `bus::drop_if_gone_async` (`ShowError::Lost`). This also removes the double opening after a slow activation. A
re-check just before the spawn (area 2's second step) adds nothing: the error reply came over the checked socket
milliseconds before. Tests `show_opens_nothing_when_the_bus_does_not_answer` (a file manager that never answers) and
`only_an_error_reply_is_an_answer`; `show_falls_back_to_the_opener` still passes (`ServiceUnknown` is an answer).

## V-7: Log injection at **Show**'s fallback (C1-6, C2-5)

CONFIRMED, Low (M5.8 V-10's class, a new site). `folder = %folder.display()` at INFO (`conflict_notify.rs:373-377`)
wrote a remote folder name unescaped. **Fixed:** `?folder` (Debug escapes `\n`, ESC and the like), as the `uri` field
beside it.

## V-8: The `xdg-open` child's environment (C2-2)

CONFIRMED, Info. `Command::new("xdg-open")` (`conflict_notify.rs:517`, `:426-452`) resolved the program through the
daemon's `PATH`, so an empty or `.` entry found a program in the daemon's working directory (the launching shell's,
`daemon/launch.rs:72-92`); `NOTIFY_SOCKET` stayed in the child's environment under systemd. **Fixed:** a bare opener name
is looked up in absolute `PATH` directories only (`resolve_opener`), the child runs in `/`, and `NOTIFY_SOCKET` is
removed; test `the_opener_comes_from_an_absolute_path_directory`. Not fixed: the file manager that `xdg-open` starts
stays in the service's cgroup under systemd (a daemon restart closes it; D139 says so). Starting it through
`systemd-run --user --scope` is a design question for the user (M7 or no fix). The daemon's own working directory
is unchanged.

## V-9: Forged signals and **Show** without limit (C2-3)

CONFIRMED, Info (T4: a process of the user's own session). zbus 5.19 lets a unicast `NameOwnerChanged` from any sender
redirect the proxy's signal filter, as area 2 read; each `ActionInvoked` with key `show` for an open notification
spawned a **Show**. **Fixed:** **Show** acts once per notification (the entry leaves `open` at the first action); test
`show_falls_back_to_the_opener` sends the action twice and sees one opener run. The forged owner change can still make
the watcher re-run a catch-up, which shows only what is unshown (no harm).

## V-10: Info bundle of area 1 (C1-7)

CONFIRMED, Info. (a) No name is shortened before a notification, so a long share name pushes the fixed words out of
GNOME's title (150 graphemes); open (M7: cut names to about 64 graphemes with an ellipsis in the middle). (b)
`notify.conflict_names = false` covers conflict notifications only (D139); share names stay in the orphan, held,
incoming and signatures summaries, and file names in downloads (D137). **Documented** in the user guide (which
notifications the key covers, and `notify.downloads = false` for downloads); whether to widen the key is for the user
(M7). (c) A share may be named `My Files` (`roots/shared.rs:175-187` reserves only `Shared files`), so its conflicts read
like My Files'; open (M7: reserve the label). (d) No hints and no icon are sent (`notify.rs:439`); no fix, keep it so.

## V-11: Info bundle of area 2 (C2-5, four items)

- Stale connections: `ENTRIES` keeps one entry per bus address ever used until a call on it fails
  (`notify.rs:450-507`). CONFIRMED; bounded by the number of logins in a daemon's life. Open (M7: prune on
  `bus::attach_changes()`).
- The attach's display: `handle_attach` calls `remember_display(new)` before `keep(new)` (`keywait.rs:722-727`), and a
  `detach(old)` between them clears `DESKTOP_DISPLAY` (`bus.rs:105-118`). PLAUSIBLE: the window is between two adjacent
  calls. Effect: **Show**'s `xdg-open` gets the daemon's own display variables. Open (M7: set the display under the
  `ATTACHED` lock in `keep`/`set_attached`).
- A hung server: while a retry is pending, each new conflict still tries a full send (`conflict_notify.rs:661-669`,
  `:675-700`; every call bounded). CONFIRMED; delay only. Open (M7: hold new conflicts while `retry` is set).
- Recounted sums: conflicts beyond `MAX_HELD` (256) are counted in `missed` without ids, and a later catch-up with the
  window full counts the same unshown conflicts again (`:644-650`, `:779-820`). CONFIRMED; only with more than 256
  unshown conflicts, and only the "N more" figure is wrong. Open (M7: count from `waiting()` at flush time).

## V-12: Threat model out of date (C2-5)

CONFIRMED, Info. **Fixed** in this commit: the review is listed, T4 no longer names `notify-send`, and an "M6 surfaces"
entry describes the zbus client, the inbound signals, **Show** and its fallback, and what is test-only.

## For the user

Open, proposed for M7 (or no fix, the user's choice): V-1 (follow a share's rename), V-5 (links in bodies: check on
GNOME and dunst before choosing a fix), V-3's single rule for every shown name, V-8's cgroup, V-10 (a), (b) widening
`conflict_names`, (c), and V-11's four items. None blocks the M6 release.
