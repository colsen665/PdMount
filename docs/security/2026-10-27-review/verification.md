# Verification of the M4.0.1 area reports (Fable verifier, 2026-10-27)

Reviewed tree: worktree `e55ed49` (`m4-start`), SDK fork `pdmount` at `d1c8994`. Threat model:
`docs/security/threat-model.md` (D070). Area reports: `area1.md` … `area5.md` in this directory (finding IDs A1-n … A5-n).
Fixes and their specifications: `docs/plans/m4.md` "M4.0.1 outcomes". Open Lows and Infos: TODO.md Known gaps,
"Security (M4.0.1)".

| Group | Members | Title | Verdict | Final rating | Fixed by |
|---|---|---|---|---|---|
| S-1 | A1-1 | SDK fork logs the key passphrase (`rs2/account.rs:226`), unlocked address keys (`rs2/api/cache.rs:249`) and the `/users` body (`rs2/users.rs:83`) at debug; filter covers only `proton_sdk_rs2::secret`; `-vv`/`RUST_LOG` (inherited by the daemon) reach it | CONFIRMED | High | M4.0.2.1 |
| S-2 | A1-2 | Sign-in link (user code + fork key) logged at WARN and in `notify-send` argv; selector stays in memory | CONFIRMED | Medium (from High) | M4.0.2.1 |
| S-3 | A1-3, A3-3, A4-6 | Log file and state dir created with umask modes (`daemon.rs:149-157`) | CONFIRMED | Medium (High with S-1/S-2) | M4.0.2.1 |
| S-4 | A5-1, A4-2 | Manifest signature never verified on block-map reads; DESIGN §6.3 claims it | CONFIRMED (gap) | Medium | M4.0.2.2 |
| S-5 | A3-1 | Remote claimed size drives `set_len` and an unbounded `missing_blocks()` Vec | CONFIRMED | Medium | M4.0.2.3 |
| S-6 | A3-2, A4-1 | Claimed per-block `plain_size` used as allocation size (calloc, virtual) | CONFIRMED | Low (from Medium) | M4.0.2.3 |
| S-7 | A5-5 | `rm -rf "$DATASET"` (and `FAKE_DIR`, bench scripts) | CONFIRMED | Medium | M4.0.2.4 |
| S-8 | A5-6 | `/dev/shm/pdmount-*` root without an ownership check; git's ownership check blocks the planted-repo exec | PLAUSIBLE | Low (from Medium) | M4.0.2.4 |
| S-9 | A2-4 | `apply_listing_at` `expect("counted above")` with a stamped node under a vanished folder; 29355a6 covers followers only | CONFIRMED (traced) | Low | Known gaps |
| S-10 | A5-2, A4-3 | `Retry-After` unbounded; 429 retries unbounded per request | CONFIRMED | Low | Known gaps |
| S-11 | A5-3, A4-5 | Empty listed block hash skips the per-block check | CONFIRMED | Low | M4.0.2.2 |
| S-12 | A2-1, A4-4 | Remote mode keeps setuid/setgid/sticky bits (mount is `nosuid`) | CONFIRMED | Low | Known gaps |
| S-13 | A1-6, A3-7 | Data dir and some files created with umask modes before `create_private_dir` | CONFIRMED | Low | Known gaps |
| S-14 | A1-4 | `unmount --force` acts on any mount at the path | CONFIRMED | Low | Known gaps |
| S-15 | A1-5 | Pid check compares only argv[0]'s basename | CONFIRMED | Low | Known gaps |
| S-16 | A3-4 | Config durations near `u64::MAX` panic the event/journal tasks | CONFIRMED | Low | Known gaps |
| S-17 | A2-5, A3-5 | Remote names reach logs and CLI unescaped | CONFIRMED | Low/Info | Known gaps |
| S-18 | A2-2 | Huge remote `/.pdignore` slows the hot path | CONFIRMED | Low | Known gaps |
| S-19 | A2-3 | Remote `/.pdignore` silently makes new files local-only (D018) | CONFIRMED | Info | Known gaps |
| S-20 | A2-6 | Kernel caching limits D031 checks; `O_RDONLY\|O_TRUNC` (not a bypass) | PLAUSIBLE | Info | Known gaps |
| S-21 | A1-7 | Last-component-only chmod, unbounded control `read_line`, `.tmp` reuse | CONFIRMED | Info | Known gaps |
| S-22 | A3-6 | `BlockKey::encode` not injective with NUL ids | CONFIRMED | Info | Known gaps |
| S-23 | A4-7 | Fake backend module in every build, gated at run time by `cfg!` | CONFIRMED | Info | Known gaps |
| S-24 | A4-8 | Empty name hash becomes an empty `OriginalHash` | PLAUSIBLE | Info | Known gaps |
| S-25 | A5-4 | 5xx retry replays an applied `OriginalHash` rename/move → false conflict | PLAUSIBLE | Low | Known gaps |
| S-26 | A5-7 | `$dir` inside `sh -c` strings (a1–a3) | CONFIRMED | Info | Known gaps |
| — | A2 note | u64 sum of claimed block sizes can wrap | REJECTED (unreachable) | — | `saturating_add` in M4.0.2.3 anyway |

**Verifier's overall assessment (abridged).** The core holds against other local users and untrusted remote content:
remote names never reach local paths, the control socket is 0700/0600 with a peer-uid check, secret files are 0600,
TLS validation is on, panics are contained, and the per-block hash and plain-size checks are real. The one High is a
logging gap (S-1) made worse by the log's file mode (S-3). The other Mediums are a documentation/integrity mismatch
(S-4), one remote-input DoS (S-5) and a test-script footgun (S-7).
