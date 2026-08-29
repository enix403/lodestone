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
lode init silvermine        # establish the baseline
lode status                 # what would happen — changes nothing
```

`~/.config/lode/config.toml`:

```toml
[folder.silvermine]
local  = "~/silvermine"
remote = "per-gdrive:Silvermine"
```

Machine-specific overrides go in a gitignored `config.local.toml`, so the main file can
live in a dotfiles repo.

## Documentation

- [`docs/TDD.md`](docs/TDD.md) — design, rationale, rejected alternatives, limitations
- [`docs/PLAN.md`](docs/PLAN.md) — implementation roadmap and current status

## Status

Early. The plan engine, `init`, `status`, `folders` and `doctor` work end to end. The apply
phase (`sync`/`push`/`pull`) is next — see the plan.
