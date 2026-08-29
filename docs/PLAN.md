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

## Step 2 — The apply phase: `sync`, `push`, `pull` ✅ DONE

The other half of two-phase execution, and the first code that mutates data.

Delivered:

- `session` — binds rclone and machine identity to `plan()` and `apply()`, so the preview a
  user sees and the gate on the mutation are the same code.
- `sync` / `push` / `pull`, differing only in the asserted direction; `--dry-run` and
  `--allow-deletes N` on all three.
- Snapshot rewritten from a post-sync listing **only** on success.
- Plan-all-then-apply fan-out: the combined summary is printed before anything mutates, a
  clean folder still syncs when a sibling is blocked, and the run exits with the most
  specific failure code.
- Expected bisync failures mapped to actionable errors instead of raw rclone text: stale
  lock, workdir filename too long, and the empty-side refusal.
- `lode unlock` (pulled forward from the advisory-lock step — it is what makes the
  stale-lock error
  actionable rather than a dead end).
- 11 new e2e tests covering reorg-as-moves, idempotence, both directional assertions,
  dry-run, the delete guard and its override, conflict abort, fan-out isolation, and
  unlock.

**Finding:** rclone refuses to sync when one side's listing is empty, and neither `--force`
nor `--allow-deletes` lifts it. A folder therefore cannot be emptied through the normal
commands at all — a hard floor beneath both guards. See TDD §6.7.

**Deliberately deferred from this step:** SIGINT forwarding. An interrupted run leaves the
previous snapshot intact and the next plan simply sees the partially-applied state as
ordinary changes, so the design is already self-healing here; the remaining gap is bisync's
own lock file, which `lode unlock` now handles.

## Step 3 — Filters ✅ DONE

Taken out of order deliberately. Introducing filters later changes bisync's filter
fingerprint and forces a `--resync` on every folder on every machine, so the cost grows
with every folder initialised. Doing it while there is essentially one folder costs
nothing. On a mixed macOS/Linux fleet `.DS_Store` pollution is a certainty, not a risk.

Delivered:

- `filters` — 12 compiled-in exclusions covering macOS, Linux desktop and Windows junk,
  with an FNV-1a fingerprint over the rendered rules (TDD §9.1).
- Applied to **both** sides of the tool: `--filter-from` on `lsjson` and `--filters-file`
  on bisync, so the plan and the sync can never disagree about which files exist.
- Fingerprint recorded in every snapshot; when bisync demands a resync, lodestone compares
  and reports that the built-in filter set changed rather than passing rclone's demand
  through.
- `lode doctor` reports the active rule count and fingerprint.
- 5 unit + 3 e2e tests.

**Bug found and fixed:** `lode init` talked to rclone directly rather than through
`Session`, so it baselined *unfiltered* while every later run was filtered — making bisync
demand a resync on the very next command, for every folder. `init` now goes through
`Session` like everything else. The end-to-end tests caught this; no unit test would have.

## Step 4 — `add` and `forget` ✅ DONE

Brought forward: this is the onboarding story, and onboarding a second machine is the
nearest real milestone.

Delivered:

- `configfile` — format-preserving TOML editing via `toml_edit`, so comments, alignment and
  key order in the dotfiles-tracked config survive an edit. Written atomically.
- `lode add <name> --local P --remote R [--max-deletes N] [--no-init]` — validates the name
  and that the remote resolves in `rclone.conf` **before** writing anything, then writes
  the stanza and establishes the baseline in the same command.
- Paths under `$HOME` are stored as `~/...`, so the shared config stays portable between
  macOS and Linux without a per-machine override.
- `lode forget <name> [--keep-state]` — removes the config stanza and lodestone's local
  state, and states explicitly that no files were deleted on either side.
- 8 unit + 6 e2e tests.

Onboarding a machine is now: install rclone → `rclone config` once → symlink dotfiles →
`lode add` (or `lode init` if the stanza is already in the shared config).

## Step 5 — Local trash ✅ DONE

The last real gap in the safety story. A genuine delete that passes the guard was only
recoverable from Drive's trash, and only on the remote side.

Delivered:

- `timestamp` — the calendar arithmetic the rest of the tool needed anyway (snapshot
  stamps, run directory names, run ages), extracted from `snapshot` and given a proper
  inverse so run names can be parsed back for pruning.
