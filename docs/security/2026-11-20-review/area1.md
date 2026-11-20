## Area C1 (notification text: escaping, names and privacy; M6.1–M6.4): findings

Reviewed read-only at `m6x-fix` (7faae80), `git diff 6456097..m6x-fix` for: `crates/pdmount/src/notify.rs` (`clean_name`,
`escape_markup`, `Entry::caps`, `Entry::send`, `desktop`), `conflict_notify.rs` (`body`, `Place`, `target`, `show`), `downloads.rs`,
`notify_watch.rs`, `incoming.rs`, the notification and logging parts of `roots.rs` and `roots/shared.rs`, `auth.rs`, `keywait.rs`,
`flush.rs`, `resume.rs`, and `signatures.rs` (not in the diff, but it builds one of the notifications in scope);
`pdmount-core/src/slow.rs` and the `read` paths that report into it (`engine/io.rs`, `photos.rs`). Decisions D070, D125, D137–D140;
DESIGN.md §10.8, §14.4; threat model `docs/security/threat-model.md` (ratings from there). The servers' handling was read from
their current sources: GNOME Shell `js/ui/messageList.js` and `js/misc/util.js` (`fixMarkup`, `findUrls`, `URLHighlighter`),
KDE Plasma `libnotificationmanager/notification.cpp` (`sanitize`) and `applets/notifications/components/{Summary,Body}.qml`,
dunst `src/notification.c` (`notification_format_message`, `notification_extract_urls`, `notification_open_url`), mako
`notification.c` (`format_text`) and `dbus/xdg.c` (`GetCapabilities`). `tracing-subscriber` 0.3.23 `src/fmt/format/mod.rs` and
`escape.rs` for the log format. An experiment compiled `clean_name` as it is and ran it on the characters named below.
Already known and not repeated: verification V-10 of `2026-11-15-review` (share names and owners cleaned of control characters
only; `%` log fields raw), except where M6 adds a new instance.

