# Multi-task runs: implementing several plan tasks with agents

How an orchestrating agent handles a request that covers more than one task of a milestone plan, for example
"implement M5", "M6 until M6.3", "M5 except M5.9–M5.14" or "M5.2 and M5.7". Written from the M5 agent session of
2026-11-12 (D098). Status stays in `TODO.md`; task specifications in `docs/plans/m<n>.md`; this file only describes the
procedure.

## 1. Resolve the scope

Read `TODO.md`'s task checklist for the milestone and the plan's task table (Model, Depends on) and the `[full]` marks.

| Request | Tasks |
|---|---|
| "Mn" | every open task of Mn |
| "Mn until Mn.m" | the open tasks up to and including Mn.m, in the plan table's order |
| "Mn except Mn.m[, …]" | every open task but those named; a range "Mn.a–Mn.b" excludes the tasks between them in table order |
| "Mn.a, Mn.b" / "Mn.a–Mn.b" | those tasks |

Leave out, and report as not started with the reason:
- tasks the user runs (manual runs, such as M5.6);
- tasks that need something from the user first: real-account data or setup, an exception to D028, consent for a long real-account
  run (D030);
- the milestone's exit task (the plan's "last" row), unless it is named;
- tasks whose dependencies are left out, unless the dependency is already done.

Ask the user only if the request is still ambiguous after this, and say which tasks were left out before starting.

## 2. Waves

- Order the tasks by "Depends on". A wave is the set of tasks whose dependencies are all merged; start the next wave's
  tasks as soon as their own dependencies are merged, not when the whole wave is.
- At most three agents at once. More slow each other's builds and make timing-sensitive tests flaky.
- Tell each agent which tasks run in parallel and which files they touch, so that it keeps shared code (the daemon's
  start and stop, `main.rs`, the scenario lists) to small, localized changes.
- Pre-assign decision numbers (the next free D numbers, one per task that may need one), so parallel agents do not
  collide.
- Two small tasks that build on each other (such as M5.4 and M5.5) can go to one agent as two commits.

## 3. The agent brief

Agents run with `isolation: worktree` and the model of the plan's Model column. The brief is all they know; it contains:

1. **Base check:** `git merge --ff-only <integration branch>` (or the main worktree's branch for the first wave), and the
   `git log --oneline` it must then show. A new worktree may start from an older commit; the check catches it.
2. **What to read:** `AGENTS.md`, `TODO.md` Handoff notes, the decisions the task names, the task's section of the plan,
   the plan's ground rules (for M4 onwards: `docs/plans/m4.md`, "Ground rules for all tasks"), the `DESIGN.md` sections.
3. **The task:** a pointer to the plan section, plus what earlier tasks provide (options, helpers, file names) and any
   facts from earlier results the task depends on.
4. **Rules:** fake backend only, never `login` or the real account; no push, no tags; never bare `git stash`; the
   public-repository rules (no local paths, user names, machine details or key names); **do not edit `TODO.md` or
   `docs/handoff-log.md`** (the orchestrator does, so these never conflict); update `DESIGN.md` and
   `docs/decisions.md` for what changes; the pre-assigned decision number, status `proposed`, by `claude`.
5. **CI:** `scripts/ci.sh --quick`, plus the scenarios the task adds or changes and the neighbouring ones it could affect
   (`BACKEND=fake`, after `cargo build -p pdmount --features fake-backend`), with its own `SCENARIO_ROOT` when it runs
   scenario scripts directly. `scripts/ci.sh` picks a root per checkout by itself (D098).
6. **At the end:** unmount its test daemons, stop its background loops, then one commit `Mn.k: <title>` ending with the
   model's `Co-Authored-By` trailer.
7. **Report:** branch and SHA; what changed; tests with the exact commands and results; deviations and open questions; a
   short handoff note; what later tasks or the user must do.

## 4. Integration

- Keep an integration branch in a scratch worktree under the session's scratchpad, so that a CI run in the main worktree
  is not disturbed by checkouts.
- Cherry-pick the agents' commits in dependency order. Resolve conflicts: scenario lists and similar lists are unions;
  the decision index stays in numeric order, entries likewise; two descriptions of one mechanism in `DESIGN.md` become
  one. After resolving: `cargo fmt --all -- --check` and both clippy runs.
- Amend each task's commit with its `TODO.md` checkbox and its entry at the top of `docs/handoff-log.md`, so that the
  one-commit-per-task rule holds.
- Read the code of every merged task once, aimed at what the agent's report does not show (lock order, exit paths,
  external input).

## 5. CI

- Agents run the quick CI. The orchestrator runs the full `scripts/ci.sh` on the integration tip once per wave or
  batch of waves, after fast-forwarding the main worktree to it, as D084 requires for `[full]` tasks and before anything
  is pushed. One full run covers every task merged so far; per-task full runs in the agents would cost about 35 minutes
  each and the merged result still needs its own run.
- `scripts/ci.sh` runs every step and reports all failures at the end (D098), so a failing unit test no longer hides the
  scenarios; use `--fail-fast` only for quick loops.
- Concurrent runs in different worktrees are safe (one `SCENARIO_ROOT` per checkout, locked), but load makes
  timing-sensitive tests flaky. A failure in a test the task did not touch: rerun it once. If it fails again or recurs,
  give it to a fix agent that reproduces it under load and fixes the cause; never loosen the assertion. Record what was
  seen in `TODO.md` Known gaps until it is fixed.

## 6. Cross-task review

After the last wave, one read-only reviewer (Opus) reads the whole diff of the run. Its brief names the places where
separately written tasks meet: start and stop paths, supervisors and signals, notifications, locks and `.await`, config
defaults and documentation consistency. Fix agents take its findings, grouped by file area so they do not conflict, each
one commit (`Mn review: <what>`). Then the final full CI.

## 7. Finish

- Merge into `master` (fast-forward) only when the user asked for it and the final full CI passed. Never push without the
  user's agreement; before a push the publishing rules and D084 apply.
- Update `TODO.md`: Last updated, Next action, Current state, Known gaps, the checkboxes; add the session's entry at the
  top of `docs/handoff-log.md`.
- Report to the user: tasks done, decisions `proposed` that need their review (and any `accepted` decision whose status
  changed), known gaps, tasks not started and why, what the user has to do next.

## 8. Hygiene

- Stop test daemons left by agents: `fuse.pdmount` mounts under their scenario roots (`grep fuse.pdmount /proc/mounts`),
  with SIGTERM to the daemon.
- An agent that reports but stays "waiting on its own background work" keeps a task open; stop it once its report is
  in. A `while pgrep -f <pattern>` loop matches its own command line and never ends.
- After merging, remove the agents' worktrees and branches, after checking that each is clean (`git status
  --porcelain`) and its commit is on the integration branch (cherry-picked commits have new hashes). Remove the
  integration worktree and branch.
- Remove `/dev/shm/pdmount-ci-*` (and the `.lock` files) of removed worktrees when nothing is mounted under them.
- Leave the user's untracked files alone.