- `trash` — `--backup-dir1` into `$XDG_STATE_HOME/lode/trash/<folder>/<run>/`, catching
  both locally-deleted and locally-overwritten files, with the relative path preserved.
- Runs that caught nothing are removed immediately, so `trash list` never fills with empty
  directories; the apply summary reports the run when it did catch something.
- `lode trash list|restore|prune`. `restore` copies rather than moves, defaults to the most
  recent copy, refuses to clobber without `--overwrite`, and says plainly that the restored
  file is now a local change. `prune` defaults to 30 days and needs `--all` to take
  everything.
- 14 unit + 5 e2e tests.

**Verified before building:** `--backup-dir1` does move locally-destroyed files into the
backup directory with their relative path intact, as a server-side move.

## Step 6 — Cross-platform name hazards ✅ DONE

Brought forward: these are the silent-corruption classes, and they go live the moment a
Linux machine shares a folder with the Mac.

Delivered:

- `hazards` — name-collision detection, symlink discovery, and a case-sensitivity probe.
- Collisions are refused in the **plan phase**, over the union of both sides' listings: a
  name written as NFC on Linux and NFD on macOS looks wrong on neither side alone.
- Duplicate remote filenames are detected in `lsjson` itself. Collecting into a map would
  have silently dropped one and hidden the problem; the adapter now counts and refuses,
  pointing at `rclone dedupe`.
- `doctor` reports symlinks (found without following them, so dangling links still show)
  and probes whether the folder's filesystem is case-insensitive rather than guessing from
  the platform.
- 12 unit + 4 e2e tests.

**Design flaw caught by a test.** The first version ran two passes — normalisation, then
case. But the case pass normalises *before* folding, making it a strict superset, so the
normalisation pass was unreachable dead code. Replaced with a single pass that labels each
group by cause.

**Note on coverage:** the two collision e2e tests detect that macOS APFS is case- and
normalisation-insensitive and skip, because the collision cannot even be staged there. They
exercise fully on Linux. The gate itself is covered unconditionally by unit tests over
synthetic listings.

## Interlude — first real run against Google Drive ✅ DONE

`~/silvermine` (37 files, 33 MB) adopted on macOS. Both sides were already identical, so
the baseline was a verified no-op; `rclone check` reported 0 differences before and after.

A throwaway scratch folder on the same Drive account then exercised the paths that local
directory pairs cannot prove:

- reorganising 12 files produced **12 server-side moves and 0 bytes transferred** — the
  `--track-renames` finding holds on Drive, not just locally
- a delete made on Drive propagated on `pull` and was captured in the local trash
- `trash restore` followed by `push` recovered the file and re-uploaded it

**Three bugs found by doing this, all fixed:**

1. `init` failed against a remote path that did not exist yet — bisync aborts with
   `directory not found` rather than creating it. `init` now runs `rclone mkdir` first.
   This is the ordinary case for adding a *new* folder, so it would have hit immediately.
2. `--track-renames` on a resync makes rclone log an ERROR and inflates its error count,
   because resync copies rather than syncs. Omitted for that invocation.
3. `forget` silently orphaned the folder's trash, which can hold the only copy of a file
   deleted elsewhere. It now reports it and offers `--purge-trash`.

A fourth, cosmetic, was found while writing the config: the generated header landed at the
bottom of a new file.

## Step 7 — lodestone's own advisory lock

Prevents two terminals racing before rclone is ever invoked.

- Per-folder lock under `XDG_STATE_HOME` holding pid + hostname + start time.
- Stale detection (pid gone → offer to clear), folded into the existing `lode unlock`.
- Forward SIGINT to rclone so bisync can journal, rather than dying at SIGKILL.

## Step 8 — Run history and logging

Interactive-first, since there is no daemon whose logs you would read after the fact.

- Clean summary in the foreground; raw rclone output to
  `$XDG_STATE_HOME/lode/logs/<folder>/<ts>.log`, surfaced on `-v` or on failure.
- A run log (timestamp, command, plan counts, outcome, duration, exit code) behind
  `lode log` / `lode log --show <id>`. Rotate by count and age.
- `lode diff <folder>` — the verbose per-file plan.

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