Actors: T2 (a device or app of the account: file and folder names, Photos names), T2s (the owner or an editor of a share the user
accepted: the share's name, its files' and folders' names, its renames, roles and revocation).

### C1-1: Names in a notification's body become links that GNOME Shell opens on a click and dunst on a middle click

- **Rating**: Low
- **Location**: `crates/pdmount/src/downloads.rs:148-157` (`one`: the file's name begins the body);
  `crates/pdmount/src/conflict_notify.rs:235-251` (`body`: tree and path on the first line); `crates/pdmount/src/notify.rs:409-415`
  (the body goes out as is, or escaped for `body-markup`). GNOME Shell sets every notification body through `URLHighlighter`
  (`messageList.js`: `new URLHighlighter('', true, this._useBodyMarkup)`), which runs `Util.findUrls` and, on a click, calls
  `Gio.app_info_launch_default_for_uri` with `http://` put in front of a URL without a scheme. `findUrls` matches, after the start
  of the text or a space, quote or bracket, `www\d{0,3}.` followed by any run without spaces, or `name.tld/…`. dunst collects URLs
  from summary and body (`notification_extract_urls`); a notification without actions opens a single URL in the browser at
  `do_action`, which is the middle click by default. Plasma and mako do not linkify plain text.
- **Reached by**: T2s for the download notification (any file in a share the user opens), T2 and T2s for conflicts.
- **Scenario**: An editor of a share uploads a 600 MiB video named `Storage full, confirm your plan at www.proton-plan.example`
  (no `/`, so the name is legal). The user opens it on a slow link; after 5 s GNOME Shell shows "PdMount: downloading" with the
  body "Storage full, confirm your plan at www.proton-plan.example (600 MiB) is downloading; …", the address underlined and
  clickable, in a notification under PdMount's own app name. A conflict path with a folder `a www.x.example` or
  `a x.example` followed by `/` gives the same. Escaping cannot help: the text is plain, and the server makes the link.
- **Impact**: A phishing link placed by another user, shown inside PdMount's own notification and opened by one click in the
  default browser. No file system effect.
- **Fix**: In names placed in a body, insert U+2060 WORD JOINER after every `.` and `:` (it breaks the `www.`, `name.tld/` and
  `scheme:` patterns of GNOME and dunst, and does not show), or name files only in the summary, which GNOME and Plasma show as
  plain text; add a test with a `www.` name.
- **Confidence**: high for GNOME Shell and dunst (sources read); not run on a desktop.

### C1-2: The unverified-signatures notification puts a share's name through `printable`, not `clean_name`

- **Rating**: Low
- **Location**: `crates/pdmount/src/signatures.rs:417-421` (`notification`: `place` from `crate::status::printable(name)`, which
  replaces only `char::is_control`, `crates/pdmount/src/status.rs:108-112`); reached with the share's shown name from
  `crates/pdmount/src/roots.rs:1784-1787` (`announce_tree`) and `crates/pdmount/src/roots/shared.rs:832`. DESIGN.md §14.4
  (line 2070) states that every name in a notification, a tree's or share's included, is cleaned by `notify::clean_name`.
- **Reached by**: T2s: the share's name, and nodes in it whose signatures do not verify (the owner can upload such nodes, or a
  member's key may simply be unknown).
- **Scenario**: A share named `Payroll\u{202E}fdp.exe` or `Team\u{2028}PdMount: sign in needed` produces the summary
  "PdMount: unverified signatures in …" with the text reordered, or split by a line separator that Pango renders as a new line
  (GNOME replaces only `\n` in a title). Every other summary that names a share (`incoming.rs:50-52`, `roots.rs:1210-1216`,
  `roots/shared.rs:971-979`) uses `clean_name`.
- **Impact**: Display spoofing in a summary under PdMount's name.
- **Fix**: `crate::notify::clean_name(name)` in `signatures::notification`; a test with U+202E and U+2028 next to the existing ones.
- **Confidence**: high.

### C1-3: `clean_name` lets invisible and blank characters and slash look-alikes through, a narrower rule than the device names'

- **Rating**: Low
- **Location**: `crates/pdmount/src/notify.rs:283-310` (`clean_name`: Cc, U+2028, U+2029, U+061C, U+200E, U+200F, U+202A–U+202E,
  U+2066–U+2069). The device-name rule `crates/pdmount-core/src/engine/marker.rs:213-224` (`is_hidden`) also removes U+00AD,
  U+180E, U+200B–U+200D, U+2060–U+206F, U+FEFF and U+FFF9–U+FFFB; V-10 asked for one widened rule for all names.
- **Reached by**: T2, T2s: every file, folder and share name in a notification.
- **Scenario**: Verified by running `clean_name`: U+200B, U+2060, U+FEFF, U+00AD, U+FFF9, U+206A, the tag characters
  (U+E0041), the Hangul fillers (U+115F, U+3164), U+2800 BRAILLE PATTERN BLANK and the slash look-alikes U+2215 and U+FF0F all
  pass unchanged. `report.pdf` followed by 40 × U+3164 and `.exe` shows as `report.pdf` with the extension pushed past GNOME's
  cut (150 graphemes in a title, 850 in a body) or the line's end; a share file named `Documents∕taxes∕2026.pdf` (U+2215) reads
  as a path in a slow-download body, which promises "the name, not the path". Combined with C1-1, a blank run can also push a
  fake sentence to a line of its own.
- **Impact**: Display spoofing of names in notifications; no file system effect.
- **Fix**: One function for every name shown (notifications, `status`, owner labels, device names): `is_hidden`'s set plus the
  tag block U+E0000–U+E007F, variation selectors, U+034F, U+115F, U+1160, U+3164, U+FFA0, U+2800 and U+180B–U+180D replaced by
  U+FFFD, and U+2044, U+2215, U+29F8, U+FF0F shown as U+FFFD or quoted; use it in `clean_name`.
- **Confidence**: high (experiment).

### C1-4: A renamed share keeps its old name and folder in its conflict notifications, so **Show** can open another share's file

- **Rating**: Low
- **Location**: `crates/pdmount/src/roots/shared.rs:823-832` (`Place::folder(&shown.name, …)` and `announce_tree(&roots,
  &shown.name, …)` once, when the share opens); `:634-639` (an owner's rename changes the slot's name in place, `set.rename`,
  and nothing tells the announcer); `crates/pdmount/src/conflict_notify.rs:109-116` (`Place::folder`: `root =
  mountpoint.join(name)`), `:138-144`, `:268-277` (`target` builds **Show**'s path from that root), `:244-250` (`body` names
  `label`). `display_names` (`roots/shared.rs:190-215`) gives the freed old name to the next share whose own name it is.
  `incoming::announce_in` and `signatures::announce` keep the old name too (`roots.rs:1780-1787`).
- **Reached by**: T2s: the owner renames the share; the same or another user owns a second share with the old name.
- **Scenario**: Share A is shown as `Team`; the user works in `~/Shared/Team/`. A's owner renames it to `Team-old` and shares a
  new folder B named `Team` (or renames another share to `Team`). The mount now shows A as `Team-old` and B as `Team`. The next
  conflict in A is announced "Team: /plan.odt …", and **Show** asks the file manager for `~/Shared/Team/plan (conflict …).odt`,
  a path in B, where B's owner has put a file of that name. The user, told the copy is there, opens and edits B's file and
  merges into it.
- **Impact**: The user is pointed at another user's file as their own conflict copy; their edits go into that share. Without
  a second share, **Show** opens a path that does not exist and the text names a folder that is gone.
- **Fix**: Keep the share's key in `Place` and resolve the label and root from the tree set when a notification is built (or
  re-`follow` with a new `Place` on rename); the same for the incoming and signatures announcers.
- **Confidence**: high for the code (traced: open, rename, naming order); the scenario assumes the user acts on **Show**.

### C1-5: The server's capabilities are asked once per connection, so a server that changes keeps the old markup decision

- **Rating**: Low
- **Location**: `crates/pdmount/src/notify.rs:349`, `:396-407` (`caps` is a `OnceCell` per connection), `:409-415` (the body is
  escaped only if that cached answer had `body-markup`). Nothing resets it when `org.freedesktop.Notifications` changes owner:
  `notify_watch.rs:122-129` sees `NameOwnerChanged` but only calls the conflict announcer.
- **Reached by**: T2/T2s write the names; the trigger is a server change on the same session bus: a different server takes the
  name (switching between mako, dunst or swaync in a Wayland session), or mako reloads with `markup` turned on (it answers
  `GetCapabilities` from its current configuration, `dbus/xdg.c`).
- **Scenario**: The daemon starts while mako runs with markup off (no `body-markup`); later the user enables markup, or starts
  dunst. Bodies now go out unescaped and are parsed as Pango markup: a file named
  `<span size="1">x</span>Your session expired.mp4` hides text or sets size and colour in a download or conflict notification.
  (Parsing errors make mako and GNOME fall back to escaping; dunst and mako render valid Pango markup.) The reverse change only
  shows `&amp;` literally.
- **Impact**: Display spoofing through markup in remote names; no links (Pango has none, dunst strips `<a>`).
- **Fix**: Ask `GetCapabilities` before each send (notifications are rare), or clear `caps` on `NameOwnerChanged` of the
  service.
- **Confidence**: high for the code; the trigger is uncommon.

### C1-6: **Show**'s fallback logs the conflict copy's folder with Display formatting (log injection)

- **Rating**: Low
- **Location**: `crates/pdmount/src/conflict_notify.rs:373-377` (`folder = %folder.display()`). In `tracing-subscriber` 0.3.23 a
  `%` field is written with its Display, unescaped; only the `message` field goes through `EscapeGuard`, and that escapes ANSI and
  C1, not newlines (`src/fmt/format/mod.rs:1299-1336`, `escape.rs`). A new instance of V-10's class, added in M6.2.
- **Reached by**: T2, T2s: the folder names on the copy's path (a remote folder may hold `\n` and ESC; `representable` filters
  only `/`, NUL, `.`, `..` and length).
