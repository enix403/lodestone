# lodestone — implementation plan

Ordered so that each step leaves the tool usable and each builds on proven ground. See
`docs/TDD.md` for the design each step implements.

---

## Step 1 — Foundation and the plan engine ✅ DONE

**Why first:** the plan engine is the whole design. Everything else is plumbing around it,
and it is the part that must be right before anything is allowed to mutate a file. It is
also pure logic, so it can be tested exhaustively without a cloud account.

Delivered:

- `config` — two-layer TOML merge, field-by-field local override, validation, refusal when
  the state directory would live inside a synced folder.
- `paths` — XDG on both platforms, `~` expansion, lexical containment check.
- `machine` — persistent machine id; foreign snapshots refused.
- `snapshot` — the merge base; atomic write; tri-state content comparison that never
  treats "unknown" as "equal".
- `rclone` — binary discovery, version parsing and hard version floor, `listremotes`,
  `lsjson --hash-type md5`, bisync argument construction, workdir filename-length
  projection.
- `plan` — three-way delta, symmetric hash-based rename matching, conflict detection,
  delete guard, directional assertions, summary rendering.
- Commands: `init`, `status` (text and `--json`), `folders`, `doctor`,
  `doctor rename-test`.
- 35 unit tests + 6 end-to-end tests driving the real binary against real rclone with two
  local directories (no network, no cloud account).

**Two findings that changed the design, both empirical:**

1. `--track-renames` works in bisync (12/12 server-side moves, zero transfers) **but is
   unreachable without `--force`**, because bisync's percentage delete guard fires during
   delta detection, before the sync stage. lodestone therefore always passes `--force` and
   substitutes its own guard. See TDD §6.1.
2. bisync encodes both full paths into one workdir filename; deep paths breach the
   255-byte limit and fail with `file name too long`. Now a `doctor` check. See TDD §9.

---

## Step 2 — The apply phase: `sync`, `push`, `pull`

The other half of two-phase execution, and the first code that mutates data.

- Run bisync with the flags in `Rclone::bisync_base_args` after the plan passes its gates.
- Rewrite the snapshot from a post-sync listing, **only** on success.
- `--allow-deletes N` and `--dry-run` on all three.
- Ctrl-C: forward SIGINT, let bisync journal, mark the snapshot stale so the next run
  rescans rather than trusting a half-applied state.
- Fan-out: plan every folder, print one combined summary, then apply only the clean ones;
  isolate failures; exit with the most specific code.
- e2e tests: reorganise → `push` → assert server-side moves and a correct new snapshot;
  remote change → `push` refuses with exit 12; conflict → nothing is written.

## Step 3 — Locking and crash safety

Prevents two terminals racing, and makes interruption recoverable.

- Per-folder advisory lock under `XDG_STATE_HOME` holding pid + hostname + start time.
- Stale detection (pid gone → offer to clear); `lode unlock`.
- Snapshot "stale" flag honoured by the plan phase.

## Step 4 — Local trash

Deletions must be recoverable without going to Drive's web UI.

- `--backup-dir1` into `$XDG_STATE_HOME/lode/trash/<folder>/<timestamp>/`.
- `lode trash list|restore|prune`. No `--backup-dir2` (TDD §6.4).

## Step 5 — `add` and `forget`

Completes onboarding to a single command.

- `lode add <name> --local P --remote R` — validate the remote resolves, write the stanza
  with `toml_edit` so comments and ordering in the dotfiles-tracked file survive, then
  `init`. `--no-init` to only write config.
- `lode forget <name>` — remove config stanza and local state; never touch files; say so
  explicitly; `--purge-state` for the trash directory.

## Step 6 — Run history and logging

Interactive-first, since there is no daemon whose logs you would read after the fact.

- Clean summary in the foreground; raw rclone output to
  `$XDG_STATE_HOME/lode/logs/<folder>/<ts>.log`, surfaced on `-v` or on failure.
- A run log (timestamp, command, plan counts, outcome, duration, exit code) behind
  `lode log` / `lode log --show <id>`. Rotate by count and age.
- `lode diff <folder>` — the verbose per-file plan.

## Step 7 — Filters

- Compiled-in OS-junk list (TDD §9.1), applied via `--filter-from`.
- Fingerprint the filter set into the snapshot so a change explains *why* bisync demands a
  resync instead of letting rclone say it cryptically.

## Step 8 — Cross-platform `doctor` checks

Cheap, and they catch silent corruption classes.

- NFC/NFD collisions; case-only collisions; duplicate remote filenames; symlink report.
- Each aborts rather than warns — there is no safe automatic resolution.

## Step 9 — Multi-machine hardening

Only meaningful once a second machine is live.

- Verify a reorganisation on machine A is absorbed cheaply by machine B (TDD §5.2). This is
  covered by unit tests today; it needs a real two-machine run against Google Drive.
- Optionally use Drive's stable file IDs (`lsjson`'s `ID` field) to raise remote rename
  confidence above hash matching alone.

## Step 10 — Distribution

Deferred until wanted: static musl Linux builds plus both macOS arches on GitHub Releases,
and an `install.sh` for the dotfiles repo (TDD §11).

---

## Backlog / not scheduled

- `RcEngine` — drive a persistent `rclone rcd` over the rc API (TDD §3.3).
- Inode-keyed local index as an extra rename signal, if copy-then-delete file managers
  turn out to matter in practice (TDD §5.2).
- Additional backends beyond Google Drive — mostly a matter of relaxing the md5 assumption.
