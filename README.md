# PdMount

A FUSE filesystem for Linux that mounts Proton Drive "My Files" read-write. It is written in Rust on
[fuser](https://crates.io/crates/fuser) and the community Proton Drive SDK
([proton-sdk-rs2](https://github.com/tirbofish/proton-sdk-rs2)).

> This is a third-party application, not officially supported by Proton.

**Status:** see [TODO.md](TODO.md).

| Document | Content |
|---|---|
| [docs/user-guide.md](docs/user-guide.md) | Installing and using PdMount: mount, autostart, access rules, conflicts, queue, cache, troubleshooting |
| [TODO.md](TODO.md) | Current status, next action, open questions |
| [AGENTS.md](AGENTS.md) | Instructions for agents working in this repository (CLAUDE.md refers to it) |
| [docs/decisions.md](docs/decisions.md) | Decision log |
| [DESIGN.md](DESIGN.md) | Architecture: concurrency, read/write paths, sync, conflicts, access control, testing, milestones |
| [docs/pdcli-evaluation.md](docs/pdcli-evaluation.md) | Evaluation of the existing pdcli mount and why PdMount is a separate project |
| [docs/sdk-patches.md](docs/sdk-patches.md) | Patches to the community SDK, carried in [a fork](https://github.com/colsen665/proton-sdk-rs2) |

Licence: MIT ([LICENSE](LICENSE)).

Requirements: DESIGN.md §1.1. Requirement decisions: D001–D010 in docs/decisions.md.