- **Scenario**: A share editor creates a folder `x\n2026-11-20T10:00:00Z  WARN pdmount::auth: …` with a file in it; a conflict on
  that file is shown, the user clicks **Show** on a desktop without `FileManager1`, and the daemon log gets a forged line.
- **Impact**: Log injection (T2's list); misleads a user or a support reading of the log.
- **Fix**: `folder = ?folder` (Debug escapes), as `uri` next to it already is.
- **Confidence**: high.

### C1-7: Minor (Info)

- Length: no name is cut before it goes into a notification. A share name of up to 255 bytes in "PdMount: N changes to <name>
  kept", "… to <name> wait" or "changes on their way in <name> from <devices>" pushes the words after it out of GNOME's
  150-grapheme title; a deep conflict path pushes "what happened" out of the 850-grapheme body. Cut names (about 64 graphemes,
  an ellipsis in the middle so the extension stays) and put the fixed sentence before the name.
- `notify.conflict_names = false` removes names only from conflict notifications (as DESIGN.md and D139 define it; verified:
  the body is `what(kind)` alone, the summary and sums are fixed text, the M6.3 path passes the setting, `send_one` at
  `conflict_notify.rs:933`). Share names stay in the summaries of the orphan (`roots.rs:1210-1216`), held (`roots/shared.rs:971-979`),
  incoming (`incoming.rs:50-52`) and unverified-signatures notifications, and file names in downloads (by D137, documented in
  the user guide). D139 itself notes that "a share's name can say as much as a file's"; a lock-screen user who sets
  `conflict_names = false` still sees them. Either apply the setting to tree names in every notification, or say in the user
  guide which notifications it covers.
- A share can be named `My Files` (only `Shared files` is reserved in `display_names`, `roots/shared.rs:175-187`); its conflicts
  then read "My Files: /path", the same as My Files' own when the profile has further roots. Reserve the label, or prefix share
  labels ("Shared: Team").
- No hints are sent (`notify.rs:439`, empty map) and `app_icon` is empty, so no `image-path`, `x-kde-urls` or sound file comes
  from remote text; worth keeping so (Plasma would show any local image named in an `<img>` of a body it parsed as markup).

## Checked and fine (area C1)

- `escape_markup` (`notify.rs:269-281`) against the specification's subset (`b`, `i`, `u`, `a href`, `img`): with `&`, `<`, `>`
  escaped no tag, entity or numeric reference (`&#x202E;`) can be formed from a name; `"` and `'` matter only inside
  attributes, which cannot be opened. Cleaning comes before escaping (`clean_name` in the callers, escaping in `Entry::send`).
  GNOME `fixMarkup(…, true)` keeps the five entities and `b`/`i`/`u`; Plasma `sanitize` keeps `&amp;`/`&lt;`/`&gt;` and re-escapes
  text with `writeCharacters`; dunst and mako hand the body to Pango, which shows the entities as characters.
- The summary is never parsed as markup by the servers checked: GNOME `fixMarkup(text.replace(/\n/g, ' '), false)`; Plasma
  `Summary.qml` `textFormat: Text.PlainText`; dunst `%s` with `MARKUP_NO`; mako escapes every field it does not mark as markup.
  (xfce4-notifyd and swaync not checked.)
- Every remote name in a summary goes through `clean_name` except C1-2; bodies with names (conflicts, downloads) are cleaned;
  the incoming body carries only numbers and a range-checked time; the sign-in, keyring, shutdown and resume notices carry a
  profile name (lower-case ASCII, digits, `-`, `_`; `paths.rs:33-45`) and counts. NUL and other Cc become U+FFFD, so a name
  cannot make the D-Bus string invalid and drop the notification.
- No notification text is in a process's arguments any more (zbus instead of `notify-send`, D138); `xdg-open` gets only the
  copy's folder, an absolute path (no option injection), with the bus that passed the uid check.
- `file_uri` (`conflict_notify.rs:281-291`) percent-encodes every byte but `A-Z a-z 0-9 - . _ ~ /`.
- Logs: conflict `path` and `tree` (`conflict_notify.rs:577-590`), `uri` (`:370`), the slow download's `name`
  (`slow.rs:111`, debug level) are `&str`/`String` fields, written with Debug, which escapes `\n`, ESC, U+202E, U+200B and the
  like (checked in the experiment); the log never had notification text before M6 either (except V-10's and C1-6's `%` fields).
- Action keys and labels are constants (`show`/`Show`); `app_name` is `PdMount`; signals are passed on only for ids this
  process sent (`notify.rs:523-552`).
- `notify_stub.rs` is built only with `test` or `fake-backend` (`main.rs:23-24`).
