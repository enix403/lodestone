# lodestone

Manual, git-like bidirectional folder sync on top of `rclone bisync`, with every file kept
available offline.

No daemon, no watcher, no polling. You run a command when you have made changes, or when
you want to collect changes made elsewhere.

```console
$ lode status
silvermine  ↑ 1 outgoing  ↻ 5 renamed  − 1 deleted
  local  +  new-scan.pdf
  local  R  inbox/doc1.pdf -> archive/2024/doc1.pdf
  local  R  inbox/doc2.pdf -> archive/2024/doc2.pdf
  local  -  inbox/doc6.pdf
```

Moving a subtree costs nothing: moves are matched by content hash and executed as
server-side renames. Only deletes whose content vanishes entirely count against the
mass-deletion guard — so reorganising 300 files never trips a guard set at 10.

## Requirements

- **rclone ≥ 1.66** on `PATH`, with your remote already configured (`rclone config`).
  lodestone owns none of that — rclone keeps its config, its remotes and its OAuth tokens.
- macOS or Linux.

## Install

```sh
cargo install --path .
```

## Use

```sh
lode doctor                 # check the environment
lode doctor rename-test     # verify rename tracking on this machine, offline

lode add silvermine --local ~/silvermine --remote per-gdrive:Silvermine
lode init silvermine        # (only needed if the stanza is already in your config)
lode status                 # what would happen — changes nothing
lode push                   # send changes up (aborts if anything is coming down)
lode pull                   # bring changes down (aborts if anything is going up)
lode sync                   # both directions, no assertion
```

With no folder argument, every configured folder is planned first and the combined summary
printed before anything is touched. Add `--dry-run` to stop there.

Anything a sync would destroy on the local side — a file deleted on another machine, or a
local copy overwritten by an incoming edit — is moved to a timestamped local trash instead
of being lost:

```console
$ lode trash list
silvermine
  20260829T191500Z  (2026-08-29T19:15:00Z)
        4.9 K  inbox/doc2.pdf

$ lode trash restore silvermine inbox/doc2.pdf
```

Nothing is applied unless the plan is clean. Conflicts abort (exit 10), an unexpected mass
deletion aborts (exit 11, overridable per-run with `--allow-deletes N`), and a violated
directional assertion aborts (exit 12).

`~/.config/lode/config.toml`:

```toml
[folder.silvermine]
local  = "~/silvermine"
remote = "per-gdrive:Silvermine"
```

`lode add` writes this stanza for you, preserving any comments and formatting already in
the file, and stores paths under `$HOME` as `~/...` so one config works on both macOS and
Linux. `lode forget <name>` reverses it — config and local state only, never your files.

Machine-specific overrides go in a gitignored `config.local.toml`, so the main file can
live in a dotfiles repo.

OS junk (`.DS_Store`, `._*`, `Thumbs.db`, `.directory`, …) is excluded by a built-in,
non-configurable rule set, so browsing a folder in Finder never pollutes the remote. It is
deliberately not configurable: bisync demands a full re-baseline whenever its filter set
changes, so a per-machine list would force a resync every time you switched machines.

`lode doctor` checks the things that break a mixed macOS/Linux fleet silently: filenames
differing only in case or in Unicode normalisation, duplicate names on the remote, symlinks
rclone will skip, and whether your filesystem is case-insensitive. Collisions abort a sync
rather than warn — there is no safe automatic resolution for them.

## Documentation

- [`docs/TDD.md`](docs/TDD.md) — design, rationale, rejected alternatives, limitations
- [`docs/PLAN.md`](docs/PLAN.md) — implementation roadmap and current status

## Status

Usable. The plan engine and the apply phase both work end to end, covered by 125 tests
including 43 that drive the real binary against real rclone with no network.

Still to come: run history and an advisory lock — see [the plan](docs/PLAN.md).
